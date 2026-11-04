//! Agent Runner - Background task that executes agents.
//!
//! This module provides the background loop that:
//! 1. Periodically checks the scheduler for ready agents
//! 2. Executes automations through the assistant runtime
//! 3. Handles completion and errors
//!
//! # Starting the Runner
//!
//! The runner is started during app initialization via `start_agent_runner()`.
//! It runs continuously in the background, checking for work every 5 seconds.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                      AGENT RUNNER                                │
//! │                                                                  │
//! │  loop {                                                          │
//! │      sleep(CHECK_INTERVAL)                                       │
//! │           │                                                      │
//! │           ▼                                                      │
//! │      scheduler.next_ready()                                      │
//! │           │                                                      │
//! │           ▼ (if Some)                                            │
//! │      get definition + instance                                   │
//! │           │                                                      │
//! │           ▼                                                      │
//! │      engine::run_session_turn(...)                               │
//! │           │                                                      │
//! │           ▼                                                      │
//! │      scheduler.complete_agent(success)                           │
//! │  }                                                               │
//! └─────────────────────────────────────────────────────────────────┘
//! ```

use std::time::Duration;

use tauri::{AppHandle, Manager};

use crate::agents::{SchedulerState, SharedScheduler};
use crate::assistant::dispatcher;
use crate::assistant::engine::{self, AssistantDeps, RunTurnInput};
use crate::assistant::events::{emit_event, AssistantUiEvent, RunSkipReason};
use crate::assistant::repository::{self, CreateRunParams};
use crate::assistant::runtime;
use crate::assistant::types::{
    AssistantRun, AssistantSession, ProviderConnection, RunStatus, RunTrigger,
};
use crate::config::{workspace_config, AgentConfig};
use crate::db::DbPool;
use crate::AppState;

/// Loads a workspace-local agent from `<workspace>/.clai/config.json`.
///
/// Reads only `workspace_id`'s config, so another workspace's unreadable
/// config cannot stop this one's scheduled runs.
fn load_workspace_agent_as_config(
    state: &AppState,
    workspace_id: &str,
    id: &str,
) -> Result<Option<AgentConfig>, RunnerError> {
    let app_config = state
        .config_manager
        .lock()
        .map_err(|e| RunnerError::AssistantPersistence(format!("Lock error: {}", e)))?
        .get();
    let (config, roster) = state.resolve_workspace_roster(workspace_id).map_err(|e| {
        RunnerError::AssistantPersistence(format!("Failed to load workspace config: {}", e))
    })?;
    let Some(agent) = roster
        .iter()
        .map(|resolved| &resolved.agent)
        .find(|agent| agent.id == id)
    else {
        return Ok(None);
    };
    let created_at = chrono::DateTime::from_timestamp_millis(agent.created_at)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_default();
    let updated_at = chrono::DateTime::from_timestamp_millis(agent.updated_at)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_default();
    let is_manager = agent.id == config.main_agent_id();
    Ok(Some(AgentConfig {
        id: agent.id.clone(),
        workspace_id: config.id,
        name: agent.name.clone(),
        description: agent.description.clone(),
        // Schedule is workspace-level. Only the default (manager)
        // agent inherits the workspace schedule; sub-agents never
        // run on their own schedule.
        schedule_enabled: is_manager && config.schedule.enabled,
        schedule_kind: if is_manager {
            config.schedule.kind.clone()
        } else {
            crate::config::workspace_config::ScheduleKind::default()
        },
        enabled: agent.enabled,
        provider_connection_ids: agent.provider_connection_ids.clone(),
        selected_skill_ids: workspace_config::refs_to_skill_ids(
            &app_config,
            &agent.selected_skills,
        ),
        created_at,
        updated_at,
    }))
}

// =============================================================================
// Configuration
// =============================================================================

/// How often to check for ready agents (in seconds).
const CHECK_INTERVAL_SECS: u64 = 5;

/// How long a scheduled fire that failed to start (no enabled provider,
/// config or DB error, ...) waits before it is retried.
const START_FAILURE_RETRY_MS: i64 = 5 * 60 * 1000;

// =============================================================================
// Runner
// =============================================================================

/// Starts the agent runner background task.
///
/// This spawns a tokio task that runs indefinitely, periodically checking
/// for agents that need to run and executing them.
///
/// # Arguments
///
/// * `app_handle` - Tauri app handle for accessing state and emitting events
/// * `scheduler` - The shared agent scheduler
///
/// # Returns
///
/// A handle to the spawned task (can be used to abort if needed).
pub fn start_agent_runner(app_handle: AppHandle, scheduler: SharedScheduler) {
    tracing::info!(
        check_interval_secs = CHECK_INTERVAL_SECS,
        "Starting agent runner"
    );

    // Cleanup must progress even while another scheduled turn is active.
    let cleanup_app = app_handle.clone();
    let cleanup_scheduler = scheduler.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(CHECK_INTERVAL_SECS)).await;
            retry_pending_cancellations(&cleanup_app, &cleanup_scheduler).await;
        }
    });

    // Use Tauri's async runtime to spawn the background task
    tauri::async_runtime::spawn(async move {
        tracing::info!("Agent runner background task started");
        loop {
            // Sleep first to avoid running immediately on startup
            tokio::time::sleep(Duration::from_secs(CHECK_INTERVAL_SECS)).await;

            tracing::debug!("Checking for ready agents...");

            // Check for and run agents
            if let Err(e) = run_next_agent(&app_handle, &scheduler).await {
                tracing::error!(error = %e, "Agent runner error");
            }
        }
    });
}

/// Checks for and runs the next ready agent.
///
/// This is called periodically by the runner loop. It:
/// 1. Checks if the scheduler is paused
/// 2. Gets the next ready agent (if any)
/// 3. Executes the agent
/// 4. Marks it complete
async fn run_next_agent(
    app_handle: &AppHandle,
    scheduler: &SharedScheduler,
) -> Result<(), RunnerError> {
    {
        let state = app_handle.state::<AppState>();
        let mut sched = scheduler.lock().await;
        if let Err(error) = crate::agents::init::reconcile_deferred(&mut sched, |workspace_id| {
            let root = state
                .workspace_root(workspace_id)
                .ok_or_else(|| format!("Workspace not found: {workspace_id}"))?;
            workspace_config::load(&root).map_err(|error| error.to_string())
        }) {
            tracing::warn!(%error, "Deferred schedule reconciliation will be retried");
        }
    }
    let app = app_handle.clone();
    let owned_scheduler = scheduler.clone();
    dispatch_ready_agents(scheduler, move |instance_id| {
        let app = app.clone();
        let scheduler = owned_scheduler.clone();
        async move { run_claimed_instance(&app, &scheduler, &instance_id).await }
    })
    .await;
    Ok(())
}

/// Claim one bounded polling batch, then detach its attempts. Collect before
/// spawning so an immediately busy session cannot be reclaimed in this poll.
async fn dispatch_ready_agents<F, Fut>(
    scheduler: &SharedScheduler,
    run: F,
) -> Vec<tokio::task::JoinHandle<()>>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Result<(), RunnerError>> + Send + 'static,
{
    let mut claimed = Vec::new();
    while let Some(id) = claim_next_instance(scheduler).await {
        claimed.push(id);
    }
    claimed
        .into_iter()
        .map(|instance_id| {
            let body = run(instance_id.clone());
            let scheduler = scheduler.clone();
            tokio::spawn(async move {
                if let Err(error) = release_slot_on_error(&scheduler, &instance_id, body).await {
                    tracing::error!(%instance_id, %error, "Scheduled attempt failed");
                }
            })
        })
        .collect()
}

/// Claims the next ready instance, marking it running in the scheduler.
async fn claim_next_instance(scheduler: &SharedScheduler) -> Option<String> {
    let mut sched = scheduler.lock().await;

    let instance_count = sched.instance_count();
    let state = sched.state();
    let running_agent = sched.running_instance();
    tracing::debug!(
        instance_count,
        scheduler_state = ?state,
        running_agent = ?running_agent,
        "Scheduler status"
    );

    if matches!(state, SchedulerState::Paused { .. }) {
        tracing::debug!("Scheduler is paused");
        return None;
    }

    sched.next_ready()
}

/// Awaits `body` for a claimed instance and, if it fails while the instance
/// still holds its workspace claim, releases the slot with a
/// retry after [`START_FAILURE_RETRY_MS`]. Without this an early error left
/// the instance marked running, blocking every later scheduled run. A missing
/// agent is retried too rather than removed: a failed workspace delete
/// restores the workspace and expects its scheduler entries intact, while a
/// successful delete removes them itself. The error is returned unchanged for
/// the runner loop to log.
async fn release_slot_on_error(
    scheduler: &SharedScheduler,
    instance_id: &str,
    body: impl std::future::Future<Output = Result<(), RunnerError>>,
) -> Result<(), RunnerError> {
    let result = body.await;
    if result.is_err() {
        let mut sched = scheduler.lock().await;
        if sched.is_instance_running(instance_id) {
            let retry_at = chrono::Utc::now().timestamp_millis() + START_FAILURE_RETRY_MS;
            sched.complete_agent(instance_id, false, retry_at);
        }
    }
    result
}

async fn claimed_instance_details(
    scheduler: &SharedScheduler,
    instance_id: &str,
) -> Result<
    (
        String,
        String,
        Option<crate::agents::scheduler::DispatchLease>,
    ),
    RunnerError,
> {
    let sched = scheduler.lock().await;
    let instance = sched
        .get_instance(instance_id)
        .ok_or_else(|| RunnerError::InstanceNotFound(instance_id.to_string()))?;
    let workspace_id = instance.workspace_id.clone();
    let agent_id = instance.agent_id.clone();
    Ok((
        workspace_id,
        agent_id,
        sched.take_claimed_dispatch_lease(instance_id),
    ))
}

