//! Agent scheduler.
//!
//! A simple scheduler that manages agent instances and determines
//! when they should run. Enhanced workspaces progress independently.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                         SCHEDULER                               │
//! │                                                                 │
//! │  ┌──────────────┐    ┌──────────────┐    ┌──────────────┐     │
//! │  │  Definitions │    │  Instances   │    │  next_ready  │     │
//! │  │  (templates) │───▶│ (per space)  │───▶│  (find work) │     │
//! │  └──────────────┘    └──────────────┘    └──────────────┘     │
//! └─────────────────────────────────────────────────────────────────┘
//! ```

use std::collections::{HashMap, HashSet};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex as SyncMutex,
};
use std::time::Instant;

use tokio::sync::Mutex;

use super::types::{AgentDefinition, AgentInstance};

// =============================================================================
// Scheduler State
// =============================================================================

/// Current state of the scheduler.
#[derive(Debug, Clone, PartialEq)]
pub enum SchedulerState {
    /// Scheduler is running normally.
    Running,
    /// Scheduler is paused (e.g., no connectivity).
    Paused { reason: String },
    /// Scheduler is stopped.
    Stopped,
}

// =============================================================================
// Scheduler
// =============================================================================

/// Synchronous gate shared with the platform power notification callbacks.
#[derive(Clone, Default)]
pub struct WakeSignal(Arc<SyncMutex<WakeState>>);

#[derive(Default)]
struct WakeState {
    sleeping: bool,
    generation: u64,
    clock_generation: u64,
}

/// The wake generation captured by a scheduled claim.
#[derive(Clone)]
pub struct DispatchLease {
    signal: WakeSignal,
    generation: u64,
    cron_clock_generation: Option<u64>,
    committed: Arc<AtomicBool>,
    claim_valid: Arc<AtomicBool>,
    allowed_blocks: Option<super::weekly_blocks::WeeklyAllowedBlocks>,
}

impl DispatchLease {
    /// Commit the first provider operation while ordered with sleep and clock events.
    pub fn try_commit(&self) -> bool {
        self.try_commit_with_clock(|| chrono::Utc::now().timestamp_millis())
    }

    fn try_commit_with_clock(&self, clock: impl FnOnce() -> i64) -> bool {
        let Ok(state) = self.signal.0.lock() else {
            return false;
        };
        if self.committed.load(Ordering::Relaxed) {
            return true;
        }
        if !self.claim_valid.load(Ordering::Relaxed)
            || self
                .allowed_blocks
                .as_ref()
                .is_some_and(|blocks| !blocks.is_allowed(clock()))
            || state.sleeping
            || state.generation != self.generation
            || self
                .cron_clock_generation
                .is_some_and(|generation| state.clock_generation != generation)
        {
            return false;
        }
        self.committed.store(true, Ordering::Relaxed);
        true
    }

    /// Whether this claim still belongs to the current wake/cron-clock timing.
    /// Manual revocation does not itself discard a due check; a wake or clock
    /// change must rearm it even when manual input is also waiting.
    pub fn timing_is_current(&self) -> bool {
        self.signal.0.lock().is_ok_and(|state| {
            !state.sleeping
                && state.generation == self.generation
                && self
                    .cron_clock_generation
                    .is_none_or(|generation| state.clock_generation == generation)
        })
    }

    /// Whether wake, schedule changes or manual priority invalidated the claim.
    pub fn is_current(&self) -> bool {
        self.is_current_at(chrono::Utc::now().timestamp_millis())
    }

    pub(crate) fn is_current_at(&self, wall_ms: i64) -> bool {
        self.signal.0.lock().is_ok_and(|state| {
            self.claim_valid.load(Ordering::Relaxed)
                && self
                    .allowed_blocks
                    .as_ref()
                    .is_none_or(|blocks| blocks.is_allowed(wall_ms))
                && !state.sleeping
                && state.generation == self.generation
                && self
                    .cron_clock_generation
                    .is_none_or(|generation| state.clock_generation == generation)
        })
    }
}

impl WakeSignal {
    #[cfg(test)]
    pub fn dispatch_lease(&self) -> Option<DispatchLease> {
        let state = self.0.lock().ok()?;
        if state.sleeping {
            return None;
        }
        Some(DispatchLease {
            signal: self.clone(),
            generation: state.generation,
            cron_clock_generation: None,
            committed: Arc::new(AtomicBool::new(false)),
            claim_valid: Arc::new(AtomicBool::new(true)),
            allowed_blocks: None,
        })
    }

    pub fn clock_changed(&self) {
        if let Ok(mut state) = self.0.lock() {
            state.clock_generation = state.clock_generation.wrapping_add(1);
        }
    }

    pub fn will_sleep(&self) {
        if let Ok(mut state) = self.0.lock() {
            state.sleeping = true;
        }
    }

    pub fn did_wake(&self) {
        if let Ok(mut state) = self.0.lock() {
            state.generation = state.generation.wrapping_add(1);
            state.sleeping = false;
        }
    }
}

pub const PENDING_CANCELLATION_REASON: &str =
    "Scheduled run cleanup failed; messages wait while cleanup retries.";

#[derive(Clone, Debug)]
pub struct PendingCancellation {
    pub workspace_id: String,
    pub session_id: String,
    pub run_id: String,
    pub instance_id: String,
    pub previous_cli_session: Option<crate::assistant::run_lifecycle::CliSessionBeforeStart>,
    rearm: bool,
    /// Closing-only abandonment keeps one due check even if rollback finishes
    /// after the next opening. A real wake invalidates this retained check.
    preserve_due: bool,
}

/// The agent scheduler.
///
/// Manages agent definitions and instances. Finds the next agent
/// that is ready to run, with one claim per workspace.
pub struct Scheduler {
    pub(crate) scripts: HashMap<String, super::scripts::ScriptRuntime>,
    /// Current scheduler state.
    state: SchedulerState,

    /// Agent definitions (templates).
    definitions: HashMap<String, AgentDefinition>,

    /// Agent instances (per space/room).
    instances: HashMap<String, AgentInstance>,

    /// Claimed attempts, keyed by instance; admission excludes the owning workspace.
    running: HashSet<String>,
    waiting: HashSet<String>,
    enhanced: HashMap<String, super::schedule::EnhancedTiming>,
    wake_monitor_available: bool,
    wake_signal: WakeSignal,
    observed_wake_generation: u64,
    observed_clock_generation: u64,
    claimed_dispatch_leases: HashMap<String, DispatchLease>,
    pending_cancellations: HashMap<String, PendingCancellation>,
    pending_reconcile: HashSet<String>,
    invalidated_completions: HashSet<String>,
}

impl Scheduler {
    /// Creates a new scheduler.
    pub fn new() -> Self {
        Self {
            state: SchedulerState::Running,
            scripts: HashMap::new(),
            definitions: HashMap::new(),
            instances: HashMap::new(),
            running: HashSet::new(),
            waiting: HashSet::new(),
            enhanced: HashMap::new(),
            wake_monitor_available: false,
            wake_signal: WakeSignal::default(),
            observed_wake_generation: 0,
            observed_clock_generation: 0,
            claimed_dispatch_leases: HashMap::new(),
            pending_cancellations: HashMap::new(),
            pending_reconcile: HashSet::new(),
            invalidated_completions: HashSet::new(),
        }
    }

    // =========================================================================
    // Registration
    // =========================================================================

    /// Registers an agent definition.
    pub fn register_definition(&mut self, definition: AgentDefinition) {
        self.definitions.insert(definition.id.clone(), definition);
    }

    /// Gets an agent definition.
    pub fn get_definition(&self, agent_id: &str) -> Option<&AgentDefinition> {
        self.definitions.get(agent_id)
    }

