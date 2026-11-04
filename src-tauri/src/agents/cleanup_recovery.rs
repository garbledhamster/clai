//! Restart recovery for the existing singleton cleanup intent. No replay of
//! condition, preparation or model work: only the bound after script is retried.
use std::future::Future;

use tauri::Manager;
use tokio::sync::oneshot;

use super::hooks::{finish_intent, CleanupIntent};
use super::scheduler::Scheduler;
use super::scripts::{self, CapturedScript, ScriptPhase, ScriptResult};
use crate::config::WorkspaceConfig;
use crate::AppState;

pub const RECOVERY_EXPLANATION: &str = "Interrupted cleanup remains outstanding. Preparation or the agent may not have started, and cleanup may already have finished before the interruption was recorded. Retrying can repeat its effects; the script must tolerate both cases. The workspace waits for cleanup or an explicit Skip, even outside allowed blocks and while future checks are disabled.";

/// Idempotent at startup, lazy open and dispatch. Do not turn a live attempt
/// into recovery merely because a settings read sees its durable intent.
pub(crate) fn restore_runtime(scheduler: &mut Scheduler, workspace: &WorkspaceConfig) {
    let runtime = scheduler.scripts.entry(workspace.id.clone()).or_default();
    if workspace.schedule.cleanup_intent.is_some() && !runtime.cleanup_owned {
        runtime.cleanup_owned = true;
        runtime.cleanup_interrupted = true;
        runtime.phase = ScriptPhase::After;
        runtime.error = Some(RECOVERY_EXPLANATION.into());
    }
    if !runtime.results.contains_key(&ScriptPhase::After) {
        if let Some(result) = workspace
            .schedule
            .cleanup_completion
            .as_ref()
            .and_then(|c| c.result.clone())
        {
            runtime.results.insert(ScriptPhase::After, result.clone());
            if !runtime.cleanup_owned && runtime.pending.is_none() && !runtime.running {
                runtime.record_result(ScriptPhase::After, result);
            }
        }
    }
}

/// Poll independently of schedule admission. Each workspace owns one detached
/// task, so an unanswered approval cannot hold up recovery elsewhere.
pub(crate) async fn poll(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    let locators = match state.workspace_index.read() {
        Ok(index) => index.locators_sorted(),
        Err(_) => return,
    };
    for locator in locators {
        let Ok(workspace) = crate::config::workspace_config::load(&locator.root_path) else {
            continue;
        };
        if workspace.schedule.cleanup_intent.is_none() {
            continue;
        }
        let should_start = {
            let mut scheduler = state.scheduler.lock().await;
            restore_runtime(&mut scheduler, &workspace);
            let runtime = &scheduler.scripts[&workspace.id];
            runtime.cleanup_interrupted
                && !runtime.recovery_active
                && !runtime.running
                && !scheduler.workspace_has_scheduled_claim(&workspace.id)
        };
        if !should_start {
            continue;
        }
        let app = app.clone();
        let workspace_id = workspace.id;
        tauri::async_runtime::spawn(async move {
            let state = app.state::<AppState>();
            if let Err(error) = recover(state.inner(), &workspace_id).await {
                tracing::warn!(%workspace_id, %error, "Cleanup remains outstanding");
            }
            // Durable completion releases queued manual work; an unresolved
            // cleanup keeps the same dispatcher gate in place.
            if !state
                .scheduler
                .lock()
                .await
                .script_is_running(&workspace_id)
            {
                if let Ok(pool) = state.workspace_db(&workspace_id).await {
                    resume_pending(&app, &workspace_id, &pool).await;
                }
            }
        });
    }
}

pub(crate) async fn resume_pending(
    app: &tauri::AppHandle,
    workspace_id: &str,
    pool: &crate::db::DbPool,
) {
    let deps = crate::assistant::engine::AssistantDeps {
        workspace_id: workspace_id.into(),
        pool: pool.clone(),
        app: app.clone(),
    };
    if let Ok(sessions) = crate::assistant::signals::sessions_with_pending(pool).await {
        for session in sessions {
            crate::assistant::dispatcher::kick_logged(&deps, &session).await;
        }
    }
}

pub(crate) async fn recover(state: &AppState, workspace_id: &str) -> Result<(), String> {
    recover_with_executor(state, workspace_id, |snapshot| {
        snapshot.execute(state, workspace_id)
    })
    .await
}