/// Runs an instance already claimed by [`claim_next_instance`]. Every `Ok`
/// path releases the scheduler slot itself; errors are released by
/// [`release_slot_on_error`].
#[allow(
    clippy::cognitive_complexity,
    reason = "lint debt: cognitive complexity 85 against a budget of 25"
)]
async fn run_claimed_instance(
    app_handle: &AppHandle,
    scheduler: &SharedScheduler,
    instance_id: &str,
) -> Result<(), RunnerError> {
    let state = app_handle.state::<AppState>();

    let (workspace_id, agent_id, dispatch_lease) =
        claimed_instance_details(scheduler, instance_id).await?;

    let agent_config = load_workspace_agent_as_config(state.inner(), &workspace_id, &agent_id)?
        .ok_or_else(|| RunnerError::AgentNotFound(agent_id.clone()))?;
    let workspace_pool = state
        .workspace_db(&agent_config.workspace_id)
        .await
        .map_err(RunnerError::AssistantPersistence)?;

    // Check if the automation is still enabled.
    if !agent_config.enabled {
        scheduler.lock().await.remove_instance(instance_id);

        return Ok(());
    }

    let connection = resolve_agent_connection(state.inner(), &agent_config)?;

    let session =
        ensure_workspace_manager_session(app_handle, &workspace_pool, &agent_config).await?;

    let enhanced = scheduler.lock().await.is_enhanced(instance_id);
    let manual = scheduler
        .lock()
        .await
        .get_instance(instance_id)
        .is_some_and(|instance| instance.manual_run_pending);
    let run = start_tick_run(
        &workspace_pool,
        &session,
        &connection,
        TickContext {
            instance_id,
            agent_id: &agent_id,
            manual,
            enhanced,
        },
        |event| {
            let _ = emit_event(app_handle, &workspace_id, &session, None, event);
        },
    )
    .await?;
    if run.is_none() && enhanced {
        scheduler.lock().await.wait_for_session(instance_id);
        kick_after_abandon(app_handle, &workspace_id, &workspace_pool, &session.id).await;
        return Ok(());
    }
    let ran = run.is_some();
    let result = match run {
        Some(run) => {
            let registration = runtime::register_run(
                runtime::LiveRunKey::new(&workspace_id, &session.id),
                &run.id,
            );
            let outcome = run_queued_with_preflight(
                &workspace_pool,
                &session,
                &run,
                scheduler,
                instance_id,
                dispatch_lease.as_ref(),
                |event| {
                    let _ = emit_event(app_handle, &workspace_id, &session, None, event);
                },
                || {
                    run_scheduled_agent(
                        app_handle,
                        &agent_config.workspace_id,
                        &workspace_pool,
                        &session,
                        &connection,
                        run.clone(),
                        registration.token(),
                    )
                },
            )
            .await;
            drop(registration);
            match outcome? {
                StartOutcome::AbandonedOnSleepOrClockChange => {
                    kick_after_abandon(app_handle, &workspace_id, &workspace_pool, &session.id)
                        .await;
                    return Ok(());
                }
                StartOutcome::Finished(result) => result,
            }
        }
        None => Ok(()),
    };

    if let Err(e) = complete_tick_and_persist(
        state.inner(),
        scheduler,
        instance_id,
        log_tick_result(instance_id, &result),
        &agent_config,
        ran,
    )
    .await
    {
        tracing::warn!(workspace_id = %agent_config.workspace_id, error = %e,
            "Failed to persist run completion to workspace config");
    }

    Ok(())
}

async fn kick_after_abandon(app: &AppHandle, workspace_id: &str, pool: &DbPool, session_id: &str) {
    let deps = AssistantDeps {
        workspace_id: workspace_id.to_string(),
        pool: pool.clone(),
        app: app.clone(),
    };
    dispatcher::kick_logged(&deps, session_id).await;
}

fn log_tick_result(instance_id: &str, result: &Result<(), RunnerError>) -> bool {
    match result {
        Ok(()) => {
            tracing::info!(instance_id = %instance_id, "Agent completed successfully");
            true
        }
        Err(error) => {
            tracing::error!(instance_id = %instance_id, %error, "Agent execution error");
            false
        }
    }
}

enum StartOutcome {
    Finished(Result<(), RunnerError>),
    AbandonedOnSleepOrClockChange,
}

/// Check a queued attempt before engine entry. Later provider boundaries still
/// need their own guard; this preflight does not commit the lease.
#[allow(clippy::too_many_arguments)]
async fn run_queued_with_preflight<Fut>(
    pool: &DbPool,
    session: &AssistantSession,
    run: &AssistantRun,
    scheduler: &SharedScheduler,
    instance_id: &str,
    lease: Option<&crate::agents::scheduler::DispatchLease>,
    mut emit: impl FnMut(AssistantUiEvent),
    start: impl FnOnce() -> Fut,
) -> Result<StartOutcome, RunnerError>
where
    Fut: std::future::Future<Output = Result<(), RunnerError>>,
{
    let manual_check = if lease.is_some() {
        repository::has_pending_user_message(pool, &session.id).await
    } else {
        Ok(false)
    };
    // A failed preflight still owns a queued run row. Close it through the
    // same cleanup path before surfacing the error and releasing the claim.
    let preflight_error = manual_check.as_ref().err().cloned();
    let manual_waiting = manual_check.unwrap_or(false);
    let previous_cli_session = if preflight_error.is_none()
        && !manual_waiting
        && lease.is_none_or(|lease| lease.is_current())
    {
        match crate::assistant::provider_dispatch::with_scheduled_dispatch(lease.cloned(), start())
            .await
        {
            Err(RunnerError::ScheduledDispatchAbandoned) => None,
            Err(RunnerError::ScheduledPreparationAbandoned(previous)) => Some(previous),
            result => return Ok(StartOutcome::Finished(result)),
        }
    } else {
        None
    };
    if run.session_id != session.id {
        return Err(RunnerError::AssistantPersistence(
            "Scheduled run session mismatch".into(),
        ));
    }
    let cleanup = async {
        crate::assistant::run_lifecycle::rollback_scheduled_preparation(
            pool,
            run,
            previous_cli_session.as_ref(),
            &mut emit,
        )
        .await?;
        crate::assistant::run_lifecycle::cancel_run_with_sink(pool, &run.id, &mut emit)
            .await
            .map_err(|error| error.to_string())
    }
    .await;
    if let Err(error) = cleanup {
        scheduler.lock().await.retain_failed_cancellation(
            instance_id,
            &session.id,
            &run.id,
            previous_cli_session,
        );
        return Err(RunnerError::AssistantPersistence(error));
    }
    if (manual_waiting
        || repository::has_pending_user_message(pool, &session.id)
            .await
            .unwrap_or(false))
        && lease.is_none_or(|lease| lease.timing_is_current())
    {
        scheduler.lock().await.wait_for_session(instance_id);
    } else {
        scheduler.lock().await.abandon_enhanced_claim(instance_id);
    }
    if let Some(error) = preflight_error {
        return Err(RunnerError::AssistantPersistence(error));
    }
    Ok(StartOutcome::AbandonedOnSleepOrClockChange)
}

async fn retry_abandoned_cancellation(
    pool: &DbPool,
    scheduler: &SharedScheduler,
    workspace_id: &str,
    expected_run_id: &str,
    mut emit: impl FnMut(AssistantUiEvent),
) -> Result<(), String> {
    let pending = scheduler
        .lock()
        .await
        .pending_cancellations()
        .into_iter()
        .find(|pending| pending.workspace_id == workspace_id && pending.run_id == expected_run_id)
        .ok_or_else(|| "Pending cancellation changed".to_string())?;
    if let Some(run) = repository::get_run(pool, &pending.run_id).await? {
        crate::assistant::run_lifecycle::rollback_scheduled_preparation(
            pool,
            &run,
            pending.previous_cli_session.as_ref(),
            &mut emit,
        )
        .await?;
    }
    match repository::get_run(pool, &pending.run_id).await? {
        Some(run) if run.status.is_active() => {
            crate::assistant::run_lifecycle::cancel_run_with_sink(pool, &pending.run_id, emit)
                .await
                .map_err(|error| error.to_string())?;
        }
        Some(run) if run.status == RunStatus::Cancelled => {
            emit(AssistantUiEvent::RunCancelled { run });
        }
        _ => {}
    }
    scheduler
        .lock()
        .await
        .finish_pending_cancellation(workspace_id, &pending.run_id);
    Ok(())
}

/// Retry abandoned run cleanup independently of other workspaces' active turns.
async fn retry_pending_cancellations(app: &AppHandle, scheduler: &SharedScheduler) {
    let pending = scheduler.lock().await.pending_cancellations();
    let state = app.state::<AppState>();
    for item in pending {
        let result = async {
            let pool = state.workspace_db(&item.workspace_id).await?;
            let session = repository::get_session(&pool, &item.session_id).await?;
            retry_abandoned_cancellation(
                &pool,
                scheduler,
                &item.workspace_id,
                &item.run_id,
                |event| {
                    if let Some(session) = &session {
                        let _ = emit_event(app, &item.workspace_id, session, None, event);
                    }
                },
            )
            .await?;
            if let Some(session) = session {
                kick_after_abandon(app, &item.workspace_id, &pool, &session.id).await;
            }
            Ok::<(), String>(())
        }
        .await;
        if let Err(error) = result {
            tracing::warn!(workspace_id = %item.workspace_id, %error, "Scheduled run cancellation remains pending");
        }
    }
}