    /// Creates and registers an instance of an agent.
    ///
    /// Returns `None` if the agent definition doesn't exist. An agent has at
    /// most one instance, so registering one twice keeps the first.
    pub fn create_instance(&mut self, agent_id: &str) -> Option<String> {
        let definition = self.definitions.get(agent_id)?;
        let instance = AgentInstance::new(definition);
        let instance_id = instance.instance_id.clone();

        // Don't overwrite existing instances
        if self.instances.contains_key(&instance_id) {
            return Some(instance_id);
        }

        self.instances.insert(instance_id.clone(), instance);

        Some(instance_id)
    }

    /// Removes an agent instance.
    pub fn remove_instance(&mut self, instance_id: &str) {
        self.instances.remove(instance_id);
        self.enhanced.remove(instance_id);
        self.pending_reconcile.remove(instance_id);
        self.invalidated_completions.remove(instance_id);
        self.invalidate_claim(instance_id);
        self.running.remove(instance_id);
        self.waiting.remove(instance_id);
        self.claimed_dispatch_leases.remove(instance_id);
    }

    /// Removes all instances for a given agent ID.
    ///
    /// Returns the number of instances removed.
    pub fn remove_instances_for_agent(&mut self, agent_id: &str) -> usize {
        // Match on the instance's own `agent_id` rather than on the shape of
        // the key: this survives the key format changing, which the old
        // `"{agent_id}:"` prefix test did not.
        let to_remove: Vec<String> = self
            .instances
            .values()
            .filter(|instance| instance.agent_id == agent_id)
            .map(|instance| instance.instance_id.clone())
            .collect();

        let count = to_remove.len();
        for id in to_remove {
            self.remove_instance(&id);
        }
        count
    }

    /// Removes every definition and instance registered for `workspace_id`.
    /// Returns the number of instances removed.
    pub fn remove_workspace(&mut self, workspace_id: &str) -> usize {
        self.pending_cancellations.remove(workspace_id);
        self.definitions
            .retain(|_, definition| definition.workspace_id != workspace_id);
        let to_remove: Vec<String> = self
            .instances
            .values()
            .filter(|instance| instance.workspace_id == workspace_id)
            .map(|instance| instance.instance_id.clone())
            .collect();
        for id in &to_remove {
            self.remove_instance(id);
        }
        to_remove.len()
    }

    // =========================================================================
    // Scheduling
    // =========================================================================

    /// Gets the next agent that is ready to run.
    ///
    /// Returns `None` if:
    /// - Scheduler is paused/stopped
    /// - A claim already owns this workspace (or legacy admission is busy)
    /// - No agents are ready
    pub fn next_ready(&mut self) -> Option<String> {
        self.next_ready_with_clock(|| (chrono::Utc::now().timestamp_millis(), Instant::now()))
    }

    #[cfg(test)]
    pub(crate) fn next_ready_at(&mut self, wall_ms: i64, now: Instant) -> Option<String> {
        self.next_ready_with_clock(|| (wall_ms, now))
    }

    fn next_ready_with_clock(&mut self, clock: impl FnOnce() -> (i64, Instant)) -> Option<String> {
        // Sample after acquiring the notification gate so an event cannot be
        // consumed using a time observed before that event.
        let signal = self.wake_signal.clone();
        let guard = signal.0.lock();
        let (wall_ms, now) = clock();
        let (sleeping, generation, clock_generation) = match &guard {
            Ok(state) => (state.sleeping, state.generation, state.clock_generation),
            Err(_) => (
                true,
                self.observed_wake_generation,
                self.observed_clock_generation,
            ),
        };
        if !sleeping && generation != self.observed_wake_generation {
            self.observed_wake_generation = generation;
            self.note_wake_at(wall_ms, now);
        }
        if !sleeping && clock_generation != self.observed_clock_generation {
            self.observed_clock_generation = clock_generation;
            for (id, timing) in &mut self.enhanced {
                if self
                    .instances
                    .get(id)
                    .is_some_and(|instance| !instance.is_running)
                    && timing.clock_changed(wall_ms, now).is_err()
                {
                    self.wake_monitor_available = false;
                }
            }
        }
        // Check scheduler state
        if self.state != SchedulerState::Running {
            return None;
        }

        // Legacy schedules retain their global admission gate. An enhanced
        // workspace can progress independently, but never beside its own claim.
        let busy_workspaces: HashSet<_> = self
            .running
            .iter()
            .filter_map(|id| self.instances.get(id).map(|i| i.workspace_id.clone()))
            .collect();
        // Find first ready instance
        for instance in self.instances.values_mut() {
            if self
                .scripts
                .get(&instance.workspace_id)
                .is_some_and(|script| script.running)
                || busy_workspaces.contains(&instance.workspace_id)
                || (!self.enhanced.contains_key(&instance.instance_id) && !self.running.is_empty())
            {
                continue;
            }
            if self
                .pending_cancellations
                .contains_key(&instance.workspace_id)
            {
                continue;
            }
            let ready = if let Some(timing) = self.enhanced.get(&instance.instance_id) {
                !instance.is_running
                    && (instance.manual_run_pending
                        || (instance.enabled
                            && self.wake_monitor_available
                            && !sleeping
                            && timing.is_due(wall_ms, now)))
            } else {
                instance.is_ready(now)
            };
            if ready {
                if self.enhanced.contains_key(&instance.instance_id) && !instance.manual_run_pending
                {
                    self.claimed_dispatch_leases.insert(
                        instance.instance_id.clone(),
                        DispatchLease {
                            signal: signal.clone(),
                            generation,
                            cron_clock_generation: self
                                .enhanced
                                .get(&instance.instance_id)
                                .filter(|timing| timing.is_cron())
                                .map(|_| clock_generation),
                            committed: Arc::new(AtomicBool::new(false)),
                            claim_valid: Arc::new(AtomicBool::new(true)),
                            allowed_blocks: self
                                .enhanced
                                .get(&instance.instance_id)
                                .and_then(|timing| timing.allowed_blocks()),
                        },
                    );
                }
                self.waiting.remove(&instance.instance_id);
                instance.is_running = true;
                self.running.insert(instance.instance_id.clone());
                return Some(instance.instance_id.clone());
            }
        }

        None
    }

    pub fn take_claimed_dispatch_lease(&self, instance_id: &str) -> Option<DispatchLease> {
        self.claimed_dispatch_leases.get(instance_id).cloned()
    }

    fn invalidate_claim(&self, instance_id: &str) {
        if let Some(lease) = self.claimed_dispatch_leases.get(instance_id) {
            // Order revocation with provider commitment, exactly as wake events.
            if let Ok(_gate) = self.wake_signal.0.lock() {
                lease.claim_valid.store(false, Ordering::Relaxed);
            }
        }
    }

    pub fn is_instance_running(&self, instance_id: &str) -> bool {
        self.running.contains(instance_id)
    }

    pub fn is_enhanced(&self, instance_id: &str) -> bool {
        self.enhanced.contains_key(instance_id)
    }

    /// Keep one due attempt rather than advancing its completion-based cadence.
    pub fn wait_for_session(&mut self, instance_id: &str) {
        self.running.remove(instance_id);
        self.claimed_dispatch_leases.remove(instance_id);
        if let Some(instance) = self.instances.get_mut(instance_id) {
            instance.is_running = false;
            if instance.enabled {
                self.waiting.insert(instance_id.to_string());
            }
        }
    }

