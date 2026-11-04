//! Agent initialization.
//!
//! Agents are workspace-local (`workspace_agents` DB table). Scheduling for
//! them is populated by `populate_scheduler_from_workspace_agents` below,
//! which runs after the DB pool is ready.

use crate::agents::{AgentDefinition, SharedScheduler};
use crate::config::{workspace_config, ConfigManager, ExecutionCapabilityConfig, ShellAccessMode};
use crate::AppState;

/// No-op kept so the synchronous lib.rs setup path stays untouched. The real
/// scheduler population now happens in `populate_scheduler_from_workspace_agents`
/// once the DB pool is initialized.
pub fn initialize_scheduler(_scheduler: &SharedScheduler, _config_manager: &ConfigManager) {
    // intentionally empty
}

/// Main-thread startup boundary: availability follows observer registration.
pub fn initialize_wake_monitor(
    scheduler: &mut crate::agents::scheduler::Scheduler,
    register: impl FnOnce(crate::agents::scheduler::WakeSignal) -> Result<(), String>,
) -> Result<(), String> {
    let result = register(scheduler.wake_signal());
    scheduler.set_wake_monitor_available(
        result.is_ok(),
        chrono::Utc::now().timestamp_millis(),
        std::time::Instant::now(),
    );
    result
}

/// Populates the scheduler with scheduled workspace-local agents from workspace configs.
///
/// Called after workspace storage is initialized. Reads every workspace
/// `config.json`, registers scheduled agent definitions, and creates an
/// instance for each enabled agent.
pub async fn populate_scheduler_from_workspace_agents(
    scheduler: &SharedScheduler,
    state: &AppState,
) {
    let locators = match state.workspace_index.read() {
        Ok(index) => index.locators_sorted(),
        Err(e) => {
            tracing::warn!(
                "Failed to read workspace index for scheduler population: {}",
                e
            );
            return;
        }
    };

    let mut sched = scheduler.lock().await;
    for locator in locators {
        let Ok(config) = workspace_config::load(&locator.root_path) else {
            continue;
        };
        apply_workspace_schedule(&mut sched, &config);
    }

    // Restore a persisted global "pause all" overlay. This sits on top of the
    // per-workspace instance state seeded above: while paused the runner skips
    // every tick, and resuming leaves each workspace's individual pause state
    // (set via apply_workspace_schedule) intact.
    let globally_paused = state
        .config_manager
        .lock()
        .map(|cm| cm.get().scheduler_paused)
        .unwrap_or(false);
    if globally_paused {
        sched.pause("Paused by user".to_string());
    }
}

/// Reconcile the scheduler's view of a single workspace with its current
/// `WorkspaceConfig`. Removes any prior instance for the workspace's
/// default agent, then re-registers if `schedule.enabled`. Idempotent —
/// callable from startup (via `populate_scheduler_from_workspace_agents`)
/// and from the `workspace_set_schedule` / `workspace_set_schedule_paused`
/// Tauri commands so live state always tracks the file.
pub fn apply_workspace_schedule(
    sched: &mut crate::agents::scheduler::Scheduler,
    config: &crate::config::WorkspaceConfig,
) {
    super::cleanup_recovery::restore_runtime(sched, config);
    // The workspace schedule fires the workspace's own Main agent. A shared
    // definition never becomes a scheduler key: the same teammate assigned in
    // five workspaces would collide on one instance.
    let Some(agent) = config.main_agent.as_ref() else {
        return;
    };

    // Preserve the active slot; apply the latest saved settings after it exits.
    if sched.is_instance_running(&agent.id) {
        sched.defer_reconcile(&agent.id);
        return;
    }

    // Always start clean: drop any prior instance(s) for this agent. Cheap
    // when there is none; safe when there is one of either polarity.
    sched.remove_instances_for_agent(&agent.id);

    if !config.schedule.enabled {
        return;
    }

    let execution: ExecutionCapabilityConfig = agent.execution.clone();
    let mut tools: Vec<&'static str> = vec!["dashboard", "tabs"];
    if !matches!(execution.shell.mode, ShellAccessMode::Off) {
        tools.push("bash");
    }
    if execution.web.enabled {
        tools.push("web");
    }
    let definition = AgentDefinition::new(&agent.id, &agent.name)
        .with_workspace_id(&config.id)
        .with_tools(tools);
    sched.register_definition(definition);

    if agent.enabled {
        let instance_id = sched.create_instance(&agent.id);
        // Paused workspaces still get an instance (so pause/resume can flip
        // it without re-registering), but the instance starts disabled so
        // the runner skips it until resumed.
        if let Some(instance_id) = instance_id {
            // Seed the in-memory `next_run_at` from the persisted
            // wall-clock value. Without this, every scheduled workspace
            // would fire immediately on app start regardless of when it
            // was actually due — the `Instant` `next_run_at` doesn't
            // survive process restart.
            sched.set_instance_next_run_at(&instance_id, config.schedule.next_run_at_unix_ms);
            if matches!(
                config.schedule.kind,
                crate::config::workspace_config::ScheduleKind::EnhancedInterval { .. }
                    | crate::config::workspace_config::ScheduleKind::EnhancedCron { .. }
            ) {
                let configured = sched
                    .configure_enhanced(
                        &instance_id,
                        config.schedule.kind.clone(),
                        chrono::Utc::now().timestamp_millis(),
                        std::time::Instant::now(),
                    )
                    .and_then(|()| {
                        sched.configure_allowed_blocks(
                            &instance_id,
                            config.schedule.allowed_blocks.clone(),
                        )
                    });
                if let Err(error) = configured {
                    tracing::warn!(%error, "Unable to configure enhanced schedule");
                    sched.set_instance_enabled(&instance_id, false);
                }
            }
            if config.schedule.paused {
                sched.set_instance_enabled(&instance_id, false);
            }
        }
    }
}