/// Releases the scheduler slot and schedules the next fire. Returns the next
/// fire time, `None` when the schedule cannot compute one.
fn complete_tick_locked(
    sched: &mut crate::agents::scheduler::Scheduler,
    instance_id: &str,
    success: bool,
    agent_config: &AgentConfig,
) -> Option<i64> {
    // Compute the next wall-clock fire time from the workspace's
    // schedule kind. For Interval mode this is `now + N min`; for Cron
    // this is the next cron fire in the user's timezone. Errors are
    // logged but non-fatal — the instance won't be re-scheduled, which
    // is the safest behavior in the face of a malformed cron expression
    // (user can fix via the UI without the agent loop-firing).
    let now_unix_ms = chrono::Utc::now().timestamp_millis();
    let next_run_at_unix_ms = match crate::agents::schedule::compute_next_run_at(
        &agent_config.schedule_kind,
        now_unix_ms,
    ) {
        Ok(target) => Some(target),
        Err(e) => {
            tracing::warn!(
                workspace_id = %agent_config.workspace_id,
                error = %e,
                "Failed to compute next run time; agent will not be rescheduled"
            );
            None
        }
    };
    // When we can't compute a next time, pass `i64::MAX` so the
    // scheduler parks the instance indefinitely. The user has to
    // fix the schedule via the UI before it ticks again.
    sched.complete_agent(
        instance_id,
        success,
        next_run_at_unix_ms.unwrap_or(i64::MAX),
    );

    // Log next run time
    if let Some(instance) = sched.get_instance(instance_id) {
        let seconds_until_next = instance.seconds_until_next_run();
        tracing::info!(
            instance_id = %instance_id,
            seconds_until_next,
            minutes_until_next = seconds_until_next / 60,
            "Scheduled next agent run"
        );
    }
    next_run_at_unix_ms
}

async fn complete_tick_and_persist(
    state: &AppState,
    scheduler: &SharedScheduler,
    instance_id: &str,
    success: bool,
    agent_config: &AgentConfig,
    ran: bool,
) -> Result<(), String> {
    let mut sched = scheduler.lock().await;
    let unchanged = !sched.completion_is_invalidated(instance_id);
    let next = complete_tick_locked(&mut sched, instance_id, success, agent_config);
    persist_workspace_run_completion(
        state,
        &agent_config.workspace_id,
        next,
        ran,
        &agent_config.schedule_kind,
        unchanged,
    )
}

/// Which scheduler tick [`start_tick_run`] is starting.
struct TickContext<'a> {
    instance_id: &'a str,
    agent_id: &'a str,
    /// A user-requested "Run now" rather than a scheduled fire.
    manual: bool,
    enhanced: bool,
}

/// Creates the tick's run using the session's atomic idle check. Enhanced
/// ticks also yield to pending user messages; their caller retains one due
/// attempt while legacy busy ticks advance as before. A skipped Run now is
/// reported through `emit` because a user is waiting on it.
async fn start_tick_run(
    pool: &DbPool,
    session: &AssistantSession,
    connection: &ProviderConnection,
    tick: TickContext<'_>,
    emit: impl FnOnce(AssistantUiEvent),
) -> Result<Option<AssistantRun>, RunnerError> {
    let params = CreateRunParams {
        session_id: session.id.clone(),
        status: RunStatus::Queued,
        trigger: RunTrigger::Scheduled,
        connection_id: connection.id.clone(),
        protocol_id: connection.protocol_id.clone(),
        model_id: connection.model_id.clone(),
        error: None,
    };
    let run = if tick.enhanced && !tick.manual {
        repository::create_scheduled_run_if_idle(pool, params).await
    } else {
        repository::create_run_if_idle(pool, params).await
    }
    .map_err(RunnerError::AssistantPersistence)?;
    if run.is_none() {
        let active_run_id = repository::active_run_id(pool, &session.id)
            .await
            .map_err(RunnerError::AssistantPersistence)?;
        tracing::info!(
            instance_id = tick.instance_id,
            agent_id = tick.agent_id,
            session_id = %session.id,
            manual = tick.manual,
            "session has an active run; skipping tick"
        );
        if tick.manual {
            emit(AssistantUiEvent::RunSkipped {
                reason: RunSkipReason::SessionBusy,
                active_run_id,
            });
        }
    }
    Ok(run)
}

/// Persist a run's completion to the workspace config: the new
/// `schedule.next_run_at_unix_ms` anchor (`None` parks the schedule) and
/// `last_run_completed_at` when the tick `ran`, which the workspace rail
/// compares against `last_opened_at` to show the "unread" indicator; a
/// skipped tick produced nothing to read. Refreshes the in-memory workspace
/// index so subsequent loads see both. Failed runs stamp completion too — a
/// failure the user hasn't seen is exactly the kind of activity the unread
/// marker exists for.
pub(crate) fn persist_workspace_run_completion(
    state: &AppState,
    workspace_id: &str,
    next_run_at_unix_ms: Option<i64>,
    ran: bool,
    attempted_kind: &crate::config::workspace_config::ScheduleKind,
    schedule_unchanged: bool,
) -> Result<(), String> {
    let now = chrono::Utc::now().timestamp_millis();
    // Atomic RMW: `workspace_mark_opened` fires from the FE the moment a
    // run ends while the user is viewing the workspace, so a bare
    // load→save here raced it and the anchor written below could be
    // clobbered by the other writer's stale copy (the schedule then
    // re-fired on every app restart).
    state.update_workspace_config(workspace_id, |config| {
        if schedule_unchanged && config.schedule.enabled && &config.schedule.kind == attempted_kind
        {
            config.schedule.next_run_at_unix_ms = next_run_at_unix_ms;
        }
        if ran {
            config.last_run_completed_at = now;
        }
        config.updated_at = now;
        Ok(())
    })?;
    Ok(())
}

// =============================================================================
// Errors
// =============================================================================

/// Errors that can occur in the agent runner.
#[derive(Debug)]
pub enum RunnerError {
    ScheduledDispatchAbandoned,
    ScheduledPreparationAbandoned(crate::assistant::run_lifecycle::CliSessionBeforeStart),
    /// Agent instance not found.
    InstanceNotFound(String),
    /// Agent config not found in ConfigManager.
    AgentNotFound(String),
    /// Failed to persist or load assistant runtime data.
    AssistantPersistence(String),
    /// Failed to create or restore an assistant session.
    AssistantSession(String),
    /// No usable provider connections are configured for the agent.
    NoProviderConfigured(String),
}

impl std::fmt::Display for RunnerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunnerError::ScheduledPreparationAbandoned(_) => {
                write!(f, "scheduled preparation cleanup required")
            }
            RunnerError::ScheduledDispatchAbandoned => {
                write!(f, "scheduled attempt abandoned before provider dispatch")
            }
            RunnerError::InstanceNotFound(id) => write!(f, "Agent instance not found: {}", id),
            RunnerError::AgentNotFound(id) => write!(f, "Agent config not found: {}", id),
            RunnerError::AssistantPersistence(msg) => {
                write!(f, "Assistant persistence failed: {}", msg)
            }
            RunnerError::AssistantSession(msg) => write!(f, "Assistant session failed: {}", msg),
            RunnerError::NoProviderConfigured(msg) => write!(f, "No provider configured: {}", msg),
        }
    }
}

impl std::error::Error for RunnerError {}

/// Find-or-create the canonical (workspace, manager) session for a
/// scheduled run.
///
/// This is the **same** session row that the workspace card-click path
/// resolves via `commands::workspace::find_workspace_session`, so the user
/// sees one continuous thread regardless of how a turn was triggered. The
/// agent's configuration is resolved when the run starts, so an existing row
/// is used as-is.
async fn ensure_workspace_manager_session(
    app_handle: &AppHandle,
    pool: &DbPool,
    agent_config: &crate::config::AgentConfig,
) -> Result<crate::assistant::types::AssistantSession, RunnerError> {
    let session = repository::find_or_create_manager_session(pool, &agent_config.id)
        .await
        .map_err(RunnerError::AssistantPersistence)?;

    emit_event(
        app_handle,
        &agent_config.workspace_id,
        &session,
        None,
        AssistantUiEvent::SessionCreated {
            session: Box::new(session.clone()),
        },
    )
    .map_err(RunnerError::AssistantSession)?;

    Ok(session)
}

fn resolve_agent_connection(
    state: &AppState,
    agent_config: &AgentConfig,
) -> Result<ProviderConnection, RunnerError> {
    let all_connections = state
        .config_manager
        .lock()
        .map_err(|e| RunnerError::AssistantPersistence(format!("Lock error: {}", e)))?
        .get_provider_connections();

    let connection = crate::config::first_enabled_connection(
        &agent_config.provider_connection_ids,
        &all_connections,
    )
    .ok_or_else(|| RunnerError::NoProviderConfigured(agent_config.id.clone()))?;

    if agent_config.provider_connection_ids.first() != Some(&connection.id) {
        tracing::warn!(
            agent_id = %agent_config.id,
            connection_id = %connection.id,
            "First configured provider connection is disabled or missing; using the first enabled one"
        );
    }

    Ok(connection.clone())
}

