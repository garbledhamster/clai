//! Opt-in native evidence only: no AppState, databases, files or providers.

use std::ptr::NonNull;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use block2::RcBlock;
use clai_lib::scheduler_verification::{
    self as production, AgentDefinition, ScheduleKind, Scheduler,
};
use objc2::{rc::Retained, runtime::ProtocolObject};
use objc2_app_kit::{
    NSWorkspace, NSWorkspaceDidWakeNotification, NSWorkspaceWillSleepNotification,
};
use objc2_foundation::{
    NSDate, NSNotification, NSNotificationCenter, NSObjectProtocol, NSRunLoop,
    NSSystemClockDidChangeNotification,
};

const INTERVAL: &str = "probe-interval";
const CRON: &str = "probe-cron";

#[derive(Clone, Copy, Default)]
struct Events {
    sleep_ms: Option<i64>,
    wake_ms: Option<i64>,
    clocks: usize,
}

struct Observation {
    center: Retained<NSNotificationCenter>,
    token: Retained<ProtocolObject<dyn NSObjectProtocol>>,
}

impl Drop for Observation {
    fn drop(&mut self) {
        unsafe {
            self.center.removeObserver((*self.token).as_ref());
        }
    }
}

fn observe(events: &Arc<Mutex<Events>>) -> Vec<Observation> {
    let workspace = NSWorkspace::sharedWorkspace().notificationCenter();
    let clock = NSNotificationCenter::defaultCenter();
    let registrations = unsafe {
        [
            (workspace.clone(), NSWorkspaceWillSleepNotification, 0),
            (workspace, NSWorkspaceDidWakeNotification, 1),
            (clock, NSSystemClockDidChangeNotification, 2),
        ]
    };
    registrations
        .into_iter()
        .map(|(center, name, kind)| {
            let events = events.clone();
            let callback = RcBlock::new(move |_: NonNull<NSNotification>| {
                let now = chrono::Utc::now().timestamp_millis();
                let mut events = events.lock().unwrap();
                match kind {
                    0 => events.sleep_ms = Some(now),
                    1 => events.wake_ms = Some(now),
                    _ => events.clocks += 1,
                }
            });
            let token = unsafe {
                center.addObserverForName_object_queue_usingBlock(Some(name), None, None, &callback)
            };
            Observation { center, token }
        })
        .collect()
}

fn pump() {
    NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.05));
    // A run loop without pending sources may return immediately.
    std::thread::sleep(Duration::from_millis(10));
}

fn fresh_scheduler() -> Result<Scheduler, String> {
    let mut scheduler = Scheduler::new();
    let wall = chrono::Utc::now().timestamp_millis();
    let now = Instant::now();
    for (id, kind) in [
        (
            INTERVAL,
            ScheduleKind::EnhancedInterval {
                interval_seconds: 15,
            },
        ),
        (
            CRON,
            ScheduleKind::EnhancedCron {
                expression: "* * * * *".into(),
                timezone: "UTC".into(),
            },
        ),
    ] {
        scheduler.register_definition(
            AgentDefinition::new(id, id).with_workspace_id("disposable-native-probe"),
        );
        scheduler
            .create_instance(id)
            .ok_or("Could not create in-memory instance")?;
        // An overdue legacy anchor must not cause enhanced work to replay.
        scheduler.set_instance_next_run_at(id, Some(wall - 120_000));
        scheduler.configure_enhanced(id, kind, wall, now)?;
    }
    production::initialize_wake_monitor(&mut scheduler, production::install)?;
    Ok(scheduler)
}

fn wait_for_real_wake(events: &Arc<Mutex<Events>>) -> Result<Events, String> {
    println!("READY: At this Mac, choose Apple menu > Sleep. Leave it asleep for at least 75 seconds, then wake it normally.");
    println!("This process never requests sleep. Waiting at most 10 awake minutes for one cycle.");
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        pump();
        let snapshot = *events.lock().unwrap();
        if let (Some(sleep), Some(wake)) = (snapshot.sleep_ms, snapshot.wake_ms) {
            if wake >= sleep {
                if wake.saturating_sub(sleep) < 75_000 {
                    return Err("The observed sleep was shorter than 75 seconds; repeat the probe with a longer manual sleep.".into());
                }
                println!("SIGNALS PASS: real will-sleep={sleep}, did-wake={wake}, clock-change callbacks={}", snapshot.clocks);
                return Ok(snapshot);
            }
        }
        if Instant::now() >= deadline {
            return Err("No complete real sleep/wake cycle observed within the limit.".into());
        }
    }
}