/// Save and apply settings while holding the scheduler lock, preserving write order.
pub async fn update_schedule_and_reconcile(
    state: &AppState,
    workspace_id: &str,
    invalidates_completion: bool,
    mutate: impl FnOnce(&mut crate::config::WorkspaceConfig) -> Result<(), String>,
) -> Result<(), String> {
    let mut sched = state.scheduler.lock().await;
    let ((), config) = state.update_workspace_config(workspace_id, mutate)?;
    if invalidates_completion {
        sched.invalidate_pending_cancellation_rearm(workspace_id);
        sched.invalidate_running_completion(config.main_agent_id());
    }
    apply_with_legacy_deadline(&mut sched, &config, !invalidates_completion);
    Ok(())
}

fn apply_with_legacy_deadline(
    sched: &mut crate::agents::scheduler::Scheduler,
    config: &crate::config::WorkspaceConfig,
    preserve: bool,
) {
    use crate::config::workspace_config::ScheduleKind;
    let id = config.main_agent_id();
    let previous = if preserve
        && config.schedule.enabled
        && !sched.completion_is_invalidated(id)
        && matches!(
            config.schedule.kind,
            ScheduleKind::Interval { .. } | ScheduleKind::Cron { .. }
        ) {
        sched
            .get_instance(id)
            .filter(|instance| !instance.is_running)
            .map(|instance| instance.next_run_at)
    } else {
        None
    };
    apply_workspace_schedule(sched, config);
    if let Some(deadline) = previous {
        if let Some(instance) = sched.get_instance_mut(id) {
            instance.next_run_at = deadline;
        }
    }
}

