//! Orders the first external provider operation against scheduler sleep/clock events.
//!
//! The scheduled engine future owns this scope. Its preparation and compaction
//! futures are awaited inline; tasks spawned after provider launch are already
//! part of a committed active attempt. Manual turns do not enter this scope.
//! Tokio tasks do not inherit task locals: any future pre-dispatch provider work
//! moved into a spawned task must explicitly carry and scope its lease.
use std::future::Future;

use crate::agents::scheduler::DispatchLease;
use crate::assistant::providers::types::ProviderError;

tokio::task_local! {
    static SCHEDULED_DISPATCH: Option<DispatchLease>;
}

pub(crate) async fn with_scheduled_dispatch<T>(
    lease: Option<DispatchLease>,
    future: impl Future<Output = T>,
) -> T {
    SCHEDULED_DISPATCH.scope(lease, future).await
}

/// The logical start of the attempt, immediately before its first external
/// provider operation. The wake gate is released before any send/spawn/await.
/// This is a commitment to start, not acknowledgment of network transmission.
pub(crate) fn commit_provider_dispatch() -> Result<(), ProviderError> {
    match SCHEDULED_DISPATCH.try_with(|lease| lease.as_ref().is_none_or(DispatchLease::try_commit))
    {
        Ok(false) => Err(ProviderError::ScheduledDispatchAbandoned),
        // Unscoped work is manual or a legacy schedule.
        Ok(true) | Err(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::scheduler::WakeSignal;
    use crate::assistant::auth::secrets::ProviderSecretStorage;
    use crate::assistant::providers;
    use crate::assistant::types::{AuthMode, CompletionRequest, ProviderConnection};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    fn connection(protocol: &str, base: String) -> ProviderConnection {
        ProviderConnection {
            id: "dispatch-test".into(),
            name: "dispatch-test".into(),
            protocol_id: protocol.into(),
            provider_id: protocol.into(),
            auth_mode: AuthMode::DeveloperApiKey,
            base_url: Some(base),
            secret_ref: "dispatch-test-secret".into(),
            model_id: "test".into(),
            account_label: None,
            enabled: true,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn request() -> CompletionRequest {
        CompletionRequest {
            run_id: "run".into(),
            session_id: "session".into(),
            model_id: "test".into(),
            messages: Vec::new(),
            tools: Vec::new(),
            temperature: None,
            max_output_tokens: None,
            images: Default::default(),
        }
    }

    #[tokio::test]
    async fn sleep_during_async_preparation_abandons_but_commitment_can_finish() {
        let signal = WakeSignal::default();
        let lease = signal.dispatch_lease().unwrap();
        with_scheduled_dispatch(Some(lease), async {
            tokio::task::yield_now().await;
            signal.will_sleep();
            signal.did_wake();
            assert!(matches!(
                commit_provider_dispatch(),
                Err(ProviderError::ScheduledDispatchAbandoned)
            ));
        })
        .await;
        let lease = signal.dispatch_lease().unwrap();
        with_scheduled_dispatch(Some(lease), async {
            commit_provider_dispatch().unwrap();
            signal.will_sleep();
            tokio::task::yield_now().await;
            commit_provider_dispatch().unwrap();
        })
        .await;
        // Manual work outside the scheduled scope remains allowed during sleep.
        commit_provider_dispatch().unwrap();
    }

    #[tokio::test]
    async fn sleeping_claim_never_sends_api_turn_or_compaction_request() {
        use axum::{routing::post, Router};
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let router = Router::new().fallback(post(move || {
            seen.fetch_add(1, Ordering::Relaxed);
            async { axum::http::StatusCode::BAD_REQUEST }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        ProviderSecretStorage::set_secret("dispatch-test-secret", "test-key").unwrap();
        for protocol in ["openai", "anthropic", "openrouter"] {
            let mut connection = connection(
                if protocol == "openrouter" {
                    "openai"
                } else {
                    protocol
                },
                base.clone(),
            );
            connection.provider_id = protocol.into();
            let adapter = providers::resolve_connection_adapter(&connection).unwrap();
            for sessionless in [false, true] {
                let signal = WakeSignal::default();
                let lease = signal.dispatch_lease().unwrap();
                let sleep = signal.clone();
                ProviderSecretStorage::before_test_read("dispatch-test-secret", move || {
                    sleep.will_sleep();
                    sleep.did_wake();
                });
                let result = with_scheduled_dispatch(Some(lease), async {
                    if sessionless {
                        adapter
                            .stream_sessionless_completion(&connection, request(), None)
                            .await
                    } else {
                        adapter.stream_completion(&connection, request()).await
                    }
                })
                .await;
                assert!(
                    matches!(result, Err(ProviderError::ScheduledDispatchAbandoned)),
                    "{protocol}, sessionless={sessionless}"
                );
            }
            assert_eq!(calls.load(Ordering::Relaxed), 0);
        }
        // The same adapters still issue ordinary manual requests outside the scope.
        for protocol in ["openai", "anthropic", "openrouter"] {
            let mut connection = connection(
                if protocol == "openrouter" {
                    "openai"
                } else {
                    protocol
                },
                base.clone(),
            );
            connection.provider_id = protocol.into();
            let adapter = providers::resolve_connection_adapter(&connection).unwrap();
            assert!(matches!(
                adapter.stream_completion(&connection, request()).await,
                Err(ProviderError::RequestFailed(_))
            ));
        }
        assert_eq!(calls.load(Ordering::Relaxed), 3);
        server.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sleeping_claim_never_launches_any_cli_compactor() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("launched");
        let binary = tmp.path().join("fake-provider");
        std::fs::write(
            &binary,
            format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        for protocol in ["claude-code", "codex", "opencode"] {
            let connection = connection(protocol, binary.to_string_lossy().into_owned());
            let adapter = providers::resolve_connection_adapter(&connection).unwrap();
            let signal = WakeSignal::default();
            let lease = signal.dispatch_lease().unwrap();
            signal.will_sleep();
            let result = with_scheduled_dispatch(
                Some(lease),
                adapter.stream_sessionless_completion(&connection, request(), Some(tmp.path())),
            )
            .await;
            assert!(
                matches!(result, Err(ProviderError::ScheduledDispatchAbandoned)),
                "{protocol}"
            );
            assert!(!marker.exists());
        }
    }
    #[tokio::test]
    async fn abandoned_compaction_preserves_history_and_returns_a_distinct_engine_result() {
        use crate::assistant::types::{
            CompactionTrigger, ContentPart, MessageRole, SessionContext,
        };
        use crate::assistant::{
            compaction, compaction_service, engine::AssistantEngineError, repository,
        };
        let (_tmp, pool) = crate::db::test_support::workspace_pool().await;
        let session = repository::create_session(&pool, SessionContext::default())
            .await
            .unwrap();
        for role in [MessageRole::User, MessageRole::Assistant, MessageRole::User] {
            repository::create_message(
                &pool,
                repository::CreateMessageParams {
                    session_id: session.id.clone(),
                    role,
                    content: vec![ContentPart::Text {
                        text: "history".into(),
                    }],
                    provider_metadata: None,
                    run_id: None,
                },
            )
            .await
            .unwrap();
        }
        let mut conversation = compaction_service::load_with_pending(&pool, &session.id)
            .await
            .unwrap();
        let connection = connection("codex", "/unavailable-provider-must-never-launch".into());
        let signal = WakeSignal::default();
        let lease = signal.dispatch_lease().unwrap();
        signal.will_sleep();
        let error = with_scheduled_dispatch(
            Some(lease),
            compaction_service::compact_conversation(
                &pool,
                &session,
                &connection,
                None,
                CompactionTrigger::Automatic,
                Some("run"),
                0,
                &mut conversation,
            ),
        )
        .await
        .expect_err("compaction must be abandoned");
        assert!(matches!(
            error,
            compaction::CompactionError::ScheduledDispatchAbandoned
        ));
        assert!(matches!(
            AssistantEngineError::from(error),
            AssistantEngineError::ScheduledDispatchAbandoned
        ));
        assert_eq!(conversation.messages().len(), 3);
        assert!(repository::latest_completed_compaction(&pool, &session.id)
            .await
            .unwrap()
            .is_none());
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM assistant_compactions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }
}
