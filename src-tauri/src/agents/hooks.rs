//! Preparation and cleanup extend the existing workspace attempt; they own no queue.
//! Durable cleanup intent is written before any preparation/model work can start.
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::runner::RunnerError;
use super::scheduler::DispatchLease;
use super::scripts::{
    self, CapturedScript, ScriptConfig, ScriptConsent, ScriptPhase, ScriptResult,
};
use crate::{db::DbPool, AppState};

/// Both origin and captured bytes are retained. Recovery must validate the
/// original input and current access against identity, then execute these bytes.
/// A crash after execution but before clearing this record may repeat cleanup.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupIntent {
    pub id: String,
    pub session_id: String,
    pub run_id: String,
    pub script: ScriptConfig,
    pub content: String,
    pub interpreter: String,
    pub access: String,
    pub identity: ScriptConsent,
    pub unattended_consent: Option<ScriptConsent>,
}

/// Completion and intent removal share one atomic configuration write. Retain
/// the most recent result so a completed failure stays visible after restart.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupCompletion {
    pub intent_id: String,
    pub completed_at_unix_ms: i64,
    pub skipped: bool,
    pub result: Option<ScriptResult>,
}

pub(crate) fn finish_intent(
    state: &AppState,
    workspace_id: &str,
    id: &str,
    result: Option<ScriptResult>,
) -> Result<(), String> {
    state.update_workspace_config(workspace_id, |workspace| {
        if workspace
            .schedule
            .cleanup_intent
            .as_ref()
            .is_none_or(|intent| intent.id != id)
        {
            return Err("Cleanup ownership changed; its durable intent was preserved.".into());
        }
        workspace.schedule.cleanup_completion = Some(CleanupCompletion {
            intent_id: id.into(),
            completed_at_unix_ms: chrono::Utc::now().timestamp_millis(),
            skipped: result.is_none(),
            result,
        });
        workspace.schedule.cleanup_intent = None;
        Ok(())
    })?;
    Ok(())
}

impl CleanupIntent {
    pub(crate) fn capture(
        &self,
        state: &AppState,
        workspace_id: &str,
    ) -> Result<CapturedScript, String> {
        let captured = scripts::capture(state, workspace_id, &self.script)?;
        if !self.identity.allows(&captured)
            || self.content != captured.content
            || self.interpreter != captured.interpreter.display().to_string()
        {
            return Err("The interrupted cleanup's script, interpreter, Main agent or access changed. Cleanup requires attention.".into());
        }
        Ok(captured)
    }
}

pub(crate) fn persist_intent(
    state: &AppState,
    workspace_id: &str,
    session_id: &str,
    run_id: &str,
    snapshot: &CapturedScript,
    consent: Option<ScriptConsent>,
) -> Result<CleanupIntent, String> {
    let intent = CleanupIntent {
        id: uuid::Uuid::new_v4().to_string(),
        session_id: session_id.into(),
        run_id: run_id.into(),
        script: snapshot.config.clone(),
        content: snapshot.content.clone(),
        interpreter: snapshot.interpreter.display().to_string(),
        access: snapshot.access.clone(),
        identity: snapshot.consent(),
        unattended_consent: consent,
    };
    state.update_workspace_config(workspace_id, |workspace| {
        if workspace.schedule.cleanup_intent.is_some() {
            return Err("Outstanding cleanup already owns this workspace.".into());
        }
        // Recheck the same atomic configuration used by settings writes.
        let current = scripts::capture_for_config(state, workspace, &snapshot.config)?;
        if !current.same_approval(snapshot) {
            return Err("Cleanup input or access changed before preparation.".into());
        }
        workspace.schedule.cleanup_intent = Some(intent.clone());
        Ok(())
    })?;
    Ok(intent)
}

pub(crate) fn clear_intent(state: &AppState, workspace_id: &str, id: &str) -> Result<(), String> {
    state.update_workspace_config(workspace_id, |workspace| {
        if workspace
            .schedule
            .cleanup_intent
            .as_ref()
            .is_some_and(|intent| intent.id == id)
        {
            workspace.schedule.cleanup_intent = None;
        } else {
            return Err("Cleanup ownership changed; its durable intent was preserved.".into());
        }
        Ok(())
    })?;
    Ok(())
}

async fn eligible(
    pool: &DbPool,
    session_id: &str,
    lease: &DispatchLease,
    cancel: &CancellationToken,
) -> Result<bool, String> {
    Ok(!cancel.is_cancelled()
        && lease.is_current()
        && !crate::assistant::repository::has_pending_user_message(pool, session_id).await?)
}