async fn run_scheduled_agent(
    app_handle: &AppHandle,
    workspace_id: &str,
    pool: &DbPool,
    session: &AssistantSession,
    connection: &ProviderConnection,
    run: AssistantRun,
    cancel_token: tokio_util::sync::CancellationToken,
) -> Result<(), RunnerError> {
    let deps = AssistantDeps {
        workspace_id: workspace_id.to_string(),
        pool: pool.clone(),
        app: app_handle.clone(),
    };

    tracing::info!(
        session_id = %session.id,
        connection_id = %connection.id,
        protocol_id = %connection.protocol_id,
        model_id = %connection.model_id,
        "Starting scheduled assistant run"
    );

    let input = RunTurnInput {
        session_id: session.id.clone(),
        run_id: Some(run.id.clone()),
        trigger: RunTrigger::Scheduled,
        connection_id: connection.id.clone(),
        cancel_token,
    };
    let result = engine::run_session_turn(&deps, input).await;
    if !matches!(
        result,
        Err(engine::AssistantEngineError::ScheduledDispatchAbandoned
            | engine::AssistantEngineError::ScheduledPreparationAbandoned(_))
    ) {
        dispatcher::kick_logged(&deps, &session.id).await;
    }

    result.map_err(|error| {
        if let engine::AssistantEngineError::ScheduledPreparationAbandoned(previous) = error {
            return RunnerError::ScheduledPreparationAbandoned(previous);
        }
        if matches!(
            error,
            engine::AssistantEngineError::ScheduledDispatchAbandoned
        ) {
            return RunnerError::ScheduledDispatchAbandoned;
        }
        tracing::warn!(
            run_id = %run.id,
            connection_id = %connection.id,
            error = %error,
            "Scheduled assistant run failed"
        );
        RunnerError::AssistantSession(error.to_string())
    })
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_runner_error_display() {
        let err = RunnerError::InstanceNotFound("test-id".to_string());
        assert!(err.to_string().contains("test-id"));

        let err = RunnerError::AgentNotFound("test-agent".to_string());
        assert!(err.to_string().contains("test-agent"));
    }

    #[test]
    fn no_provider_configured_error_names_agent() {
        let err = RunnerError::NoProviderConfigured("agent-1".to_string());
        let text = err.to_string();
        assert!(text.contains("No provider configured"));
        assert!(text.contains("agent-1"));
    }

    #[tokio::test]
    async fn settings_write_waits_for_reconciliation_and_wins_after_old_snapshot() {
        use crate::config::workspace_config::{ScheduleKind, WorkspaceConfig};
        use crate::workspace_index::WorkspaceIndex;
        use std::sync::Arc;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        let mut config = WorkspaceConfig::new("ws".into(), "Workspace".into(), 0, "main".into());
        config.schedule.enabled = true;
        config.schedule.kind = ScheduleKind::Interval {
            interval_minutes: 1,
        };
        workspace_config::save(&root, &config).unwrap();
        let mut index = WorkspaceIndex::default();
        index.insert_config(root.clone(), &config);
        let cm = crate::ConfigManager::new_for_tests(
            crate::config::AppConfig::default(),
            temp.path().join("app.json"),
        );
        let state = Arc::new(AppState::new_for_tests(cm, index).unwrap());
        let mut sched = state.scheduler.lock().await;
        let stale_snapshot = workspace_config::load(&root).unwrap();
        let writer_state = state.clone();
        let writer = tokio::spawn(async move {
            crate::agents::init::update_schedule_and_reconcile(&writer_state, "ws", true, |c| {
                c.schedule.enabled = false;
                Ok(())
            })
            .await
            .unwrap();
        });
        tokio::task::yield_now().await;
        assert!(!writer.is_finished());
        assert!(workspace_config::load(&root).unwrap().schedule.enabled);
        crate::agents::init::apply_workspace_schedule(&mut sched, &stale_snapshot);
        drop(sched);
        writer.await.unwrap();
        assert!(!workspace_config::load(&root).unwrap().schedule.enabled);
        assert_eq!(state.scheduler.lock().await.next_ready(), None);
    }

    #[tokio::test]
    async fn pausing_an_active_run_keeps_its_completion_anchor() {
        use crate::config::workspace_config::{ScheduleKind, WorkspaceConfig};
        use crate::workspace_index::WorkspaceIndex;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        let mut config = WorkspaceConfig::new("ws".into(), "Workspace".into(), 0, "main".into());
        config.schedule.enabled = true;
        config.schedule.kind = ScheduleKind::Interval {
            interval_minutes: 1,
        };
        workspace_config::save(&root, &config).unwrap();
        let mut index = WorkspaceIndex::default();
        index.insert_config(root.clone(), &config);
        let cm = crate::ConfigManager::new_for_tests(
            crate::config::AppConfig::default(),
            temp.path().join("app.json"),
        );
        let state = AppState::new_for_tests(cm, index).unwrap();
        {
            let mut sched = state.scheduler.lock().await;
            crate::agents::init::apply_workspace_schedule(&mut sched, &config);
            assert_eq!(sched.next_ready().as_deref(), Some("main"));
        }
        let agent = load_workspace_agent_as_config(&state, "ws", "main")
            .unwrap()
            .unwrap();
        crate::agents::init::update_schedule_and_reconcile(&state, "ws", false, |c| {
            c.schedule.paused = true;
            Ok(())
        })
        .await
        .unwrap();
        complete_tick_and_persist(&state, &state.scheduler, "main", true, &agent, true)
            .await
            .unwrap();
        let saved = workspace_config::load(&root).unwrap();
        assert!(saved.schedule.paused);
        assert!(saved.schedule.next_run_at_unix_ms.is_some());
    }

    #[tokio::test]
    async fn same_kind_disable_and_reenable_preserves_new_anchor_on_old_completion() {
        use crate::config::workspace_config::{ScheduleKind, WorkspaceConfig};
        use crate::workspace_index::WorkspaceIndex;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        let mut config = WorkspaceConfig::new("ws".into(), "Workspace".into(), 0, "main".into());
        config.schedule.enabled = true;
        config.schedule.kind = ScheduleKind::Interval {
            interval_minutes: 1,
        };
        workspace_config::save(&root, &config).unwrap();
        let mut index = WorkspaceIndex::default();
        index.insert_config(root.clone(), &config);
        let cm = crate::ConfigManager::new_for_tests(
            crate::config::AppConfig::default(),
            temp.path().join("app.json"),
        );
        let state = AppState::new_for_tests(cm, index).unwrap();
        {
            let mut sched = state.scheduler.lock().await;
            crate::agents::init::apply_workspace_schedule(&mut sched, &config);
            assert_eq!(sched.next_ready().as_deref(), Some("main"));
        }
        let agent = load_workspace_agent_as_config(&state, "ws", "main")
            .unwrap()
            .unwrap();
        crate::agents::init::update_schedule_and_reconcile(&state, "ws", true, |c| {
            c.schedule.enabled = false;
            c.schedule.next_run_at_unix_ms = None;
            Ok(())
        })
        .await
        .unwrap();
        crate::agents::init::update_schedule_and_reconcile(&state, "ws", true, |c| {
            c.schedule.enabled = true;
            c.schedule.next_run_at_unix_ms = Some(123_456);
            Ok(())
        })
        .await
        .unwrap();
        complete_tick_and_persist(&state, &state.scheduler, "main", true, &agent, true)
            .await
            .unwrap();
        assert_eq!(
            workspace_config::load(&root)
                .unwrap()
                .schedule
                .next_run_at_unix_ms,
            Some(123_456)
        );
    }

    fn retry_test_workspace() -> (tempfile::TempDir, std::path::PathBuf, AppState) {
        use crate::config::workspace_config::{ScheduleKind, WorkspaceConfig};
        use crate::workspace_index::WorkspaceIndex;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        let mut config = WorkspaceConfig::new("ws".into(), "Workspace".into(), 0, "main".into());
        config.schedule.enabled = true;
        config.schedule.kind = ScheduleKind::Interval {
            interval_minutes: 1,
        };
        workspace_config::save(&root, &config).unwrap();
        let mut index = WorkspaceIndex::default();
        index.insert_config(root.clone(), &config);
        let cm = crate::ConfigManager::new_for_tests(
            crate::config::AppConfig::default(),
            temp.path().join("app.json"),
        );
        let state = AppState::new_for_tests(cm, index).unwrap();
        (temp, root, state)
    }

    #[tokio::test]
    async fn failed_start_keeps_retry_after_deferred_pause_and_resume() {
        use crate::config::workspace_config::ScheduleKind;
        let (_temp, root, state) = retry_test_workspace();
        {
            let config = workspace_config::load(&root).unwrap();
            let mut sched = state.scheduler.lock().await;
            crate::agents::init::apply_workspace_schedule(&mut sched, &config);
            assert_eq!(sched.next_ready().as_deref(), Some("main"));
        }
        for paused in [true, false] {
            crate::agents::init::update_schedule_and_reconcile(&state, "ws", false, |c| {
                c.schedule.paused = paused;
                Ok(())
            })
            .await
            .unwrap();
        }
        let result = release_slot_on_error(&state.scheduler, "main", async {
            Err(RunnerError::NoProviderConfigured("main".into()))
        })
        .await;
        assert!(result.is_err());
        let mut sched = state.scheduler.lock().await;
        let retry = sched.get_instance("main").unwrap().next_run_at.unwrap();
        crate::agents::init::reconcile_deferred(&mut sched, |_| {
            workspace_config::load(&root).map_err(|e| e.to_string())
        })
        .unwrap();
        assert_eq!(sched.next_ready(), None);
        assert_eq!(sched.get_instance("main").unwrap().next_run_at, Some(retry));

        assert!(sched.force_ready("main"));
        assert_eq!(sched.next_ready().as_deref(), Some("main"));
        drop(sched);
        crate::agents::init::update_schedule_and_reconcile(&state, "ws", false, |c| {
            c.schedule.paused = true;
            Ok(())
        })
        .await
        .unwrap();
        let _ = release_slot_on_error(&state.scheduler, "main", async {
            Err(RunnerError::NoProviderConfigured("main".into()))
        })
        .await;
        let paused_retry = {
            let mut sched = state.scheduler.lock().await;
            let retry = sched.get_instance("main").unwrap().next_run_at.unwrap();
            crate::agents::init::reconcile_deferred(&mut sched, |_| {
                workspace_config::load(&root).map_err(|e| e.to_string())
            })
            .unwrap();
            assert!(!sched.get_instance("main").unwrap().enabled);
            retry
        };
        crate::agents::init::update_schedule_and_reconcile(&state, "ws", false, |c| {
            c.schedule.paused = false;
            Ok(())
        })
        .await
        .unwrap();
        let mut sched = state.scheduler.lock().await;
        assert_eq!(sched.next_ready(), None);
        assert_eq!(
            sched.get_instance("main").unwrap().next_run_at,
            Some(paused_retry)
        );

        assert!(sched.force_ready("main"));
        assert_eq!(sched.next_ready().as_deref(), Some("main"));
        drop(sched);
        crate::agents::init::update_schedule_and_reconcile(&state, "ws", true, |c| {
            c.schedule.kind = ScheduleKind::Interval {
                interval_minutes: 2,
            };
            c.schedule.next_run_at_unix_ms = Some(chrono::Utc::now().timestamp_millis() + 120_000);
            Ok(())
        })
        .await
        .unwrap();
        let _ = release_slot_on_error(&state.scheduler, "main", async {
            Err(RunnerError::NoProviderConfigured("main".into()))
        })
        .await;
        let mut sched = state.scheduler.lock().await;
        let failed_retry = sched.get_instance("main").unwrap().next_run_at.unwrap();
        crate::agents::init::reconcile_deferred(&mut sched, |_| {
            workspace_config::load(&root).map_err(|e| e.to_string())
        })
        .unwrap();
        let replacement = sched.get_instance("main").unwrap().next_run_at.unwrap();
        assert!(replacement < failed_retry);
        assert_eq!(
            workspace_config::load(&root).unwrap().schedule.kind,
            ScheduleKind::Interval {
                interval_minutes: 2
            }
        );
    }

    #[tokio::test]
    async fn failed_start_releases_scheduler_slot() {
        let scheduler = crate::agents::create_shared_scheduler();
        let instance_id = {
            let mut sched = scheduler.lock().await;
            sched.register_definition(crate::agents::AgentDefinition::new("agent-1", "Agent"));
            sched.create_instance("agent-1").unwrap()
        };

        let claimed = claim_next_instance(&scheduler).await;
        assert_eq!(claimed.as_deref(), Some(instance_id.as_str()));

        let result = release_slot_on_error(&scheduler, &instance_id, async {
            Err(RunnerError::NoProviderConfigured("agent-1".to_string()))
        })
        .await;
        assert!(matches!(result, Err(RunnerError::NoProviderConfigured(_))));

        let mut sched = scheduler.lock().await;
        assert!(!sched.is_running());
        let instance = sched.get_instance(&instance_id).unwrap();
        assert!(!instance.is_running);
        // Retried later, not hot-looped on every tick.
        assert!(instance.next_run_at.is_some());
        assert_eq!(sched.next_ready(), None);

        sched.set_instance_next_run_at(&instance_id, None);
        assert_eq!(sched.next_ready(), Some(instance_id));
    }

    #[tokio::test]
    async fn start_for_missing_agent_keeps_instance_for_retry() {
        let scheduler = crate::agents::create_shared_scheduler();
        let instance_id = {
            let mut sched = scheduler.lock().await;
            sched.register_definition(crate::agents::AgentDefinition::new("agent-1", "Agent"));
            sched.create_instance("agent-1").unwrap()
        };
        claim_next_instance(&scheduler).await;

        let result = release_slot_on_error(&scheduler, &instance_id, async {
            Err(RunnerError::AgentNotFound("agent-1".to_string()))
        })
        .await;
        assert!(matches!(result, Err(RunnerError::AgentNotFound(_))));

        let sched = scheduler.lock().await;
        assert!(!sched.is_running());
        let instance = sched.get_instance(&instance_id).unwrap();
        assert!(instance.next_run_at.is_some());
    }

    // The scheduled agent is looked up in its own workspace only: another
    // workspace's unreadable config must not stop it from starting.
    #[test]
    fn scheduled_agent_resolves_despite_another_unreadable_workspace() {
        use crate::config::workspace_config::WorkspaceConfig;
        use crate::workspace_index::WorkspaceIndex;

        let temp = tempfile::tempdir().expect("temp dir");
        let parent = temp.path().join("workspaces");
        let mut index = WorkspaceIndex::default();
        let mut sched = crate::agents::scheduler::Scheduler::new();
        for (id, main, updated_at) in [
            ("ws-broken", "main-broken", 2_000),
            ("ws-ok", "main-ok", 1_000),
        ] {
            let root = parent.join(id);
            let mut config =
                WorkspaceConfig::new(id.to_string(), id.to_string(), updated_at, main.to_string());
            config.schedule.enabled = true;
            workspace_config::save(&root, &config).expect("seed workspace");
            index.insert_config(root, &config);
            crate::agents::init::apply_workspace_schedule(&mut sched, &config);
        }
        std::fs::write(
            workspace_config::config_path(&parent.join("ws-broken")),
            "{ not json",
        )
        .expect("corrupt config");
        let config_manager = crate::ConfigManager::new_for_tests(
            crate::config::AppConfig::default(),
            temp.path().join("config.json"),
        );
        let state = AppState::new_for_tests(config_manager, index).expect("app state");

        let instance = sched
            .get_instance(&crate::agents::types::AgentInstance::instance_id_for(
                "main-ok",
            ))
            .expect("healthy workspace is scheduled");
        let agent =
            load_workspace_agent_as_config(&state, &instance.workspace_id, &instance.agent_id)
                .expect("healthy workspace config loads")
                .expect("scheduled agent resolves");
        assert_eq!(agent.workspace_id, "ws-ok");
        assert!(agent.schedule_enabled);
    }

    mod sleep_preflight {
        use super::*;
        use crate::agents::{scheduler::Scheduler, AgentDefinition};
        use crate::assistant::types::{AuthMode, SessionContext};
        use crate::config::workspace_config::ScheduleKind;
        use crate::db::test_support::workspace_pool;
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        use std::time::{Duration, Instant};
        use tokio::sync::Mutex;

        fn connection() -> ProviderConnection {
            ProviderConnection {
                id: "conn".into(),
                name: "conn".into(),
                protocol_id: "openai".into(),
                provider_id: "openai".into(),
                auth_mode: AuthMode::DeveloperApiKey,
                base_url: None,
                secret_ref: String::new(),
                model_id: "model".into(),
                account_label: None,
                enabled: true,
                created_at: 0,
                updated_at: 0,
            }
        }

        async fn fixture(
            manual: bool,
        ) -> (
            tempfile::TempDir,
            DbPool,
            AssistantSession,
            AssistantRun,
            SharedScheduler,
            Option<crate::agents::scheduler::DispatchLease>,
        ) {
            let (tmp, pool) = workspace_pool().await;
            let session = repository::create_session(&pool, SessionContext::default())
                .await
                .unwrap();
            let mut scheduler = Scheduler::new();
            scheduler.register_definition(
                AgentDefinition::new("agent", "Agent").with_workspace_id("ws"),
            );
            let id = scheduler.create_instance("agent").unwrap();
            let start = Instant::now() - Duration::from_secs(30);
            scheduler
                .configure_enhanced(
                    &id,
                    ScheduleKind::EnhancedInterval {
                        interval_seconds: 15,
                    },
                    0,
                    start,
                )
                .unwrap();
            scheduler.set_wake_monitor_available(!manual, 0, start);
            if manual {
                assert!(scheduler.force_ready("agent"));
            }
            assert_eq!(
                scheduler.next_ready_at(15_000, start + Duration::from_secs(15)),
                Some(id.clone())
            );
            let lease = scheduler.take_claimed_dispatch_lease(&id);
            let run = start_tick_run(
                &pool,
                &session,
                &connection(),
                TickContext {
                    instance_id: &id,
                    agent_id: "agent",
                    manual,
                    enhanced: true,
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();
            (
                tmp,
                pool,
                session,
                run,
                Arc::new(Mutex::new(scheduler)),
                lease,
            )
        }

        #[tokio::test]
        async fn enhanced_workspaces_claim_independently_without_same_workspace_overlap() {
            let (_tmp, _pool, _session, _run, scheduler, first_lease) = fixture(false).await;
            let mut sched = scheduler.lock().await;
            let start = Instant::now() - Duration::from_secs(30);
            for (agent, workspace) in [("other", "other-ws"), ("same", "ws")] {
                sched.register_definition(
                    AgentDefinition::new(agent, agent).with_workspace_id(workspace),
                );
                sched.create_instance(agent).unwrap();
                sched
                    .configure_enhanced(
                        agent,
                        ScheduleKind::EnhancedInterval {
                            interval_seconds: 15,
                        },
                        0,
                        start,
                    )
                    .unwrap();
            }
            assert_eq!(
                sched.next_ready_at(30_000, Instant::now()).as_deref(),
                Some("other")
            );
            assert!(first_lease.unwrap().is_current());
            assert!(sched.next_ready_at(30_000, Instant::now()).is_none());
        }

        #[tokio::test]
        async fn runner_progresses_second_workspace_while_first_model_waits_and_releases_only_failed_claim(
        ) {
            let (_tmp, _pool, _session, _run, scheduler, _lease) = fixture(false).await;
            {
                let mut sched = scheduler.lock().await;
                sched.wait_for_session("agent");
                sched.register_definition(
                    AgentDefinition::new("other", "Other").with_workspace_id("other-ws"),
                );
                sched.create_instance("other").unwrap();
                sched
                    .configure_enhanced(
                        "other",
                        ScheduleKind::EnhancedInterval {
                            interval_seconds: 15,
                        },
                        0,
                        Instant::now() - Duration::from_secs(30),
                    )
                    .unwrap();
            }
            let held = Arc::new(tokio::sync::Notify::new());
            let second_finished = Arc::new(tokio::sync::Notify::new());
            let mut handles = dispatch_ready_agents(&scheduler, |id| {
                let held = held.clone();
                let second_finished = second_finished.clone();
                async move {
                    if id == "agent" {
                        held.notified().await;
                        Ok(())
                    } else {
                        second_finished.notify_one();
                        Err(RunnerError::NoProviderConfigured(id))
                    }
                }
            })
            .await;
            assert_eq!(handles.len(), 2);
            tokio::time::timeout(Duration::from_secs(2), second_finished.notified())
                .await
                .unwrap();
            // Await the failed attempt's own completion, rather than race its guard.
            tokio::time::timeout(Duration::from_secs(2), async {
                while !handles.iter().any(|handle| handle.is_finished()) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let index = handles
                .iter()
                .position(|handle| handle.is_finished())
                .unwrap();
            handles.swap_remove(index).await.unwrap();
            {
                let mut sched = scheduler.lock().await;
                assert!(sched.is_instance_running("agent"));
                assert!(!sched.is_instance_running("other"));
                assert!(
                    sched.next_ready().is_none(),
                    "failed start must wait for its next interval"
                );
            }
            held.notify_one();
            for handle in handles {
                handle.await.unwrap();
            }
        }

        #[tokio::test]
        async fn busy_enhanced_tick_retains_one_due_check_and_admits_it_after_manual_completion() {
            let (_tmp, pool, session, active, scheduler, _lease) = fixture(false).await;
            let original = repository::latest_run(&pool, &session.id)
                .await
                .unwrap()
                .unwrap();
            for _ in 0..3 {
                let run = start_tick_run(
                    &pool,
                    &session,
                    &connection(),
                    TickContext {
                        instance_id: "agent",
                        agent_id: "agent",
                        manual: false,
                        enhanced: true,
                    },
                    |_| {},
                )
                .await
                .unwrap();
                assert!(run.is_none());
                let mut sched = scheduler.lock().await;
                sched.wait_for_session("agent");
                assert!(sched.waiting_reason("agent").is_some());
                assert_eq!(sched.next_ready().as_deref(), Some("agent"));
            }
            assert_eq!(
                repository::latest_run(&pool, &session.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .id,
                original.id
            );
            repository::complete_run(&pool, &active.id, RunStatus::Completed, None)
                .await
                .unwrap();
            let next = start_tick_run(
                &pool,
                &session,
                &connection(),
                TickContext {
                    instance_id: "agent",
                    agent_id: "agent",
                    manual: false,
                    enhanced: true,
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();
            repository::complete_run(&pool, &next.id, RunStatus::Completed, None)
                .await
                .unwrap();
            let mut sched = scheduler.lock().await;
            sched.complete_agent("agent", true, 0);
            assert!(sched.waiting_reason("agent").is_none());
            assert!(sched.next_ready().is_none());
        }

        #[tokio::test]
        async fn user_message_committing_while_schedule_waits_for_sqlite_writer_wins_admission() {
            let (_tmp, pool, session, old, _scheduler, _lease) = fixture(false).await;
            repository::complete_run(&pool, &old.id, RunStatus::Completed, None)
                .await
                .unwrap();
            let message = repository::create_user_message(
                &pool,
                session.id.clone(),
                "Manual wins writer race".into(),
                None,
            )
            .await
            .unwrap();
            let mut tx = pool.begin().await.unwrap();
            crate::assistant::signals::insert_user_message_signal(&mut tx, &message, "conn")
                .await
                .unwrap();
            let schedule = tokio::spawn({
                let pool = pool.clone();
                let session = session.clone();
                async move {
                    start_tick_run(
                        &pool,
                        &session,
                        &connection(),
                        TickContext {
                            instance_id: "agent",
                            agent_id: "agent",
                            manual: false,
                            enhanced: true,
                        },
                        |_| {},
                    )
                    .await
                    .unwrap()
                }
            });
            tokio::task::yield_now().await;
            tx.commit().await.unwrap();
            assert!(schedule.await.unwrap().is_none());
            assert!(repository::active_run_id(&pool, &session.id)
                .await
                .unwrap()
                .is_none());
            assert!(
                matches!(dispatcher::claim(&pool, "ws", &session.id, |_| Some(connection())).await.unwrap(),
                dispatcher::Kick::Started { message_ids, .. } if message_ids == vec![message.id])
            );
        }

        #[tokio::test]
        async fn failed_manual_queue_check_cancels_the_unstarted_run_and_releases_its_claim() {
            let (_tmp, pool, session, run, scheduler, lease) = fixture(false).await;
            sqlx::query("DROP TABLE session_signals")
                .execute(&pool)
                .await
                .unwrap();
            let outcome = release_slot_on_error(&scheduler, "agent", async {
                run_queued_with_preflight(
                    &pool,
                    &session,
                    &run,
                    &scheduler,
                    "agent",
                    lease.as_ref(),
                    |_| {},
                    || async { panic!("queue check failure must not call the model") },
                )
                .await
                .map(|_| ())
            })
            .await;
            assert!(outcome.is_err());
            assert!(repository::active_run_id(&pool, &session.id)
                .await
                .unwrap()
                .is_none());
            assert!(!scheduler.lock().await.is_running());
        }

        #[tokio::test]
        async fn manual_priority_after_consumed_wake_or_clock_change_still_rearms_schedule() {
            for cron in [false, true] {
                let (_tmp, pool, session, run, scheduler, mut lease) = fixture(false).await;
                if cron {
                    let mut sched = scheduler.lock().await;
                    sched.wait_for_session("agent");
                    let start = Instant::now() - Duration::from_secs(60);
                    sched
                        .configure_enhanced(
                            "agent",
                            ScheduleKind::EnhancedCron {
                                expression: "* * * * *".into(),
                                timezone: "UTC".into(),
                            },
                            0,
                            start,
                        )
                        .unwrap();
                    assert_eq!(
                        sched
                            .next_ready_at(60_000, start + Duration::from_secs(60))
                            .as_deref(),
                        Some("agent")
                    );
                    lease = sched.take_claimed_dispatch_lease("agent");
                }
                let signal = scheduler.lock().await.wake_signal();
                if cron {
                    signal.clock_changed();
                } else {
                    signal.will_sleep();
                    signal.did_wake();
                }
                // Another workspace's polling pass consumes the event while
                // this claim is still active, so its timing has not been reset.
                assert!(scheduler.lock().await.next_ready().is_none());
                repository::create_user_message(
                    &pool,
                    session.id.clone(),
                    "Manual first".into(),
                    Some("conn"),
                )
                .await
                .unwrap();
                let before_cancel_wall = chrono::Utc::now().timestamp_millis();
                let outcome = run_queued_with_preflight(
                    &pool,
                    &session,
                    &run,
                    &scheduler,
                    "agent",
                    lease.as_ref(),
                    |_| {},
                    || async { panic!("manual input must displace the stale scheduled attempt") },
                )
                .await
                .unwrap();
                assert!(matches!(
                    outcome,
                    StartOutcome::AbandonedOnSleepOrClockChange
                ));
                let manual = dispatcher::claim(&pool, "ws", &session.id, |_| Some(connection()))
                    .await
                    .unwrap();
                let dispatcher::Kick::Started { run: manual, .. } = manual else {
                    panic!("manual request must run first")
                };
                repository::complete_run(&pool, &manual.id, RunStatus::Completed, None)
                    .await
                    .unwrap();
                let mut sched = scheduler.lock().await;
                let after = Instant::now();
                assert!(sched.next_ready_at(before_cancel_wall, after).is_none());
                if !cron {
                    assert!(sched
                        .next_ready_at(before_cancel_wall + 14_000, after + Duration::from_secs(14))
                        .is_none());
                    assert_eq!(
                        sched
                            .next_ready_at(
                                before_cancel_wall + 16_000,
                                after + Duration::from_secs(16)
                            )
                            .as_deref(),
                        Some("agent")
                    );
                }
            }
        }

        #[tokio::test]
        async fn manual_message_after_session_claim_cancels_before_model_and_keeps_check_due() {
            let (_tmp, pool, session, run, scheduler, lease) = fixture(false).await;
            repository::create_user_message(
                &pool,
                session.id.clone(),
                "Manual first".into(),
                Some("conn"),
            )
            .await
            .unwrap();
            let calls = AtomicUsize::new(0);
            let outcome = run_queued_with_preflight(
                &pool,
                &session,
                &run,
                &scheduler,
                "agent",
                lease.as_ref(),
                |_| {},
                || async {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
            .await
            .unwrap();
            assert!(matches!(
                outcome,
                StartOutcome::AbandonedOnSleepOrClockChange
            ));
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            let mut sched = scheduler.lock().await;
            assert!(!sched.is_running());
            assert!(sched.waiting_reason("agent").is_some());
            assert_eq!(sched.next_ready().as_deref(), Some("agent"));
            assert_eq!(
                repository::get_run(&pool, &run.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .status,
                RunStatus::Cancelled
            );
        }

        #[tokio::test]
        async fn disable_pause_and_manual_priority_revoke_only_uncommitted_attempts() {
            for action in ["disable", "pause", "manual", "remove"] {
                for committed in [false, true] {
                    let (_tmp, _pool, _session, _run, scheduler, lease) = fixture(false).await;
                    let lease = lease.unwrap();
                    if committed {
                        assert!(lease.try_commit());
                    }
                    let mut sched = scheduler.lock().await;
                    match action {
                        "disable" => sched.set_instance_enabled("agent", false),
                        "pause" => sched.pause("Paused by user".into()),
                        "manual" => sched.yield_to_manual("ws"),
                        "remove" => {
                            sched.remove_workspace("ws");
                        }
                        _ => unreachable!(),
                    }
                    assert_eq!(lease.try_commit(), committed, "action {action}");
                }
            }
        }

        #[tokio::test]
        async fn queued_user_request_wins_over_due_scheduled_turn() {
            let (_tmp, pool, session, previous, _scheduler, _lease) = fixture(false).await;
            repository::complete_run(&pool, &previous.id, RunStatus::Completed, None)
                .await
                .unwrap();
            let message = repository::create_user_message_with_content(
                &pool,
                session.id.clone(),
                vec![crate::assistant::types::ContentPart::Text {
                    text: "Please do this first".into(),
                }],
                Some("conn"),
            )
            .await
            .unwrap();
            let run = start_tick_run(
                &pool,
                &session,
                &connection(),
                TickContext {
                    instance_id: "agent",
                    agent_id: "agent",
                    manual: false,
                    enhanced: true,
                },
                |_| {},
            )
            .await
            .unwrap();
            assert!(
                run.is_none(),
                "scheduled work must leave pending manual input for dispatcher"
            );
            let outcome = dispatcher::claim(&pool, "ws", &session.id, |_| Some(connection()))
                .await
                .unwrap();
            assert!(
                matches!(outcome, dispatcher::Kick::Started { message_ids, .. } if message_ids == vec![message.id])
            );
        }

        #[tokio::test]
        async fn abandoned_scheduled_run_cancels_releases_and_rearms_without_provider_call() {
            let (_tmp, pool, session, run, scheduler, lease) = fixture(false).await;
            let signal = scheduler.lock().await.wake_signal();
            let barrier = Arc::new(tokio::sync::Barrier::new(2));
            let peer = barrier.clone();
            let notifications = tokio::spawn(async move {
                peer.wait().await;
                signal.will_sleep();
                signal.did_wake();
                peer.wait().await;
            });
            barrier.wait().await;
            barrier.wait().await;
            notifications.await.unwrap();
            // An intervening poll may consume the wake while the run is still claimed.
            assert_eq!(scheduler.lock().await.next_ready(), None);
            let calls = AtomicUsize::new(0);
            let mut events = Vec::new();
            let outcome = run_queued_with_preflight(
                &pool,
                &session,
                &run,
                &scheduler,
                "agent",
                lease.as_ref(),
                |e| events.push(e),
                || async {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
            .await
            .unwrap();
            assert!(matches!(
                outcome,
                StartOutcome::AbandonedOnSleepOrClockChange
            ));
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert_eq!(
                repository::get_run(&pool, &run.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .status,
                RunStatus::Cancelled
            );
            assert!(repository::active_run_id(&pool, &session.id)
                .await
                .unwrap()
                .is_none());
            assert!(matches!(
                events.as_slice(),
                [AssistantUiEvent::RunCancelled { .. }]
            ));
            let mut sched = scheduler.lock().await;
            assert!(!sched.is_running());
            let after = Instant::now();
            assert_eq!(
                sched.next_ready_at(0, after + Duration::from_secs(14)),
                None
            );
            assert_eq!(
                sched
                    .next_ready_at(0, after + Duration::from_secs(16))
                    .as_deref(),
                Some("agent")
            );
        }

        async fn preparation_message(
            pool: &DbPool,
            session: &AssistantSession,
            run: &AssistantRun,
            source: &str,
            role: crate::assistant::types::MessageRole,
            text: &str,
        ) -> crate::assistant::types::AssistantMessage {
            repository::create_message(
                pool,
                repository::CreateMessageParams {
                    session_id: session.id.clone(),
                    role,
                    content: vec![crate::assistant::types::ContentPart::Text { text: text.into() }],
                    provider_metadata: Some(serde_json::json!({"source": source})),
                    run_id: Some(run.id.clone()),
                },
            )
            .await
            .unwrap()
        }

        #[tokio::test]
        async fn api_abandonment_removes_boundary_before_the_next_manual_turn() {
            use crate::assistant::{compaction_service, types::MessageRole};
            let (_tmp, pool, session, run, scheduler, lease) = fixture(false).await;
            let user = repository::create_user_message(
                &pool,
                session.id.clone(),
                "manual input".into(),
                Some("conn"),
            )
            .await
            .unwrap();
            let outcome = run_queued_with_preflight(
                &pool,
                &session,
                &run,
                &scheduler,
                "agent",
                lease.as_ref(),
                |_| {},
                || async {
                    preparation_message(
                        &pool,
                        &session,
                        &run,
                        "api-trigger",
                        MessageRole::User,
                        "abandoned scheduled instruction",
                    )
                    .await;
                    Err(RunnerError::ScheduledDispatchAbandoned)
                },
            )
            .await
            .unwrap();
            assert!(matches!(
                outcome,
                StartOutcome::AbandonedOnSleepOrClockChange
            ));
            let next = compaction_service::load_with_pending(&pool, &session.id)
                .await
                .unwrap();
            assert_eq!(
                next.messages().len(),
                1,
                "the abandoned API boundary must not reach the next model"
            );
            assert_eq!(next.messages()[0].id, user.id);
            assert_eq!(
                crate::assistant::signals::list_open_content(&pool, &session.id)
                    .await
                    .unwrap()[0]
                    .message
                    .id,
                user.id
            );
            assert!(repository::active_run_id(&pool, &session.id)
                .await
                .unwrap()
                .is_none());
        }

        async fn abandoned_cli_start(
            pool: &DbPool,
            session: &AssistantSession,
            run: &AssistantRun,
        ) -> RunnerError {
            use crate::assistant::types::MessageRole;
            preparation_message(
                pool,
                session,
                run,
                "claude-code-trigger",
                MessageRole::User,
                "abandoned schedule marker",
            )
            .await;
            preparation_message(
                pool,
                session,
                run,
                "claude-code",
                MessageRole::Assistant,
                "",
            )
            .await;
            let mut prepared = session.clone();
            prepared.context.cli_session_id = Some("unlaunched-session".into());
            repository::update_session(pool, &prepared).await.unwrap();
            RunnerError::ScheduledPreparationAbandoned(
                crate::assistant::run_lifecycle::CliSessionBeforeStart {
                    id: session.context.cli_session_id.clone(),
                    provider: session.context.cli_session_provider.clone(),
                },
            )
        }

        #[tokio::test]
        #[allow(
            clippy::too_many_lines,
            reason = "One recovery scenario verifies cleanup ownership, rollback and pending input together"
        )]
        async fn failed_cli_preparation_cleanup_blocks_manual_work_until_retry_succeeds() {
            use crate::assistant::compaction_service;
            let (_tmp, pool, mut session, mut run, scheduler, lease) = fixture(false).await;
            run.protocol_id = "claude-code".into();
            sqlx::query("UPDATE assistant_runs SET protocol_id = 'claude-code' WHERE id = ?")
                .bind(&run.id)
                .execute(&pool)
                .await
                .unwrap();
            session.context.cli_session_id = Some("original-session".into());
            session.context.cli_session_provider = Some("claude-code".into());
            session = repository::update_session(&pool, &session).await.unwrap();
            let mut user = None;
            sqlx::query("CREATE TRIGGER reject_preparation_delete BEFORE DELETE ON assistant_messages WHEN json_extract(OLD.provider_metadata_json, '$.source') = 'claude-code-trigger' BEGIN SELECT RAISE(ABORT, 'injected preparation cleanup failure'); END").execute(&pool).await.unwrap();
            let result = run_queued_with_preflight(
                &pool,
                &session,
                &run,
                &scheduler,
                "agent",
                lease.as_ref(),
                |_| {},
                || async {
                    user = Some(
                        repository::create_user_message(
                            &pool,
                            session.id.clone(),
                            "queued manual input".into(),
                            Some("conn"),
                        )
                        .await
                        .unwrap(),
                    );
                    Err(abandoned_cli_start(&pool, &session, &run).await)
                },
            )
            .await;
            let user = user.unwrap();
            assert!(
                result.is_err(),
                "cleanup failure must not become successful abandonment"
            );
            assert!(
                repository::active_run_id(&pool, &session.id)
                    .await
                    .unwrap()
                    .is_some(),
                "manual dispatch must stay blocked"
            );
            assert!(scheduler
                .lock()
                .await
                .pending_cancellation_reason("ws")
                .is_some());
            repository::complete_run(&pool, &run.id, RunStatus::Cancelled, None)
                .await
                .unwrap();
            assert!(
                matches!(dispatcher::defer_for_cleanup(&*scheduler.lock().await, "ws", &session.id),
                Some(dispatcher::Kick::Busy { run_id }) if run_id == run.id)
            );
            assert!(
                retry_abandoned_cancellation(&pool, &scheduler, "ws", &run.id, |_| {})
                    .await
                    .is_err()
            );
            sqlx::query("DROP TRIGGER reject_preparation_delete")
                .execute(&pool)
                .await
                .unwrap();
            retry_abandoned_cancellation(&pool, &scheduler, "ws", &run.id, |_| {})
                .await
                .unwrap();
            assert!(repository::active_run_id(&pool, &session.id)
                .await
                .unwrap()
                .is_none());
            assert!(scheduler
                .lock()
                .await
                .pending_cancellation_reason("ws")
                .is_none());
            assert!(
                dispatcher::defer_for_cleanup(&*scheduler.lock().await, "ws", &session.id)
                    .is_none()
            );
            let restored = repository::get_session(&pool, &session.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                restored.context.cli_session_id.as_deref(),
                Some("original-session")
            );
            let next = compaction_service::load_with_pending(&pool, &session.id)
                .await
                .unwrap();
            assert_eq!(next.messages().len(), 1);
            assert_eq!(next.messages()[0].id, user.id);
            assert_eq!(
                crate::assistant::signals::list_open_content(&pool, &session.id)
                    .await
                    .unwrap()[0]
                    .message
                    .id,
                user.id
            );
        }

        #[tokio::test]
        async fn committed_engine_attempt_finishes_after_sleep_without_cancellation() {
            let (_tmp, pool, session, run, scheduler, lease) = fixture(false).await;
            let signal = scheduler.lock().await.wake_signal();
            let outcome = run_queued_with_preflight(
                &pool,
                &session,
                &run,
                &scheduler,
                "agent",
                lease.as_ref(),
                |_| {},
                || async {
                    repository::update_run_status(&pool, &run.id, RunStatus::Running, None)
                        .await
                        .map_err(RunnerError::AssistantPersistence)?;
                    crate::assistant::provider_dispatch::commit_provider_dispatch()
                        .map_err(|_| RunnerError::ScheduledDispatchAbandoned)?;
                    signal.will_sleep();
                    tokio::task::yield_now().await;
                    // A later operation belongs to the same active attempt.
                    crate::assistant::provider_dispatch::commit_provider_dispatch()
                        .map_err(|_| RunnerError::ScheduledDispatchAbandoned)?;
                    repository::complete_run(&pool, &run.id, RunStatus::Completed, None)
                        .await
                        .map_err(RunnerError::AssistantPersistence)?;
                    Ok(())
                },
            )
            .await
            .unwrap();
            assert!(matches!(outcome, StartOutcome::Finished(Ok(()))));
            assert_eq!(
                repository::get_run(&pool, &run.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .status,
                RunStatus::Completed
            );
        }

        #[tokio::test]
        async fn sleep_after_engine_entry_cancels_and_rearms_without_dispatch() {
            let (_tmp, pool, session, run, scheduler, lease) = fixture(false).await;
            let signal = scheduler.lock().await.wake_signal();
            let calls = AtomicUsize::new(0);
            let outcome = run_queued_with_preflight(
                &pool,
                &session,
                &run,
                &scheduler,
                "agent",
                lease.as_ref(),
                |_| {},
                || async {
                    tokio::task::yield_now().await;
                    signal.will_sleep();
                    signal.did_wake();
                    crate::assistant::provider_dispatch::commit_provider_dispatch()
                        .map_err(|_| RunnerError::ScheduledDispatchAbandoned)?;
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
            .await
            .unwrap();
            assert!(matches!(
                outcome,
                StartOutcome::AbandonedOnSleepOrClockChange
            ));
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert_eq!(
                repository::get_run(&pool, &run.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .status,
                RunStatus::Cancelled
            );
            assert!(repository::active_run_id(&pool, &session.id)
                .await
                .unwrap()
                .is_none());
            let mut sched = scheduler.lock().await;
            assert!(!sched.is_running());
            let after = Instant::now();
            assert_eq!(
                sched.next_ready_at(0, after + Duration::from_secs(14)),
                None
            );
            assert_eq!(
                sched
                    .next_ready_at(0, after + Duration::from_secs(16))
                    .as_deref(),
                Some("agent")
            );
        }

        #[tokio::test]
        async fn failed_cancellation_stays_visible_and_recovers_without_blocking_the_scheduler() {
            let (_tmp, pool, session, run, scheduler, lease) = fixture(false).await;
            let signal = scheduler.lock().await.wake_signal();
            signal.will_sleep();
            signal.did_wake();
            sqlx::query("CREATE TRIGGER reject_cancel BEFORE UPDATE OF status ON assistant_runs WHEN NEW.status = 'cancelled' BEGIN SELECT RAISE(ABORT, 'injected cancellation failure'); END")
                .execute(&pool).await.unwrap();
            let calls = AtomicUsize::new(0);
            let result = release_slot_on_error(&scheduler, "agent", async {
                run_queued_with_preflight(
                    &pool,
                    &session,
                    &run,
                    &scheduler,
                    "agent",
                    lease.as_ref(),
                    |_| {},
                    || async {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    },
                )
                .await
                .map(|_| ())
            })
            .await;
            assert!(result.is_err());
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert!(repository::active_run_id(&pool, &session.id)
                .await
                .unwrap()
                .is_some());
            {
                let mut sched = scheduler.lock().await;
                assert!(!sched.is_running());
                assert!(sched.pending_cancellation_reason("ws").is_some());
                assert_eq!(sched.next_ready(), None);
                sched.register_definition(
                    AgentDefinition::new("other", "Other").with_workspace_id("other-ws"),
                );
                let other = sched.create_instance("other").unwrap();
                assert_eq!(sched.next_ready(), Some(other.clone()));
                sched.complete_agent(&other, true, i64::MAX);
            }
            assert!(
                retry_abandoned_cancellation(&pool, &scheduler, "ws", &run.id, |_| {})
                    .await
                    .is_err()
            );
            assert!(scheduler
                .lock()
                .await
                .pending_cancellation_reason("ws")
                .is_some());
            sqlx::query("DROP TRIGGER reject_cancel")
                .execute(&pool)
                .await
                .unwrap();
            let mut events = Vec::new();
            retry_abandoned_cancellation(&pool, &scheduler, "ws", &run.id, |event| {
                events.push(event);
            })
            .await
            .unwrap();
            assert!(matches!(
                events.as_slice(),
                [AssistantUiEvent::RunCancelled { .. }]
            ));
            assert!(repository::active_run_id(&pool, &session.id)
                .await
                .unwrap()
                .is_none());
            assert!(scheduler
                .lock()
                .await
                .pending_cancellation_reason("ws")
                .is_none());
            let next = repository::create_run_if_idle(
                &pool,
                CreateRunParams {
                    session_id: session.id.clone(),
                    status: RunStatus::Queued,
                    trigger: RunTrigger::UserMessage,
                    connection_id: "conn".into(),
                    protocol_id: "openai".into(),
                    model_id: "model".into(),
                    error: None,
                },
            )
            .await
            .unwrap();
            assert!(next.is_some(), "a manual turn may start after cleanup");
        }

        #[tokio::test]
        async fn manual_run_is_not_cancelled_by_the_scheduled_sleep_guard() {
            let (_tmp, pool, session, run, scheduler, lease) = fixture(true).await;
            assert!(lease.is_none());
            scheduler.lock().await.wake_signal().will_sleep();
            let calls = AtomicUsize::new(0);
            let outcome = run_queued_with_preflight(
                &pool,
                &session,
                &run,
                &scheduler,
                "agent",
                lease.as_ref(),
                |_| {},
                || async {
                    crate::assistant::provider_dispatch::commit_provider_dispatch()
                        .map_err(|_| RunnerError::ScheduledDispatchAbandoned)?;
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
            .await
            .unwrap();
            assert!(matches!(outcome, StartOutcome::Finished(Ok(()))));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                repository::get_run(&pool, &run.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .status,
                RunStatus::Queued
            );
        }
    }

    mod busy_tick {
        use super::*;
        use crate::agents::scheduler::Scheduler;
        use crate::agents::AgentDefinition;
        use crate::assistant::types::{AuthMode, SessionContext};
        use crate::config::workspace_config::ScheduleKind;
        use crate::db::test_support::workspace_pool;
        use std::sync::Arc;
        use tokio::sync::Mutex;

        fn connection() -> ProviderConnection {
            ProviderConnection {
                id: "conn".into(),
                name: "conn".into(),
                protocol_id: "openai".into(),
                provider_id: "openai".into(),
                auth_mode: AuthMode::DeveloperApiKey,
                base_url: None,
                secret_ref: String::new(),
                model_id: "model".into(),
                account_label: None,
                enabled: true,
                created_at: 0,
                updated_at: 0,
            }
        }

        /// A claimed tick of a manager session that already has an active run.
        async fn busy_tick(
            manual: bool,
        ) -> (
            tempfile::TempDir,
            DbPool,
            AssistantSession,
            AssistantRun,
            SharedScheduler,
            String,
        ) {
            let (tmp, pool) = workspace_pool().await;
            let session = repository::create_session(&pool, SessionContext::default())
                .await
                .unwrap();
            let active = repository::create_run(
                &pool,
                CreateRunParams {
                    session_id: session.id.clone(),
                    status: RunStatus::Running,
                    trigger: RunTrigger::SignalWake,
                    connection_id: "conn".into(),
                    protocol_id: "openai".into(),
                    model_id: "model".into(),
                    error: None,
                },
            )
            .await
            .unwrap();
            let mut scheduler = Scheduler::new();
            scheduler.register_definition(AgentDefinition::new("agent", "Agent"));
            let instance_id = scheduler.create_instance("agent").unwrap();
            if manual {
                scheduler.set_instance_enabled(&instance_id, false);
                assert!(scheduler.force_ready("agent"));
            }
            assert_eq!(scheduler.next_ready(), Some(instance_id.clone()));
            (
                tmp,
                pool,
                session,
                active,
                Arc::new(Mutex::new(scheduler)),
                instance_id,
            )
        }

        struct Skipped {
            _tmp: tempfile::TempDir,
            pool: DbPool,
            active: AssistantRun,
            scheduler: SharedScheduler,
            instance_id: String,
            events: Vec<AssistantUiEvent>,
        }

        /// Runs a busy tick through the runner's start and completion steps.
        async fn skip(manual: bool) -> Skipped {
            let (tmp, pool, session, active, scheduler, instance_id) = busy_tick(manual).await;
            let mut events = Vec::new();
            let run = start_tick_run(
                &pool,
                &session,
                &connection(),
                TickContext {
                    instance_id: &instance_id,
                    agent_id: "agent",
                    manual,
                    enhanced: false,
                },
                |event| events.push(event),
            )
            .await
            .unwrap();
            assert!(run.is_none(), "a busy session must not get a second run");

            let mut agent = AgentConfig::new(
                "Agent".into(),
                String::new(),
                ScheduleKind::Interval {
                    interval_minutes: 30,
                },
            );
            agent.workspace_id = "ws".into();
            let before = chrono::Utc::now().timestamp_millis();
            let next =
                complete_tick_locked(&mut *scheduler.lock().await, &instance_id, true, &agent)
                    .unwrap();
            assert!(
                next > before,
                "the schedule must advance past the skipped tick"
            );
            Skipped {
                _tmp: tmp,
                pool,
                active,
                scheduler,
                instance_id,
                events,
            }
        }

        async fn counts(pool: &DbPool) -> (i64, i64) {
            sqlx::query_as(
                "SELECT (SELECT COUNT(*) FROM assistant_runs), (SELECT COUNT(*) FROM session_signals)",
            )
            .fetch_one(pool)
            .await
            .unwrap()
        }

        #[tokio::test]
        async fn scheduled_tick_skipped_when_session_run_active() {
            let tick = skip(false).await;

            assert_eq!(counts(&tick.pool).await, (1, 0), "no second run, no signal");
            assert!(tick.events.is_empty(), "a scheduled skip emits nothing");
            let sched = tick.scheduler.lock().await;
            assert!(!sched.is_running());
            let instance = sched.get_instance(&tick.instance_id).unwrap();
            assert!(instance.seconds_until_next_run() > 0);
        }

        #[tokio::test]
        async fn manual_run_now_on_busy_session_skips_clears_flag_and_emits_run_skipped() {
            let tick = skip(true).await;

            assert_eq!(counts(&tick.pool).await, (1, 0), "no second run, no signal");
            assert!(matches!(
                tick.events.as_slice(),
                [AssistantUiEvent::RunSkipped {
                    reason: RunSkipReason::SessionBusy,
                    active_run_id: Some(id),
                }] if *id == tick.active.id
            ));
            let sched = tick.scheduler.lock().await;
            assert!(!sched.is_running());
            assert!(
                !sched
                    .get_instance(&tick.instance_id)
                    .unwrap()
                    .manual_run_pending
            );
        }
    }
}