fn verify_after_wake(
    scheduler: &mut Scheduler,
    events: &Arc<Mutex<Events>>,
    observed: Events,
) -> Result<(), String> {
    // Deliver all callbacks from the notification before inspecting scheduling.
    pump();
    let resumed = Instant::now();
    let wall = chrono::Utc::now().timestamp_millis();
    let next_cron_minute = wall.div_euclid(60_000) * 60_000 + 60_000;
    if scheduler.next_ready().is_some() {
        return Err("An overdue enhanced attempt replayed immediately after real wake.".into());
    }
    println!(
        "REARM PASS: no immediate replay; interval countdown={:?}, cron countdown={:?}",
        scheduler.seconds_until_next_run(INTERVAL),
        scheduler.seconds_until_next_run(CRON)
    );
    let deadline = resumed + Duration::from_secs(90);
    let mut interval_passed = false;
    let mut cron_passed = false;
    while !(interval_passed && cron_passed) {
        pump();
        let snapshot = *events.lock().unwrap();
        if snapshot.sleep_ms != observed.sleep_ms
            || snapshot.wake_ms != observed.wake_ms
            || snapshot.clocks != observed.clocks
        {
            return Err("An additional power/clock event occurred during timing verification; repeat one clean cycle.".into());
        }
        if let Some(id) = scheduler.next_ready() {
            let elapsed = resumed.elapsed();
            let wall = chrono::Utc::now().timestamp_millis();
            if !scheduler
                .take_claimed_dispatch_lease(&id)
                .is_some_and(|lease| lease.try_commit())
            {
                return Err("Scheduled claim could not commit after wake.".into());
            }
            if id == INTERVAL {
                if elapsed < Duration::from_secs(15) {
                    return Err(
                        "Interval became ready before 15 elapsed seconds after rearming.".into(),
                    );
                }
                interval_passed = true;
                println!(
                    "INTERVAL PASS: first attempt after {:.3} elapsed seconds",
                    elapsed.as_secs_f64()
                );
            } else if id == CRON {
                if wall < next_cron_minute {
                    return Err("Cron replayed before the next future UTC minute.".into());
                }
                cron_passed = true;
                println!("CRON PASS: first future-minute attempt at UTC epoch ms {wall}");
            }
            // A fake completion only: never enter the assistant engine.
            scheduler.complete_agent(&id, true, wall);
            scheduler.set_instance_enabled(&id, false);
        }
        if Instant::now() >= deadline {
            return Err("Post-wake timing did not finish within 90 seconds.".into());
        }
    }
    Ok(())
}

pub fn run(preflight: bool) -> Result<(), String> {
    let mut scheduler = fresh_scheduler()?;
    let events = Arc::new(Mutex::new(Events::default()));
    let observations = observe(&events);
    let result = (|| {
        pump();
        if !scheduler.wake_monitor_available() || scheduler.next_ready().is_some() {
            return Err(
                "Fresh enhanced initialization was unavailable or replayed an overdue anchor."
                    .into(),
            );
        }
        println!("INITIALIZATION PASS: production observer registration and fresh in-memory interval/cron timing; overdue legacy anchors ignored.");
        if preflight {
            return Ok(());
        }
        let observed = wait_for_real_wake(&events)?;
        verify_after_wake(&mut scheduler, &events, observed)?;
        Ok(())
    })();
    drop(observations);
    let cleanup = production::uninstall();
    result.and(cleanup)?;
    if preflight {
        println!("PREFLIGHT PASS: awake setup/run-loop/teardown only. No real sleep/wake evidence claimed.");
    } else {
        println!("REAL WAKE PASS: native notifications and production scheduler timing only; full runner/UI/persisted-app-restart acceptance remains separate.");
    }
    Ok(())
}