/// Condition/before approval is provisional. Cleanup approval survives all
/// start restrictions and keeps workspace ownership until execution or Skip.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn approve_phase(
    state: &AppState,
    workspace_id: &str,
    pool: &DbPool,
    session_id: &str,
    lease: &DispatchLease,
    cancel: &CancellationToken,
    phase: ScriptPhase,
    mut snapshot: CapturedScript,
    bound_consent: Option<ScriptConsent>,
) -> Result<Option<CapturedScript>, RunnerError> {
    loop {
        if phase != ScriptPhase::After
            && !eligible(pool, session_id, lease, cancel)
                .await
                .map_err(RunnerError::AssistantPersistence)?
        {
            return Err(RunnerError::ScheduledDispatchAbandoned);
        }
        let current = scripts::capture(state, workspace_id, &snapshot.config);
        let unchanged = current
            .as_ref()
            .is_ok_and(|current| current.same_approval(&snapshot));
        let consent = if phase == ScriptPhase::After {
            bound_consent.clone()
        } else {
            let (workspace, _) = state
                .resolve_workspace_roster(workspace_id)
                .map_err(RunnerError::AssistantPersistence)?;
            phase.consent(&workspace.schedule).cloned()
        };
        if unchanged
            && consent
                .as_ref()
                .is_some_and(|consent| consent.allows(&snapshot))
        {
            return Ok(Some(snapshot));
        }
        if phase != ScriptPhase::After {
            snapshot = current.map_err(RunnerError::AssistantPersistence)?;
        }
        let (send, mut receive) = oneshot::channel();
        {
            let mut scheduler = state.scheduler.lock().await;
            let runtime = scheduler.scripts.entry(workspace_id.into()).or_default();
            runtime.request_for(snapshot.clone(), "scheduled", phase, Some(send));
            if !unchanged && phase == ScriptPhase::After {
                runtime.error = Some("The bound cleanup input or access changed. Restore it or explicitly Skip cleanup.".into());
            }
        }
        let approved = loop {
            tokio::select! {
                decision = &mut receive => break decision.unwrap_or(None),
                _ = tokio::time::sleep(std::time::Duration::from_millis(100)), if phase != ScriptPhase::After => {
                    if !eligible(pool, session_id, lease, cancel).await.map_err(RunnerError::AssistantPersistence)? {
                        state.scheduler.lock().await.scripts.entry(workspace_id.into()).or_default().pending = None;
                        return Err(RunnerError::ScheduledDispatchAbandoned);
                    }
                }
            }
        };
        let Some(approved) = approved else {
            return Ok(None);
        };
        if phase != ScriptPhase::After
            && !eligible(pool, session_id, lease, cancel)
                .await
                .map_err(RunnerError::AssistantPersistence)?
        {
            return Err(RunnerError::ScheduledDispatchAbandoned);
        }
        let current = scripts::capture(state, workspace_id, &approved.config)
            .map_err(RunnerError::AssistantPersistence)?;
        if current.same_approval(&approved) {
            return Ok(Some(approved));
        }
        if phase != ScriptPhase::After {
            snapshot = current;
        }
    }
}

async fn capture_phase(
    state: &AppState,
    workspace_id: &str,
    phase: ScriptPhase,
    script: Option<&ScriptConfig>,
) -> Result<Option<CapturedScript>, RunnerError> {
    match script
        .map(|script| scripts::capture(state, workspace_id, script))
        .transpose()
    {
        Ok(snapshot) => Ok(snapshot),
        Err(error) => {
            let mut scheduler = state.scheduler.lock().await;
            let runtime = scheduler.scripts.entry(workspace_id.into()).or_default();
            runtime.phase = phase;
            runtime.error = Some(error.clone());
            Err(RunnerError::AssistantPersistence(error))
        }
    }
}

async fn execute_phase<E, EF>(
    state: &AppState,
    workspace_id: &str,
    phase: ScriptPhase,
    snapshot: CapturedScript,
    execute: &mut E,
) -> ScriptResult
where
    E: FnMut(CapturedScript, ScriptPhase) -> EF,
    EF: std::future::Future<Output = ScriptResult>,
{
    {
        let mut scheduler = state.scheduler.lock().await;
        let runtime = scheduler.scripts.entry(workspace_id.into()).or_default();
        runtime.phase = phase;
        runtime.result = runtime.results.get(&phase).cloned();
        runtime.running = true;
        runtime.error = None;
    }
    let mut result = execute(snapshot, phase).await;
    scripts::normalize_result(phase, &mut result);
    state
        .scheduler
        .lock()
        .await
        .scripts
        .entry(workspace_id.into())
        .or_default()
        .record_result(phase, result.clone());
    result
}