    pub fn waiting_reason(&self, instance_id: &str) -> Option<&'static str> {
        if self.state != SchedulerState::Running {
            return None;
        }
        self.instances
            .get(instance_id)
            .and_then(|i| self.script_notice(&i.workspace_id))
            .or_else(|| self.allowed_block_waiting_reason(instance_id))
            .or_else(|| {
                (self.instances.get(instance_id).is_some_and(|i| i.enabled)
                    && self.waiting.contains(instance_id))
                .then_some("Waiting for the workspace to become available.")
            })
    }

    pub fn script_status(&self, workspace_id: &str) -> super::scripts::ScriptStatus {
        self.scripts
            .get(workspace_id)
            .map(|s| s.status())
            .unwrap_or_default()
    }

    pub fn script_notice(&self, workspace_id: &str) -> Option<&'static str> {
        let script = self.scripts.get(workspace_id)?;
        if script.pending.is_some() {
            Some("Waiting for condition script approval in schedule settings.")
        } else if script.running {
            Some("Running condition script.")
        } else if script.error.is_some() {
            Some("Condition script failed; see schedule settings. The next check will retry.")
        } else {
            None
        }
    }

    pub fn script_is_running(&self, workspace_id: &str) -> bool {
        self.scripts.get(workspace_id).is_some_and(|s| s.running)
    }

    pub fn workspace_has_scheduled_claim(&self, workspace_id: &str) -> bool {
        self.running.iter().any(|id| {
            self.instances
                .get(id)
                .is_some_and(|i| i.workspace_id == workspace_id)
        })
    }

    /// Future script/approval phases use the same revocation boundary.
    pub fn yield_to_manual(&self, workspace_id: &str) {
        for id in &self.running {
            if self
                .instances
                .get(id)
                .is_some_and(|i| i.workspace_id == workspace_id)
            {
                self.invalidate_claim(id);
            }
        }
    }

    /// Drop an invalidated scheduled attempt without recording its occurrence
    /// as completed. If wake was already observed, rearm from the current time.
    pub fn abandon_enhanced_claim(&mut self, instance_id: &str) {
        self.abandon_enhanced_claim_with_clock(instance_id, || {
            (chrono::Utc::now().timestamp_millis(), Instant::now())
        });
    }

    fn abandon_enhanced_claim_with_clock(
        &mut self,
        instance_id: &str,
        clock: impl FnOnce() -> (i64, Instant),
    ) -> bool {
        if !self.running.contains(instance_id) {
            return false;
        }
        let timing_invalidated = self
            .claimed_dispatch_leases
            .get(instance_id)
            .is_some_and(|lease| !lease.timing_is_current());
        if let Some(instance) = self.instances.get_mut(instance_id) {
            instance.is_running = false;
            instance.manual_run_pending = false;
        }
        self.running.remove(instance_id);
        self.claimed_dispatch_leases.remove(instance_id);
        self.waiting.remove(instance_id);
        let mut preserve_due = false;
        let signal = self.wake_signal.clone();
        match signal.0.lock() {
            Ok(state) if !state.sleeping => {
                let (wall_ms, now) = clock();
                if state.generation != self.observed_wake_generation {
                    // Consume this wake for every idle workspace now, rather
                    // than resetting this attempt a second time at the next poll.
                    self.observed_wake_generation = state.generation;
                    self.note_wake_at(wall_ms, now);
                } else if let Some(timing) = self.enhanced.get_mut(instance_id) {
                    preserve_due = !timing_invalidated && timing.waiting_for_block(wall_ms, now);
                    if !preserve_due && timing.wake(wall_ms, now).is_err() {
                        self.wake_monitor_available = false;
                    }
                }
            }
            Ok(_) => {}
            Err(_) => self.wake_monitor_available = false,
        };
        preserve_due
    }

    pub fn retain_failed_cancellation(
        &mut self,
        instance_id: &str,
        session_id: &str,
        run_id: &str,
        previous_cli_session: Option<crate::assistant::run_lifecycle::CliSessionBeforeStart>,
    ) {
        self.retain_failed_cancellation_with_clock(
            instance_id,
            session_id,
            run_id,
            previous_cli_session,
            || (chrono::Utc::now().timestamp_millis(), Instant::now()),
        );
    }

    fn retain_failed_cancellation_with_clock(
        &mut self,
        instance_id: &str,
        session_id: &str,
        run_id: &str,
        previous_cli_session: Option<crate::assistant::run_lifecycle::CliSessionBeforeStart>,
        clock: impl FnOnce() -> (i64, Instant),
    ) {
        let Some(instance) = self.instances.get(instance_id) else {
            return;
        };
        let workspace_id = instance.workspace_id.clone();
        let rearm = !self.completion_is_invalidated(instance_id);
        let preserve_due = self.abandon_enhanced_claim_with_clock(instance_id, clock);
        self.pending_cancellations.insert(
            workspace_id.clone(),
            PendingCancellation {
                workspace_id,
                session_id: session_id.to_string(),
                run_id: run_id.to_string(),
                instance_id: instance_id.to_string(),
                previous_cli_session,
                rearm,
                preserve_due,
            },
        );
    }

    pub fn pending_cancellation_reason(&self, workspace_id: &str) -> Option<&'static str> {
        self.pending_cancellations
            .contains_key(workspace_id)
            .then_some(PENDING_CANCELLATION_REASON)
    }

    pub fn pending_cancellations(&self) -> Vec<PendingCancellation> {
        self.pending_cancellations.values().cloned().collect()
    }

    pub fn invalidate_pending_cancellation_rearm(&mut self, workspace_id: &str) {
        if let Some(pending) = self.pending_cancellations.get_mut(workspace_id) {
            pending.rearm = false;
        }
    }

    pub fn finish_pending_cancellation(&mut self, workspace_id: &str, run_id: &str) {
        self.finish_pending_cancellation_with_clock(workspace_id, run_id, || {
            (chrono::Utc::now().timestamp_millis(), Instant::now())
        });
    }

    fn finish_pending_cancellation_with_clock(
        &mut self,
        workspace_id: &str,
        run_id: &str,
        clock: impl FnOnce() -> (i64, Instant),
    ) {
        let Some(pending) = self.pending_cancellations.get(workspace_id) else {
            return;
        };
        if pending.run_id != run_id {
            return;
        }
        let pending = self.pending_cancellations.remove(workspace_id).unwrap();
        if pending.rearm
            && self
                .instances
                .get(&pending.instance_id)
                .is_some_and(|i| !i.is_running)
        {
            let signal = self.wake_signal.clone();
            match signal.0.lock() {
                Ok(state) if !state.sleeping => {
                    let (wall_ms, now) = clock();
                    if state.generation != self.observed_wake_generation {
                        self.observed_wake_generation = state.generation;
                        self.note_wake_at(wall_ms, now);
                    } else if !pending.preserve_due {
                        if let Some(timing) = self.enhanced.get_mut(&pending.instance_id) {
                            if timing.wake(wall_ms, now).is_err() {
                                self.wake_monitor_available = false;
                            }
                        }
                    }
                }
                Ok(_) => {}
                Err(_) => self.wake_monitor_available = false,
            };
        }
    }

    pub fn defer_reconcile(&mut self, instance_id: &str) {
        self.pending_reconcile.insert(instance_id.to_string());
        self.set_instance_enabled(instance_id, false);
    }

    pub fn invalidate_running_completion(&mut self, agent_id: &str) {
        if self.running.contains(agent_id) {
            self.invalidated_completions.insert(agent_id.to_string());
        }
    }

    pub fn completion_is_invalidated(&self, instance_id: &str) -> bool {
        self.invalidated_completions.contains(instance_id)
    }

    pub fn deferred_reconciliations(&self) -> Vec<(String, String)> {
        self.pending_reconcile
            .iter()
            .filter_map(|id| {
                self.instances
                    .get(id)
                    .filter(|instance| !instance.is_running)
                    .map(|instance| (id.clone(), instance.workspace_id.clone()))
            })
            .collect()
    }

    pub fn take_deferred_reconcile(&mut self, instance_id: &str) -> bool {
        self.pending_reconcile.remove(instance_id)
    }

    pub fn wake_signal(&self) -> WakeSignal {
        self.wake_signal.clone()
    }

    pub fn configure_enhanced(
        &mut self,
        instance_id: &str,
        kind: crate::config::workspace_config::ScheduleKind,
        wall_ms: i64,
        now: Instant,
    ) -> Result<(), String> {
        let timing = super::schedule::EnhancedTiming::new(kind, wall_ms, now)?;
        self.enhanced.insert(instance_id.to_string(), timing);
        Ok(())
    }

    pub fn configure_allowed_blocks(
        &mut self,
        instance_id: &str,
        blocks: Option<super::weekly_blocks::WeeklyAllowedBlocks>,
    ) -> Result<(), String> {
        if let Some(timing) = self.enhanced.get_mut(instance_id) {
            timing.set_allowed_blocks(blocks)?;
        }
        Ok(())
    }

    pub fn allowed_block_waiting_reason(&self, instance_id: &str) -> Option<&'static str> {
        self.instances
            .get(instance_id)
            .filter(|instance| instance.enabled && !instance.is_running)?;
        self.enhanced
            .get(instance_id)
            .is_some_and(|timing| {
                timing.waiting_for_block(chrono::Utc::now().timestamp_millis(), Instant::now())
            })
            .then_some("Waiting for an allowed block.")
    }

    /// Countdown for the deadline that actually controls this instance.
    pub fn seconds_until_next_run(&self, instance_id: &str) -> Option<u64> {
        self.seconds_until_next_run_at(
            instance_id,
            chrono::Utc::now().timestamp_millis(),
            Instant::now(),
        )
    }

    fn seconds_until_next_run_at(
        &self,
        instance_id: &str,
        wall_ms: i64,
        now: Instant,
    ) -> Option<u64> {
        self.instances.get(instance_id).map(|instance| {
            if let Some(timing) = self.enhanced.get(instance_id) {
                return timing.seconds_until_next_run(wall_ms, now);
            }
            instance
                .next_run_at
                .map(|deadline| deadline.saturating_duration_since(now).as_secs())
                .unwrap_or(0)
        })
    }

    pub fn wake_monitor_available(&self) -> bool {
        self.wake_monitor_available
    }

    pub fn set_wake_monitor_available(&mut self, available: bool, wall_ms: i64, now: Instant) {
        self.wake_monitor_available = available;
        if available {
            self.note_wake_at(wall_ms, now);
        }
    }

    pub fn note_wake_at(&mut self, wall_ms: i64, now: Instant) {
        for pending in self.pending_cancellations.values_mut() {
            pending.preserve_due = false;
        }
        for (id, timing) in &mut self.enhanced {
            // A running attempt retains the occurrence it claimed. Completion
            // rearms it from the later of completion and that occurrence.
            if self
                .instances
                .get(id)
                .is_some_and(|instance| !instance.is_running)
                && timing.wake(wall_ms, now).is_err()
            {
                self.wake_monitor_available = false;
            }
        }
    }

    fn complete_enhanced_at(&mut self, instance_id: &str, wall_ms: i64, now: Instant) {
        if let Some(timing) = self.enhanced.get_mut(instance_id) {
            if timing.complete(wall_ms, now).is_err() {
                self.wake_monitor_available = false;
            }
        }
        if let Some(instance) = self.instances.get_mut(instance_id) {
            instance.is_running = false;
            instance.manual_run_pending = false;
        }
        self.running.remove(instance_id);
        self.waiting.remove(instance_id);
        self.claimed_dispatch_leases.remove(instance_id);
    }

    /// Marks an agent as completed and schedules its next run.
    ///
    /// # Arguments
    ///
    /// * `instance_id` - The instance to mark complete
    /// * `success` - Whether the run was successful
    /// * `next_run_at_unix_ms` - Wall-clock target for the next fire,
    ///   computed by `agents::schedule::compute_next_run_at` against the
    ///   workspace's current `ScheduleKind`. The scheduler translates
    ///   this into an `Instant` via `set_instance_next_run_at`.
    pub fn complete_agent(&mut self, instance_id: &str, success: bool, next_run_at_unix_ms: i64) {
        if self.enhanced.contains_key(instance_id) {
            self.complete_enhanced_at(
                instance_id,
                chrono::Utc::now().timestamp_millis(),
                Instant::now(),
            );
            return;
        }
        if let Some(instance) = self.instances.get_mut(instance_id) {
            instance.is_running = false;
            // Clear the one-shot manual-run flag: a paused instance that
            // ran via `force_ready` should drop back to paused, not keep
            // ticking.
            instance.manual_run_pending = false;
            if !success {
                // Could track consecutive failures here if needed
            }
        }

        // Seed the in-memory next_run_at from the wall-clock target so
        // both interval and cron modes share one code path. Past
        // targets clear next_run_at to "ready-now", matching the
        // catch-up semantic.
        self.set_instance_next_run_at(instance_id, Some(next_run_at_unix_ms));

        self.invalidate_claim(instance_id);
        self.running.remove(instance_id);
        self.waiting.remove(instance_id);
        self.claimed_dispatch_leases.remove(instance_id);
    }

    // =========================================================================
    // State Management
    // =========================================================================

    /// Pauses the scheduler.
    pub fn pause(&mut self, reason: String) {
        for id in &self.running {
            self.invalidate_claim(id);
        }
        self.waiting.clear();
        self.state = SchedulerState::Paused { reason };
    }

    /// Resumes the scheduler.
    pub fn resume(&mut self) {
        self.state = SchedulerState::Running;
    }

    /// Stops the scheduler.
    pub fn stop(&mut self) {
        for id in &self.running {
            self.invalidate_claim(id);
        }
        self.waiting.clear();
        self.state = SchedulerState::Stopped;
    }

    /// Gets the current scheduler state.
    pub fn state(&self) -> &SchedulerState {
        &self.state
    }

    /// Returns whether an agent is currently running.
    pub fn is_running(&self) -> bool {
        !self.running.is_empty()
    }

    /// Gets one running instance ID for diagnostics and legacy callers.
    pub fn running_instance(&self) -> Option<&String> {
        self.running.iter().next()
    }

    // =========================================================================
    // Instance Access
    // =========================================================================

    /// Gets a reference to an agent instance.
    pub fn get_instance(&self, instance_id: &str) -> Option<&AgentInstance> {
        self.instances.get(instance_id)
    }

    /// Gets a mutable reference to an agent instance.
    pub fn get_instance_mut(&mut self, instance_id: &str) -> Option<&mut AgentInstance> {
        self.instances.get_mut(instance_id)
    }

    /// Forces an agent to be ready for immediate execution by clearing its
    /// next_run_at. The runner loop will pick it up on its next tick.
    /// Returns true if the instance was found and updated.
    ///
    /// Works on disabled (paused) instances too — sets `manual_run_pending`
    /// so the next `is_ready` check passes once. Cleared by
    /// `complete_agent` so a paused schedule resumes its pause after the
    /// one-shot manual run, instead of going back to ticking on its own.
    pub fn force_ready(&mut self, agent_id: &str) -> bool {
        for instance in self.instances.values_mut() {
            if instance.agent_id == agent_id && !instance.is_running {
                instance.next_run_at = None;
                instance.manual_run_pending = true;
                return true;
            }
        }
        false
    }

    /// Gets the count of all registered instances.
    pub fn instance_count(&self) -> usize {
        self.instances.len()
    }

    /// Gets all instances.
    pub fn all_instances(&self) -> impl Iterator<Item = &AgentInstance> {
        self.instances.values()
    }

    /// Enables or disables an agent instance.
    pub fn set_instance_enabled(&mut self, instance_id: &str, enabled: bool) {
        if !enabled {
            self.invalidate_claim(instance_id);
            self.waiting.remove(instance_id);
        }
        if let Some(instance) = self.instances.get_mut(instance_id) {
            instance.enabled = enabled;
        }
    }

    /// Seeds the instance's in-memory `next_run_at` from a persisted
    /// wall-clock target. Used at startup (and on schedule reconcile) so
    /// a previously-scheduled workspace continues from where it left off
    /// instead of firing immediately on app launch.
    ///
    /// `target_unix_ms = None` clears `next_run_at` (instance becomes
    /// ready-now, matching the "fresh schedule, no anchor" semantic).
    /// A past target also clears `next_run_at` so the instance catches
    /// up on the next tick — useful when the app was down across one or
    /// more intervals.
    pub fn set_instance_next_run_at(&mut self, instance_id: &str, target_unix_ms: Option<i64>) {
        let Some(instance) = self.instances.get_mut(instance_id) else {
            return;
        };
        let Some(target) = target_unix_ms else {
            instance.next_run_at = None;
            return;
        };
        let now_ms = chrono::Utc::now().timestamp_millis();
        if target <= now_ms {
            // Past target — let the next tick pick it up.
            instance.next_run_at = None;
            return;
        }
        let remaining = (target - now_ms) as u64;
        instance.next_run_at = Some(Instant::now() + std::time::Duration::from_millis(remaining));
    }
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}