/// Retry deferred settings after a run. Failed reads remain pending for the next poll.
/// Callers hold the scheduler lock while loading so settings writes cannot overtake it.
pub fn reconcile_deferred(
    sched: &mut crate::agents::scheduler::Scheduler,
    load: impl Fn(&str) -> Result<crate::config::WorkspaceConfig, String>,
) -> Result<(), String> {
    let mut first_error = None;
    for (_instance_id, workspace_id) in sched.deferred_reconciliations() {
        match load(&workspace_id) {
            Ok(config) => apply_with_legacy_deadline(sched, &config, true),
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// Clears all agent instances.
#[allow(dead_code)]
pub async fn clear_all_instances(scheduler: &SharedScheduler) {
    let mut scheduler = scheduler.lock().await;

    let instance_ids: Vec<String> = scheduler
        .all_instances()
        .map(|i| i.instance_id.clone())
        .collect();

    for instance_id in instance_ids {
        scheduler.remove_instance(&instance_id);
    }
}

/// Creates a scheduler instance for a specific agent.
#[allow(dead_code)]
pub async fn create_instance_for_agent(scheduler: &SharedScheduler, agent_id: &str) {
    let mut scheduler = scheduler.lock().await;
    scheduler.create_instance(agent_id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::create_shared_scheduler;
    use crate::agents::types::AgentInstance;

    fn create_test_agent_config() -> crate::config::AgentConfig {
        crate::config::AgentConfig::new(
            "Test Agent".to_string(),
            "Test description".to_string(),
            crate::config::workspace_config::ScheduleKind::Interval {
                interval_minutes: 5,
            },
        )
    }

    #[test]
    fn failed_deferred_load_is_retried_and_reenables_the_legacy_schedule() {
        use crate::config::workspace_config::{ScheduleKind, WorkspaceConfig};
        let mut config = WorkspaceConfig::new("ws".into(), "Workspace".into(), 0, "main".into());
        config.schedule.enabled = true;
        config.schedule.kind = ScheduleKind::Interval {
            interval_minutes: 1,
        };
        let mut sched = crate::agents::scheduler::Scheduler::new();
        apply_workspace_schedule(&mut sched, &config);
        let running = sched.next_ready().unwrap();
        apply_workspace_schedule(&mut sched, &config);
        sched.complete_agent(&running, true, i64::MAX);
        assert!(reconcile_deferred(&mut sched, |_| Err("temporary read failure".into())).is_err());
        assert!(sched.next_ready().is_none());
        reconcile_deferred(&mut sched, |_| Ok(config.clone())).unwrap();
        assert!(sched.get_instance(&running).unwrap().enabled);
        assert!(sched.deferred_reconciliations().is_empty());
    }

    #[test]
    fn changing_schedule_does_not_release_active_run() {
        use crate::config::workspace_config::{ScheduleKind, WorkspaceConfig};
        let mut config = WorkspaceConfig::new("ws".into(), "Workspace".into(), 0, "main".into());
        config.schedule.enabled = true;
        config.schedule.kind = ScheduleKind::Interval {
            interval_minutes: 1,
        };
        let mut scheduler = crate::agents::scheduler::Scheduler::new();
        apply_workspace_schedule(&mut scheduler, &config);
        let running = scheduler.next_ready().unwrap();
        config.schedule.kind = ScheduleKind::EnhancedInterval {
            interval_seconds: 15,
        };
        apply_workspace_schedule(&mut scheduler, &config);
        assert_eq!(
            scheduler.running_instance().map(String::as_str),
            Some(running.as_str())
        );
        assert!(scheduler.next_ready().is_none());
        scheduler.complete_agent(&running, true, i64::MAX);
        assert!(scheduler.take_deferred_reconcile(&running));
        apply_workspace_schedule(&mut scheduler, &config);
        assert!(scheduler.next_ready().is_none()); // Wake detection remains unavailable.
    }

    #[test]
    fn disabling_schedule_does_not_release_active_run() {
        use crate::config::workspace_config::{ScheduleKind, WorkspaceConfig};
        let mut config = WorkspaceConfig::new("ws".into(), "Workspace".into(), 0, "main".into());
        config.schedule.enabled = true;
        config.schedule.kind = ScheduleKind::Interval {
            interval_minutes: 1,
        };
        let mut scheduler = crate::agents::scheduler::Scheduler::new();
        apply_workspace_schedule(&mut scheduler, &config);
        let running = scheduler.next_ready().unwrap();
        config.schedule.enabled = false;
        apply_workspace_schedule(&mut scheduler, &config);
        assert_eq!(
            scheduler.running_instance().map(String::as_str),
            Some(running.as_str())
        );
        assert!(scheduler.next_ready().is_none());
    }

    #[test]
    fn startup_enables_enhanced_schedules_only_after_observer_registration_succeeds() {
        use crate::agents::scheduler::Scheduler;
        use crate::config::workspace_config::{ScheduleKind, WorkspaceConfig};
        use std::time::{Duration, Instant};
        for available in [true, false] {
            let mut config =
                WorkspaceConfig::new("ws".into(), "Workspace".into(), 0, "main".into());
            config.schedule.enabled = true;
            config.schedule.kind = ScheduleKind::EnhancedInterval {
                interval_seconds: 15,
            };
            let mut scheduler = Scheduler::new();
            apply_workspace_schedule(&mut scheduler, &config);
            let registration = initialize_wake_monitor(&mut scheduler, |_| {
                if available {
                    Ok(())
                } else {
                    Err("observer unavailable".into())
                }
            });
            assert_eq!(registration.is_ok(), available);
            assert_eq!(
                scheduler
                    .next_ready_at(16_000, Instant::now() + Duration::from_secs(16))
                    .is_some(),
                available
            );
        }
    }

    #[test]
    fn persisted_overdue_schedules_restart_with_enhanced_freshness_and_legacy_catchup() {
        use crate::agents::{schedule::compute_next_run_at, scheduler::Scheduler};
        use crate::config::workspace_config::{ScheduleKind, WorkspaceConfig};
        use chrono::Timelike;
        use std::time::{Duration, Instant};
        // Keep the cron occurrence safely ahead of test setup, including when
        // this test happens to begin near a minute/hour boundary.
        let cron_minute = (chrono::Utc::now().minute() + 2) % 60;
        for kind in [
            ScheduleKind::EnhancedInterval {
                interval_seconds: 15,
            },
            ScheduleKind::EnhancedCron {
                expression: format!("{cron_minute} * * * *"),
                timezone: "UTC".into(),
            },
            ScheduleKind::Interval {
                interval_minutes: 5,
            },
        ] {
            let directory = tempfile::tempdir().unwrap();
            let overdue = chrono::Utc::now().timestamp_millis() - 120_000;
            let mut saved = WorkspaceConfig::new(
                "restart-workspace".into(),
                "Restart".into(),
                0,
                "main".into(),
            );
            saved.schedule.enabled = true;
            saved.schedule.kind = kind.clone();
            saved.schedule.next_run_at_unix_ms = Some(overdue);
            workspace_config::save(directory.path(), &saved).unwrap();
            drop(saved);
            let loaded = workspace_config::load(directory.path()).unwrap();
            assert_eq!(loaded.schedule.kind, kind);
            assert_eq!(loaded.schedule.next_run_at_unix_ms, Some(overdue));
            let mut restarted = Scheduler::new();
            // Match app startup: register observers before loading schedules.
            let before_startup = Instant::now();
            initialize_wake_monitor(&mut restarted, |_| Ok(())).unwrap();
            apply_workspace_schedule(&mut restarted, &loaded);
            let after_startup = Instant::now();
            let wall = chrono::Utc::now().timestamp_millis();
            let first = restarted.next_ready();
            if matches!(kind, ScheduleKind::Interval { .. }) {
                assert_eq!(
                    first.as_deref(),
                    Some("main"),
                    "legacy overdue anchors retain ready-now behavior"
                );
                continue;
            }
            assert!(
                first.is_none(),
                "enhanced {kind:?} must not replay its persisted overdue anchor"
            );
            if matches!(kind, ScheduleKind::EnhancedInterval { .. }) {
                assert_eq!(
                    restarted
                        .next_ready_at(wall + 14_000, before_startup + Duration::from_secs(14)),
                    None
                );
                assert_eq!(
                    restarted
                        .next_ready_at(wall + 16_000, after_startup + Duration::from_secs(16))
                        .as_deref(),
                    Some("main")
                );
            } else {
                let future = compute_next_run_at(&kind, wall).unwrap();
                assert!(future > wall);
                assert_eq!(
                    restarted.next_ready_at(future - 1, after_startup + Duration::from_secs(1)),
                    None
                );
                assert_eq!(
                    restarted
                        .next_ready_at(future, after_startup + Duration::from_secs(2))
                        .as_deref(),
                    Some("main")
                );
            }
        }
    }

    #[test]
    fn enhanced_schedule_does_not_run_without_wake_detection() {
        use crate::config::workspace_config::{ScheduleKind, WorkspaceConfig};
        let mut config = WorkspaceConfig::new("ws".into(), "Workspace".into(), 0, "main".into());
        config.schedule.enabled = true;
        config.schedule.kind = ScheduleKind::EnhancedInterval {
            interval_seconds: 15,
        };
        let mut scheduler = crate::agents::scheduler::Scheduler::new();
        apply_workspace_schedule(&mut scheduler, &config);
        assert!(scheduler.next_ready().is_none());
    }

    #[tokio::test]
    async fn test_create_and_remove_instance_for_agent() {
        let scheduler = create_shared_scheduler();

        let agent = create_test_agent_config();
        {
            let mut s = scheduler.lock().await;
            s.register_definition(AgentDefinition::new(&agent.id, &agent.name));
        }

        create_instance_for_agent(&scheduler, &agent.id).await;

        {
            let s = scheduler.lock().await;
            assert_eq!(s.instance_count(), 1);
            let instance = s.get_instance(&AgentInstance::instance_id_for(&agent.id));
            assert!(instance.is_some());
        }

        {
            let mut s = scheduler.lock().await;
            s.remove_instances_for_agent(&agent.id);
        }

        {
            let s = scheduler.lock().await;
            assert_eq!(s.instance_count(), 0);
        }
    }

    #[tokio::test]
    async fn test_clear_all_instances() {
        let scheduler = create_shared_scheduler();

        let agent = create_test_agent_config();
        {
            let mut s = scheduler.lock().await;
            s.register_definition(AgentDefinition::new(&agent.id, &agent.name));
        }

        create_instance_for_agent(&scheduler, &agent.id).await;

        {
            let s = scheduler.lock().await;
            assert_eq!(s.instance_count(), 1);
        }

        clear_all_instances(&scheduler).await;

        {
            let s = scheduler.lock().await;
            assert_eq!(s.instance_count(), 0);
        }
    }
}