/// Runs after an allowed condition. Cleanup wraps engine completion, so engine
/// cancellation/failure and its eager manual dispatch cannot release ownership.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_prepared<F, Fut>(
    state: &AppState,
    workspace_id: &str,
    pool: &DbPool,
    session_id: &str,
    run_id: &str,
    lease: &DispatchLease,
    cancel: CancellationToken,
    mut emit: impl FnMut(crate::assistant::events::AssistantUiEvent),
    start: F,
) -> Result<(), RunnerError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<(), RunnerError>>,
{
    let result = run_prepared_with_executor(
        state,
        workspace_id,
        pool,
        session_id,
        run_id,
        lease,
        cancel,
        &mut emit,
        |snapshot, _| snapshot.execute(state, workspace_id),
        start,
    )
    .await;
    if let Err(error) = &result {
        if !matches!(
            error,
            RunnerError::ScheduledDispatchAbandoned | RunnerError::ScheduledPreparationAbandoned(_)
        ) {
            // Capture/persistence errors happen before engine entry and still
            // own a queued row. Never strand it as an active model run.
            cancel_queued(pool, run_id, &mut emit).await?;
            let mut scheduler = state.scheduler.lock().await;
            let runtime = scheduler.scripts.entry(workspace_id.into()).or_default();
            runtime.running = false;
            runtime.error = Some(error.to_string());
        }
    }
    result
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_prepared_with_executor<F, Fut, E, EF>(
    state: &AppState,
    workspace_id: &str,
    pool: &DbPool,
    session_id: &str,
    run_id: &str,
    lease: &DispatchLease,
    cancel: CancellationToken,
    mut emit: impl FnMut(crate::assistant::events::AssistantUiEvent),
    mut execute: E,
    start: F,
) -> Result<(), RunnerError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<(), RunnerError>>,
    E: FnMut(CapturedScript, ScriptPhase) -> EF,
    EF: std::future::Future<Output = ScriptResult>,
{
    let (workspace, _) = state
        .resolve_workspace_roster(workspace_id)
        .map_err(RunnerError::AssistantPersistence)?;
    let after = capture_phase(
        state,
        workspace_id,
        ScriptPhase::After,
        workspace.schedule.after.as_ref(),
    )
    .await?;
    let before = capture_phase(
        state,
        workspace_id,
        ScriptPhase::Before,
        workspace.schedule.before.as_ref(),
    )
    .await?;
    let before = match before {
        Some(snapshot) => {
            approve_phase(
                state,
                workspace_id,
                pool,
                session_id,
                lease,
                &cancel,
                ScriptPhase::Before,
                snapshot,
                None,
            )
            .await?
        }
        None => None,
    };
    if workspace.schedule.before.is_some() && before.is_none() {
        state
            .scheduler
            .lock()
            .await
            .scripts
            .entry(workspace_id.into())
            .or_default()
            .error = Some("Before script was skipped; the agent did not start.".into());
        return cancel_queued(pool, run_id, &mut emit).await;
    }
    if !eligible(pool, session_id, lease, &cancel)
        .await
        .map_err(RunnerError::AssistantPersistence)?
    {
        return Err(RunnerError::ScheduledDispatchAbandoned);
    }
    let intent = if let Some(snapshot) = &after {
        // Set ownership while ordered with all manual claims; durable persistence
        // precedes the external process and cannot be skipped on write failure.
        let mut scheduler = state.scheduler.lock().await;
        let intent = persist_intent(
            state,
            workspace_id,
            session_id,
            run_id,
            snapshot,
            workspace.schedule.after_consent.clone(),
        )
        .map_err(RunnerError::AssistantPersistence)?;
        scheduler
            .scripts
            .entry(workspace_id.into())
            .or_default()
            .cleanup_owned = true;
        Some(intent)
    } else {
        None
    };
    // Persistence can take long enough for a block to close or manual input to
    // arrive. Check again before the first external side effect.
    if !eligible(pool, session_id, lease, &cancel)
        .await
        .map_err(RunnerError::AssistantPersistence)?
    {
        if let Some(intent) = &intent {
            clear_intent(state, workspace_id, &intent.id)
                .map_err(RunnerError::AssistantPersistence)?;
            state
                .scheduler
                .lock()
                .await
                .scripts
                .entry(workspace_id.into())
                .or_default()
                .cleanup_owned = false;
        }
        return Err(RunnerError::ScheduledDispatchAbandoned);
    }
    let prepared = if let Some(snapshot) = before {
        let result = execute_phase(
            state,
            workspace_id,
            ScriptPhase::Before,
            snapshot,
            &mut execute,
        )
        .await;
        (result.started.unwrap_or(false), result.condition_allows())
    } else {
        (false, true)
    };
    let admission = eligible(pool, session_id, lease, &cancel).await;
    let outcome = match admission {
        Ok(true) if prepared.1 => start().await,
        Ok(false) if !prepared.0 && prepared.1 && !cancel.is_cancelled() => {
            Err(RunnerError::ScheduledDispatchAbandoned)
        }
        Ok(_) => cancel_queued(pool, run_id, &mut emit).await,
        Err(error) => Err(RunnerError::AssistantPersistence(error)),
    };
    // Provider commitment tells us whether an engine call actually started.
    let cleanup_required = prepared.0 || lease.is_committed();
    if let Some(intent) = intent {
        let completion = if cleanup_required {
            let approved = approve_phase(
                state,
                workspace_id,
                pool,
                session_id,
                lease,
                &cancel,
                ScriptPhase::After,
                after.unwrap(),
                intent.unattended_consent.clone(),
            )
            .await;
            match approved {
                Ok(Some(snapshot)) => {
                    let result = execute_phase(
                        state,
                        workspace_id,
                        ScriptPhase::After,
                        snapshot,
                        &mut execute,
                    )
                    .await;
                    if result.started == Some(false) {
                        let error = result
                            .error
                            .clone()
                            .unwrap_or_else(|| "Cleanup did not start.".into());
                        let mut scheduler = state.scheduler.lock().await;
                        scheduler
                            .scripts
                            .entry(workspace_id.into())
                            .or_default()
                            .cleanup_interrupted = true;
                        return Err(RunnerError::AssistantPersistence(error));
                    }
                    Some(result)
                }
                Ok(None) => None,
                Err(error) => {
                    let mut scheduler = state.scheduler.lock().await;
                    let runtime = scheduler.scripts.entry(workspace_id.into()).or_default();
                    runtime.cleanup_interrupted = true;
                    runtime.error = Some(format!("Cleanup remains pending: {error}"));
                    return Err(error);
                }
            }
        } else {
            None
        };
        let recorded = if cleanup_required {
            finish_intent(state, workspace_id, &intent.id, completion)
        } else {
            clear_intent(state, workspace_id, &intent.id)
        };
        if let Err(error) = recorded {
            let mut scheduler = state.scheduler.lock().await;
            let runtime = scheduler.scripts.entry(workspace_id.into()).or_default();
            runtime.cleanup_interrupted = true;
            runtime.error = Some(error.clone());
            return Err(RunnerError::AssistantPersistence(error));
        }
        state
            .scheduler
            .lock()
            .await
            .scripts
            .entry(workspace_id.into())
            .or_default()
            .cleanup_owned = false;
    }
    // Once preparation began this was an attempt, even if its start lease later
    // expired. Finish its cadence after cleanup; do not carry it into a new block.
    if prepared.0 {
        match outcome {
            Err(RunnerError::ScheduledPreparationAbandoned(previous)) => {
                if let Some(run) = crate::assistant::repository::get_run(pool, run_id)
                    .await
                    .map_err(RunnerError::AssistantPersistence)?
                {
                    crate::assistant::run_lifecycle::rollback_scheduled_preparation(
                        pool,
                        &run,
                        Some(&previous),
                        &mut emit,
                    )
                    .await
                    .map_err(RunnerError::AssistantPersistence)?;
                }
                cancel_queued(pool, run_id, &mut emit).await
            }
            Err(RunnerError::ScheduledDispatchAbandoned) => {
                cancel_queued(pool, run_id, &mut emit).await
            }
            other => other,
        }
    } else {
        outcome
    }
}

async fn cancel_queued(
    pool: &DbPool,
    run_id: &str,
    emit: &mut impl FnMut(crate::assistant::events::AssistantUiEvent),
) -> Result<(), RunnerError> {
    if crate::assistant::repository::get_run(pool, run_id)
        .await
        .map_err(RunnerError::AssistantPersistence)?
        .is_some_and(|run| run.status.is_active())
    {
        crate::assistant::run_lifecycle::cancel_run_with_sink(pool, run_id, emit)
            .await
            .map_err(|error| RunnerError::AssistantPersistence(error.to_string()))?;
    }
    Ok(())
}