/// Thread-safe scheduler wrapped in Arc<Mutex>.
pub type SharedScheduler = Arc<Mutex<Scheduler>>;

/// Creates a new shared scheduler.
pub fn create_shared_scheduler() -> SharedScheduler {
    Arc::new(Mutex::new(Scheduler::new()))
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_definition() -> AgentDefinition {
        definition_named("test-agent")
    }

    fn definition_named(id: &str) -> AgentDefinition {
        AgentDefinition::new(id, "Test Agent").with_description("A test agent")
    }

    #[test]
    fn weekly_blocks_cleanup_recovery_preserves_due_unless_real_wake_intervenes() {
        use crate::config::workspace_config::ScheduleKind;
        use std::time::Duration;
        for (recovery_seconds, wake_before_recovery, poll_wake) in [
            (4560, false, false),
            (7260, false, false),
            (4560, true, false),
            (4560, true, true),
        ] {
            let start = Instant::now();
            let wall = chrono::DateTime::parse_from_rfc3339("2026-11-02T08:00:00Z")
                .unwrap()
                .timestamp_millis();
            let mut scheduler = Scheduler::new();
            scheduler.register_definition(
                AgentDefinition::new("agent", "Agent").with_workspace_id("ws"),
            );
            let id = scheduler.create_instance("agent").unwrap();
            scheduler
                .configure_enhanced(
                    &id,
                    ScheduleKind::EnhancedCron {
                        expression: "0 9 * * *".into(),
                        timezone: "UTC".into(),
                    },
                    wall,
                    start,
                )
                .unwrap();
            scheduler
                .configure_allowed_blocks(
                    &id,
                    Some(super::super::weekly_blocks::WeeklyAllowedBlocks {
                        timezone: "UTC".into(),
                        slots: vec![36, 40],
                    }),
                )
                .unwrap();
            scheduler.set_wake_monitor_available(true, wall, start);
            assert_eq!(
                scheduler.next_ready_at(wall + 3_600_000, start + Duration::from_secs(3600)),
                Some(id.clone())
            );
            scheduler.retain_failed_cancellation_with_clock(&id, "session", "run", None, || {
                (wall + 4_500_000, start + Duration::from_secs(4500))
            });
            assert!(scheduler.pending_cancellation_reason("ws").is_some());
            if wake_before_recovery {
                scheduler.wake_signal().will_sleep();
                scheduler.wake_signal().did_wake();
                if poll_wake {
                    assert_eq!(
                        scheduler
                            .next_ready_at(wall + 4_550_000, start + Duration::from_secs(4550)),
                        None
                    );
                }
            }
            scheduler.finish_pending_cancellation_with_clock("ws", "run", || {
                (
                    wall + recovery_seconds as i64 * 1000,
                    start + Duration::from_secs(recovery_seconds),
                )
            });
            let dispatch_seconds = recovery_seconds.max(7200);
            assert_eq!(
                scheduler.next_ready_at(
                    wall + dispatch_seconds as i64 * 1000,
                    start + Duration::from_secs(dispatch_seconds)
                ),
                if wake_before_recovery { None } else { Some(id) }
            );
        }
    }

    #[test]
    fn weekly_blocks_keep_claim_that_expires_at_closing_pending_until_next_opening() {
        use crate::config::workspace_config::ScheduleKind;
        use std::time::Duration;
        for wake_while_claimed in [false, true] {
            let start = Instant::now();
            let wall = chrono::DateTime::parse_from_rfc3339("2026-11-02T08:00:00Z")
                .unwrap()
                .timestamp_millis();
            let mut scheduler = Scheduler::new();
            scheduler.register_definition(create_test_definition());
            let id = scheduler.create_instance("test-agent").unwrap();
            scheduler
                .configure_enhanced(
                    &id,
                    ScheduleKind::EnhancedCron {
                        expression: "0 9 * * *".into(),
                        timezone: "UTC".into(),
                    },
                    wall,
                    start,
                )
                .unwrap();
            scheduler
                .configure_allowed_blocks(
                    &id,
                    Some(super::super::weekly_blocks::WeeklyAllowedBlocks {
                        timezone: "UTC".into(),
                        slots: vec![36, 40],
                    }),
                )
                .unwrap();
            scheduler.set_wake_monitor_available(true, wall, start);
            assert_eq!(
                scheduler.next_ready_at(wall + 3_600_000, start + Duration::from_secs(3600)),
                Some(id.clone())
            );
            if wake_while_claimed {
                scheduler.wake_signal().will_sleep();
                scheduler.wake_signal().did_wake();
                assert!(scheduler
                    .next_ready_at(wall + 4_500_000, start + Duration::from_secs(4500))
                    .is_none());
            }
            scheduler.abandon_enhanced_claim_with_clock(&id, || {
                (wall + 4_500_000, start + Duration::from_secs(4500))
            });
            assert_eq!(
                scheduler.next_ready_at(wall + 4_500_000, start + Duration::from_secs(4500)),
                None
            );
            assert_eq!(
                scheduler.next_ready_at(wall + 7_200_000, start + Duration::from_secs(7200)),
                if wake_while_claimed { None } else { Some(id) }
            );
        }
    }

    #[test]
    fn weekly_blocks_rechecked_at_commit_but_committed_work_can_finish() {
        let signal = WakeSignal::default();
        let mut lease = signal.dispatch_lease().unwrap();
        lease.allowed_blocks = Some(super::super::weekly_blocks::WeeklyAllowedBlocks {
            timezone: "UTC".into(),
            slots: vec![36],
        });
        let open = chrono::DateTime::parse_from_rfc3339("2026-11-02T09:00:00Z")
            .unwrap()
            .timestamp_millis();
        let closed = open + 15 * 60_000;
        assert!(!lease.try_commit_with_clock(|| closed));
        assert!(lease.try_commit_with_clock(|| open));
        assert!(lease.try_commit_with_clock(|| closed));
    }

    #[test]
    fn cancellation_recovery_does_not_rearm_a_replacement_schedule() {
        use crate::config::workspace_config::ScheduleKind;
        use std::time::{Duration, Instant};
        let start = Instant::now() - Duration::from_secs(100);
        let mut scheduler = Scheduler::new();
        scheduler
            .register_definition(AgentDefinition::new("agent", "Agent").with_workspace_id("ws"));
        let id = scheduler.create_instance("agent").unwrap();
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
        scheduler.set_wake_monitor_available(true, 0, start);
        assert_eq!(
            scheduler.next_ready_at(15_000, start + Duration::from_secs(15)),
            Some(id.clone())
        );
        scheduler.retain_failed_cancellation(&id, "session", "run", None);
        scheduler.invalidate_pending_cancellation_rearm("ws");
        scheduler.remove_instance(&id);
        scheduler.create_instance("agent").unwrap();
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
        assert_eq!(
            scheduler.next_ready_at(60_000, start + Duration::from_secs(60)),
            None
        );
        scheduler.finish_pending_cancellation("ws", "run");
        assert_eq!(
            scheduler.next_ready_at(60_000, start + Duration::from_secs(60)),
            Some(id)
        );
    }

    #[test]
    fn clock_change_invalidates_claimed_cron_but_not_interval() {
        use crate::config::workspace_config::ScheduleKind;
        use std::time::{Duration, Instant};
        let start = Instant::now();
        let mut cron = Scheduler::new();
        cron.register_definition(create_test_definition());
        let id = cron.create_instance("test-agent").unwrap();
        cron.configure_enhanced(
            &id,
            ScheduleKind::EnhancedCron {
                expression: "* * * * *".into(),
                timezone: "UTC".into(),
            },
            0,
            start,
        )
        .unwrap();
        cron.set_wake_monitor_available(true, 0, start);
        assert_eq!(
            cron.next_ready_at(60_000, start + Duration::from_secs(60)),
            Some(id.clone())
        );
        let lease = cron.take_claimed_dispatch_lease(&id).unwrap();
        cron.wake_signal().clock_changed();
        assert!(!lease.is_current());
        // A poll can consume the event while the claimed attempt is still active.
        assert_eq!(
            cron.next_ready_at(120_000, start + Duration::from_secs(61)),
            None
        );
        cron.abandon_enhanced_claim_with_clock(&id, || (120_000, start + Duration::from_secs(61)));
        assert_eq!(
            cron.next_ready_at(120_000, start + Duration::from_secs(61)),
            None
        );
        assert_eq!(
            cron.next_ready_at(180_000, start + Duration::from_secs(121)),
            Some(id)
        );

        let mut interval = Scheduler::new();
        interval.register_definition(create_test_definition());
        let id = interval.create_instance("test-agent").unwrap();
        interval
            .configure_enhanced(
                &id,
                ScheduleKind::EnhancedInterval {
                    interval_seconds: 15,
                },
                0,
                start,
            )
            .unwrap();
        interval.set_wake_monitor_available(true, 0, start);
        assert_eq!(
            interval.next_ready_at(15_000, start + Duration::from_secs(15)),
            Some(id.clone())
        );
        let lease = interval.take_claimed_dispatch_lease(&id).unwrap();
        interval.wake_signal().clock_changed();
        assert!(lease.is_current());
    }

    #[test]
    fn clock_is_sampled_after_pending_wake_is_synchronized() {
        use crate::config::workspace_config::ScheduleKind;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};
        let start = Instant::now();
        let mut scheduler = Scheduler::new();
        scheduler.register_definition(create_test_definition());
        let id = scheduler.create_instance("test-agent").unwrap();
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
        scheduler.set_wake_monitor_available(true, 0, start);
        let signal = scheduler.wake_signal();
        let mut wake = signal.0.lock().unwrap();
        let sampled = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let (tx, rx) = std::sync::mpsc::channel();
            let scheduler_ref = &mut scheduler;
            let sampled_ref = &sampled;
            let signal_probe = signal.clone();
            let thread = scope.spawn(move || {
                tx.send(()).unwrap();
                assert_eq!(
                    scheduler_ref.next_ready_with_clock(|| {
                        assert!(matches!(
                            signal_probe.0.try_lock(),
                            Err(std::sync::TryLockError::WouldBlock)
                        ));
                        sampled_ref.store(true, Ordering::SeqCst);
                        (60_000, start + Duration::from_secs(60))
                    }),
                    None
                );
            });
            rx.recv().unwrap();
            assert!(!sampled.load(Ordering::SeqCst));
            wake.generation += 1;
            drop(wake);
            thread.join().unwrap();
        });
        assert_eq!(
            scheduler.next_ready_at(74_000, start + Duration::from_secs(74)),
            None
        );
        assert_eq!(
            scheduler.next_ready_at(75_000, start + Duration::from_secs(75)),
            Some(id)
        );
    }

    #[test]
    fn wake_during_running_cron_does_not_skip_the_next_occurrence() {
        use crate::config::workspace_config::ScheduleKind;
        use std::time::{Duration, Instant};
        let start = Instant::now();
        let mut scheduler = Scheduler::new();
        scheduler.register_definition(create_test_definition());
        let id = scheduler.create_instance("test-agent").unwrap();
        scheduler
            .configure_enhanced(
                &id,
                ScheduleKind::EnhancedCron {
                    expression: "* * * * *".into(),
                    timezone: "UTC".into(),
                },
                0,
                start,
            )
            .unwrap();
        scheduler.set_wake_monitor_available(true, 0, start);
        assert_eq!(
            scheduler.next_ready_at(60_000, start + Duration::from_secs(60)),
            Some(id.clone())
        );
        scheduler.wake_signal().will_sleep();
        scheduler.wake_signal().did_wake();
        assert_eq!(
            scheduler.next_ready_at(120_000, start + Duration::from_secs(120)),
            None
        );
        scheduler.complete_enhanced_at(&id, 120_000, start + Duration::from_secs(120));
        assert_eq!(
            scheduler.next_ready_at(180_000, start + Duration::from_secs(180)),
            Some(id)
        );
    }

    #[test]
    fn enhanced_status_countdown_tracks_completion_and_wake_deadlines() {
        use crate::config::workspace_config::ScheduleKind;
        use std::time::Duration;
        let start = Instant::now();
        for (kind, first_due, completion, after_completion, wake, after_wake) in [
            (
                ScheduleKind::EnhancedInterval {
                    interval_seconds: 15,
                },
                15,
                20,
                15,
                100,
                15,
            ),
            (
                ScheduleKind::EnhancedCron {
                    expression: "* * * * *".into(),
                    timezone: "UTC".into(),
                },
                60,
                90,
                30,
                180,
                60,
            ),
        ] {
            let mut scheduler = Scheduler::new();
            scheduler.register_definition(create_test_definition());
            let id = scheduler.create_instance("test-agent").unwrap();
            scheduler.configure_enhanced(&id, kind, 0, start).unwrap();
            scheduler.set_wake_monitor_available(true, 0, start);
            // Deliberately leave the legacy timer at the first deadline.
            scheduler.instances.get_mut(&id).unwrap().next_run_at =
                Some(start + Duration::from_secs(first_due));
            assert_eq!(
                scheduler.seconds_until_next_run_at(&id, 0, start),
                Some(first_due)
            );
            assert_eq!(
                scheduler.next_ready_at(
                    first_due as i64 * 1000,
                    start + Duration::from_secs(first_due)
                ),
                Some(id.clone())
            );
            scheduler.complete_enhanced_at(
                &id,
                completion as i64 * 1000,
                start + Duration::from_secs(completion),
            );
            assert_eq!(
                scheduler.seconds_until_next_run_at(
                    &id,
                    completion as i64 * 1000,
                    start + Duration::from_secs(completion)
                ),
                Some(after_completion)
            );
            scheduler.note_wake_at(wake as i64 * 1000, start + Duration::from_secs(wake));
            assert_eq!(
                scheduler.seconds_until_next_run_at(
                    &id,
                    wake as i64 * 1000,
                    start + Duration::from_secs(wake)
                ),
                Some(after_wake)
            );
            assert_eq!(
                scheduler.seconds_until_next_run_at(
                    &id,
                    (wake + 5) as i64 * 1000,
                    start + Duration::from_secs(wake + 5)
                ),
                Some(after_wake - 5)
            );
        }
    }

    #[test]
    fn legacy_status_countdown_retains_its_elapsed_timer() {
        use std::time::Duration;
        let start = Instant::now();
        let mut scheduler = Scheduler::new();
        scheduler.register_definition(create_test_definition());
        let id = scheduler.create_instance("test-agent").unwrap();
        assert_eq!(
            scheduler.seconds_until_next_run_at("missing", 0, start),
            None
        );
        assert_eq!(scheduler.seconds_until_next_run_at(&id, 0, start), Some(0));
        scheduler.instances.get_mut(&id).unwrap().next_run_at =
            Some(start + Duration::from_secs(45));
        assert_eq!(
            scheduler.seconds_until_next_run_at(&id, i64::MAX, start + Duration::from_secs(15)),
            Some(30)
        );
        assert_eq!(
            scheduler.seconds_until_next_run_at(&id, 0, start + Duration::from_secs(50)),
            Some(0)
        );
    }

    #[test]
    fn clock_rollback_does_not_replay_completed_cron_and_delay_is_not_a_clock_change() {
        use crate::config::workspace_config::ScheduleKind;
        use std::time::{Duration, Instant};
        let start = Instant::now();
        let mut scheduler = Scheduler::new();
        scheduler.register_definition(create_test_definition());
        let id = scheduler.create_instance("test-agent").unwrap();
        scheduler
            .configure_enhanced(
                &id,
                ScheduleKind::EnhancedCron {
                    expression: "* * * * *".into(),
                    timezone: "UTC".into(),
                },
                0,
                start,
            )
            .unwrap();
        scheduler.set_wake_monitor_available(true, 0, start);
        // A delayed polling pass still claims the due occurrence.
        assert_eq!(
            scheduler.next_ready_at(120_000, start + Duration::from_secs(120)),
            Some(id.clone())
        );
        scheduler.complete_enhanced_at(&id, 120_000, start + Duration::from_secs(120));
        scheduler.wake_signal().clock_changed();
        assert_eq!(
            scheduler.next_ready_at(0, start + Duration::from_secs(121)),
            None
        );
        assert_eq!(
            scheduler.next_ready_at(60_000, start + Duration::from_secs(181)),
            None
        );
        assert_eq!(
            scheduler.next_ready_at(120_000, start + Duration::from_secs(241)),
            Some(id)
        );
    }

    #[test]
    fn explicit_clock_change_recomputes_cron_without_resetting_intervals() {
        use crate::config::workspace_config::ScheduleKind;
        use std::time::{Duration, Instant};
        let start = Instant::now();
        let mut scheduler = Scheduler::new();
        scheduler.register_definition(create_test_definition());
        let id = scheduler.create_instance("test-agent").unwrap();
        scheduler
            .configure_enhanced(
                &id,
                ScheduleKind::EnhancedCron {
                    expression: "* * * * *".into(),
                    timezone: "UTC".into(),
                },
                0,
                start,
            )
            .unwrap();
        scheduler.set_wake_monitor_available(true, 0, start);
        scheduler.wake_signal().clock_changed();
        assert_eq!(
            scheduler.next_ready_at(600_000, start + Duration::from_secs(1)),
            None
        );
        assert_eq!(
            scheduler.next_ready_at(660_000, start + Duration::from_secs(61)),
            Some(id)
        );

        let mut interval = Scheduler::new();
        interval.register_definition(create_test_definition());
        let id = interval.create_instance("test-agent").unwrap();
        interval
            .configure_enhanced(
                &id,
                ScheduleKind::EnhancedInterval {
                    interval_seconds: 15,
                },
                0,
                start,
            )
            .unwrap();
        interval.set_wake_monitor_available(true, 0, start);
        interval.wake_signal().clock_changed();
        assert_eq!(
            interval.next_ready_at(600_000, start + Duration::from_secs(14)),
            None
        );
        assert_eq!(
            interval.next_ready_at(601_000, start + Duration::from_secs(15)),
            Some(id)
        );
    }

    #[test]
    fn sleep_invalidates_a_claim_even_after_wake() {
        let signal = WakeSignal::default();
        let lease = signal.dispatch_lease().unwrap();
        assert!(lease.is_current());
        signal.will_sleep();
        assert!(!lease.is_current());
        signal.did_wake();
        assert!(!lease.is_current());
        assert!(signal.dispatch_lease().unwrap().is_current());
    }

    #[test]
    fn dispatch_commitment_orders_sleep_and_remains_active_afterward() {
        let signal = WakeSignal::default();
        let sleeping_lease = signal.dispatch_lease().unwrap();
        signal.will_sleep();
        assert!(!sleeping_lease.try_commit());
        signal.did_wake();
        assert!(!sleeping_lease.try_commit());

        let active_lease = signal.dispatch_lease().unwrap();
        let other_handle = active_lease.clone();
        assert!(active_lease.try_commit());
        signal.will_sleep();
        assert!(other_handle.try_commit());
    }

    #[test]
    fn sleep_blocks_claim_until_wake_rearms_the_interval() {
        use crate::config::workspace_config::ScheduleKind;
        use std::time::{Duration, Instant};
        let start = Instant::now();
        let mut scheduler = Scheduler::new();
        scheduler.register_definition(create_test_definition());
        let id = scheduler.create_instance("test-agent").unwrap();
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
        scheduler.set_wake_monitor_available(true, 0, start);
        let signal = scheduler.wake_signal();
        signal.will_sleep();
        assert_eq!(
            scheduler.next_ready_at(60_000, start + Duration::from_secs(60)),
            None
        );
        signal.did_wake();
        assert_eq!(
            scheduler.next_ready_at(61_000, start + Duration::from_secs(61)),
            None
        );
        assert_eq!(
            scheduler.next_ready_at(76_000, start + Duration::from_secs(76)),
            Some(id)
        );
    }

    #[test]
    fn observed_wake_is_processed_before_claiming_due_enhanced_work() {
        use crate::config::workspace_config::ScheduleKind;
        use std::time::{Duration, Instant};
        let start = Instant::now();
        let mut scheduler = Scheduler::new();
        scheduler.register_definition(create_test_definition());
        let id = scheduler.create_instance("test-agent").unwrap();
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
        scheduler.set_wake_monitor_available(true, 0, start);
        scheduler.wake_signal().did_wake();
        assert_eq!(
            scheduler.next_ready_at(60_000, start + Duration::from_secs(60)),
            None
        );
        assert_eq!(
            scheduler.next_ready_at(75_000, start + Duration::from_secs(75)),
            Some(id)
        );
    }

    #[test]
    fn enhanced_interval_waits_after_start_completion_and_wake() {
        use crate::config::workspace_config::ScheduleKind;
        use std::time::{Duration, Instant};
        let start = Instant::now();
        let mut scheduler = Scheduler::new();
        scheduler.register_definition(create_test_definition());
        let id = scheduler.create_instance("test-agent").unwrap();
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
        scheduler.set_wake_monitor_available(true, 0, start);
        assert_eq!(
            scheduler.next_ready_at(0, start + Duration::from_secs(14)),
            None
        );
        assert_eq!(
            scheduler.next_ready_at(0, start + Duration::from_secs(15)),
            Some(id.clone())
        );
        scheduler.complete_enhanced_at(&id, 0, start + Duration::from_secs(20));
        assert_eq!(
            scheduler.next_ready_at(0, start + Duration::from_secs(34)),
            None
        );
        scheduler.note_wake_at(0, start + Duration::from_secs(34));
        assert_eq!(
            scheduler.next_ready_at(0, start + Duration::from_secs(48)),
            None
        );
        assert_eq!(
            scheduler.next_ready_at(0, start + Duration::from_secs(49)),
            Some(id)
        );
    }

    #[test]
    fn test_scheduler_creation() {
        let scheduler = Scheduler::new();

        assert_eq!(scheduler.state(), &SchedulerState::Running);
        assert!(!scheduler.is_running());
        assert_eq!(scheduler.instance_count(), 0);
    }

    #[test]
    fn test_register_and_create_instance() {
        let mut scheduler = Scheduler::new();

        scheduler.register_definition(create_test_definition());

        let instance_id = scheduler.create_instance("test-agent").unwrap();

        assert_eq!(instance_id, "test-agent");
        assert_eq!(scheduler.instance_count(), 1);

        let instance = scheduler.get_instance(&instance_id).unwrap();
        assert_eq!(instance.agent_id, "test-agent");
    }

    #[test]
    fn test_scheduler_pause_resume() {
        let mut scheduler = Scheduler::new();

        assert_eq!(scheduler.state(), &SchedulerState::Running);

        scheduler.pause("Testing".to_string());
        assert!(matches!(scheduler.state(), SchedulerState::Paused { .. }));

        scheduler.resume();
        assert_eq!(scheduler.state(), &SchedulerState::Running);
    }

    #[test]
    fn test_next_ready_basic() {
        let mut scheduler = Scheduler::new();

        scheduler.register_definition(create_test_definition());
        scheduler.create_instance("test-agent");

        // First call should return the instance
        let next = scheduler.next_ready();
        assert_eq!(next, Some("test-agent".to_string()));

        // Second call should return None (agent is running)
        let next = scheduler.next_ready();
        assert!(next.is_none());

        // After completion, should be scheduled for later (not immediately ready)
        let target = chrono::Utc::now().timestamp_millis() + 60_000;
        scheduler.complete_agent("test-agent", true, target);
        let next = scheduler.next_ready();
        assert!(next.is_none()); // Scheduled for 60 seconds later
    }

    #[test]
    fn test_only_one_agent_at_a_time() {
        let mut scheduler = Scheduler::new();

        scheduler.register_definition(create_test_definition());
        scheduler.register_definition(definition_named("other-agent"));
        scheduler.create_instance("test-agent");
        scheduler.create_instance("other-agent");

        // First should succeed
        let first = scheduler.next_ready();
        assert!(first.is_some());

        // Second should fail (only one at a time)
        let second = scheduler.next_ready();
        assert!(second.is_none());

        // Complete first
        let target = chrono::Utc::now().timestamp_millis() + 60_000;
        scheduler.complete_agent(&first.unwrap(), true, target);

        // Now second should work (but it's scheduled for later)
        // Both instances were scheduled, so they'll run based on next_run_at
        assert!(!scheduler.is_running());
    }

    #[test]
    fn test_global_pause_preserves_per_instance_state() {
        // Global "pause all" is an overlay: while paused nothing runs, and on
        // resume each instance's individual enabled/disabled state is intact.
        let mut scheduler = Scheduler::new();
        scheduler.register_definition(create_test_definition());
        scheduler.register_definition(definition_named("other-agent"));
        let a = scheduler.create_instance("test-agent").unwrap();
        let b = scheduler.create_instance("other-agent").unwrap();

        // B is individually paused (per-workspace), A is active.
        scheduler.set_instance_enabled(&b, false);

        // Global pause hides everything, including the active A.
        scheduler.pause("Paused by user".to_string());
        assert!(scheduler.next_ready().is_none());

        // Resume lifts the overlay: A becomes ready again, but B stays
        // disabled — its per-workspace pause was preserved, not cleared.
        scheduler.resume();
        assert_eq!(scheduler.next_ready(), Some(a));
        assert!(!scheduler.get_instance(&b).unwrap().enabled);
    }

    #[test]
    fn test_paused_scheduler_returns_none() {
        let mut scheduler = Scheduler::new();

        scheduler.register_definition(create_test_definition());
        scheduler.create_instance("test-agent");

        scheduler.pause("Maintenance".to_string());

        let next = scheduler.next_ready();
        assert!(next.is_none());

        scheduler.resume();

        let next = scheduler.next_ready();
        assert!(next.is_some());
    }

    #[test]
    fn test_remove_instance() {
        let mut scheduler = Scheduler::new();

        scheduler.register_definition(create_test_definition());
        let instance_id = scheduler.create_instance("test-agent").unwrap();

        assert_eq!(scheduler.instance_count(), 1);

        scheduler.remove_instance(&instance_id);
        assert_eq!(scheduler.instance_count(), 0);
    }

    #[test]
    fn test_enable_disable_instance() {
        let mut scheduler = Scheduler::new();

        scheduler.register_definition(create_test_definition());
        scheduler.create_instance("test-agent");

        // Disable the instance
        scheduler.set_instance_enabled("test-agent", false);

        // Should not be ready
        let next = scheduler.next_ready();
        assert!(next.is_none());

        // Re-enable
        scheduler.set_instance_enabled("test-agent", true);

        // Should be ready now
        let next = scheduler.next_ready();
        assert!(next.is_some());
    }

    // -------------------------------------------------------------------
    // set_instance_next_run_at: restart-survival seed
    // -------------------------------------------------------------------

    #[test]
    fn set_instance_next_run_at_future_target_defers_readiness() {
        let mut scheduler = Scheduler::new();
        scheduler.register_definition(create_test_definition());
        let instance_id = scheduler.create_instance("test-agent").unwrap();

        // Target 1 hour in the future — instance should not be ready.
        let future = chrono::Utc::now().timestamp_millis() + 60 * 60 * 1000;
        scheduler.set_instance_next_run_at(&instance_id, Some(future));

        assert!(scheduler.next_ready().is_none());
    }

    #[test]
    fn set_instance_next_run_at_past_target_clears_to_ready_now() {
        let mut scheduler = Scheduler::new();
        scheduler.register_definition(create_test_definition());
        let instance_id = scheduler.create_instance("test-agent").unwrap();

        // Mark instance not-ready first by giving it a far-future anchor.
        let future = chrono::Utc::now().timestamp_millis() + 60 * 60 * 1000;
        scheduler.set_instance_next_run_at(&instance_id, Some(future));
        assert!(scheduler.next_ready().is_none());

        // Past anchor (1 hour ago) — clears next_run_at so the next tick
        // picks it up. Matches "catch up after restart" semantics when
        // the app was down across an interval boundary.
        let past = chrono::Utc::now().timestamp_millis() - 60 * 60 * 1000;
        scheduler.set_instance_next_run_at(&instance_id, Some(past));
        assert!(scheduler.next_ready().is_some());
    }

    #[test]
    fn set_instance_next_run_at_none_clears_to_ready_now() {
        let mut scheduler = Scheduler::new();
        scheduler.register_definition(create_test_definition());
        let instance_id = scheduler.create_instance("test-agent").unwrap();

        // Park far in the future, then clear with None.
        let future = chrono::Utc::now().timestamp_millis() + 60 * 60 * 1000;
        scheduler.set_instance_next_run_at(&instance_id, Some(future));
        scheduler.set_instance_next_run_at(&instance_id, None);

        assert!(scheduler.next_ready().is_some());
    }

    #[test]
    fn set_instance_next_run_at_ignores_unknown_instance() {
        let mut scheduler = Scheduler::new();
        // Just a smoke check that the function doesn't panic on a
        // missing id — covers the early-return branch.
        scheduler.set_instance_next_run_at("nonexistent", Some(0));
    }
}