/// Controlled executor seam used by restart/crash verification. Cancellation
/// or process death drops this future without recording completion, preserving
/// the same durable intent for the next process.
pub(crate) async fn recover_with_executor<E, EF>(
    state: &AppState,
    workspace_id: &str,
    execute: E,
) -> Result<(), String>
where
    E: FnOnce(CapturedScript) -> EF,
    EF: Future<Output = ScriptResult>,
{
    let (workspace, _) = state.resolve_workspace_roster(workspace_id)?;
    let Some(intent) = workspace.schedule.cleanup_intent.clone() else {
        return Ok(());
    };
    {
        let mut scheduler = state.scheduler.lock().await;
        restore_runtime(&mut scheduler, &workspace);
        if scheduler.workspace_has_scheduled_claim(workspace_id) {
            return Ok(());
        }
        let runtime = scheduler.scripts.entry(workspace_id.into()).or_default();
        if runtime.recovery_active || runtime.running || !runtime.cleanup_interrupted {
            return Ok(());
        }
        runtime.recovery_active = true;
    }
    let outcome = recover_attempt(state, workspace_id, &intent, execute).await;
    let mut scheduler = state.scheduler.lock().await;
    let runtime = scheduler.scripts.entry(workspace_id.into()).or_default();
    runtime.recovery_active = false;
    if let Err(error) = &outcome {
        runtime.running = false;
        runtime.error = Some(format!("{error} Restore the original cleanup input and access, or explicitly Skip cleanup. {RECOVERY_EXPLANATION}"));
    }
    outcome
}

async fn recover_attempt<E, EF>(
    state: &AppState,
    workspace_id: &str,
    intent: &CleanupIntent,
    execute: E,
) -> Result<(), String>
where
    E: FnOnce(CapturedScript) -> EF,
    EF: Future<Output = ScriptResult>,
{
    let pool = state.workspace_db(workspace_id).await?;
    if crate::assistant::repository::workspace_has_active_run(&pool).await? {
        return Err("Waiting for existing workspace work before cleanup recovery.".into());
    }
    // Always recapture the original bound source; never substitute the current
    // schedule's after script or changed file bytes for outstanding cleanup.
    let snapshot = intent.capture(state, workspace_id)?;
    let (workspace, _) = state.resolve_workspace_roster(workspace_id)?;
    let unattended = intent
        .unattended_consent
        .as_ref()
        .is_some_and(|c| c.allows(&snapshot))
        && workspace
            .schedule
            .after_consent
            .as_ref()
            .is_some_and(|c| c.allows(&snapshot));
    let approved = if unattended {
        Some(snapshot)
    } else {
        let (send, receive) = oneshot::channel();
        {
            let mut scheduler = state.scheduler.lock().await;
            let runtime = scheduler.scripts.entry(workspace_id.into()).or_default();
            runtime.request_for(snapshot, "recovery", ScriptPhase::After, Some(send));
            runtime.error = Some(RECOVERY_EXPLANATION.into());
        }
        // A lost sender is an interruption, never an explicit Skip.
        receive
            .await
            .map_err(|_| "Cleanup approval was interrupted; its intent was retained.".to_string())?
    };
    let Some(snapshot) = approved else {
        return record_completion(state, workspace_id, &intent.id, None).await;
    };
    let fresh = intent.capture(state, workspace_id)?;
    if !snapshot.same_approval(&fresh) {
        return Err("Cleanup changed after approval.".into());
    }
    {
        let mut scheduler = state.scheduler.lock().await;
        let runtime = scheduler.scripts.entry(workspace_id.into()).or_default();
        runtime.pending = None;
        runtime.phase = ScriptPhase::After;
        runtime.running = true;
        runtime.error = None;
    }
    let mut result = execute(snapshot).await;
    scripts::normalize_result(ScriptPhase::After, &mut result);
    // Policy/source/setup failures before launch remain actionable. A process
    // that started and exited unsuccessfully is a completed failure.
    if result.started == Some(false) {
        let error = result
            .error
            .clone()
            .unwrap_or_else(|| "Cleanup did not start.".into());
        state
            .scheduler
            .lock()
            .await
            .scripts
            .entry(workspace_id.into())
            .or_default()
            .record_result(ScriptPhase::After, result);
        return Err(error);
    }
    record_completion(state, workspace_id, &intent.id, Some(result)).await
}

pub(crate) async fn record_completion(
    state: &AppState,
    workspace_id: &str,
    intent_id: &str,
    result: Option<ScriptResult>,
) -> Result<(), String> {
    // Order release with manual claims; nothing may start between durable
    // completion and releasing the in-memory cleanup gate.
    let mut scheduler = state.scheduler.lock().await;
    finish_intent(state, workspace_id, intent_id, result.clone())?;
    let runtime = scheduler.scripts.entry(workspace_id.into()).or_default();
    runtime.pending = None;
    runtime.running = false;
    runtime.cleanup_owned = false;
    runtime.cleanup_interrupted = false;
    if let Some(result) = result {
        runtime.record_result(ScriptPhase::After, result);
    } else {
        runtime.error = None;
    }
    Ok(())
}

pub(crate) async fn skip_unavailable(
    state: &AppState,
    workspace_id: &str,
    intent_id: &str,
) -> Result<(), String> {
    let mut scheduler = state.scheduler.lock().await;
    let runtime = scheduler.scripts.entry(workspace_id.into()).or_default();
    if !runtime.cleanup_interrupted
        || runtime.running
        || runtime.recovery_active
        || runtime.pending.is_some()
    {
        return Err(
            "Cleanup is active or has an approval. Resolve its current request before skipping."
                .into(),
        );
    }
    finish_intent(state, workspace_id, intent_id, None)?;
    runtime.cleanup_owned = false;
    runtime.cleanup_interrupted = false;
    runtime.error = None;
    Ok(())
}
