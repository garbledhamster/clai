//! Platform notification registration for the in-process scheduler.

use super::scheduler::WakeSignal;
#[cfg(any(target_os = "macos", test))]
use std::sync::Arc;

#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    WillSleep,
    DidWake,
    ClockChanged,
}

#[cfg(any(target_os = "macos", test))]
type Callback = Arc<dyn Fn() + Send + Sync>;

#[cfg(any(target_os = "macos", test))]
fn register_all<T>(
    signal: WakeSignal,
    mut register: impl FnMut(Event, Callback) -> Result<T, String>,
) -> Result<Vec<T>, String> {
    let sleep = signal.clone();
    let wake = signal.clone();
    let callbacks: [(Event, Callback); 3] = [
        (Event::WillSleep, Arc::new(move || sleep.will_sleep())),
        (Event::DidWake, Arc::new(move || wake.did_wake())),
        (
            Event::ClockChanged,
            Arc::new(move || signal.clock_changed()),
        ),
    ];
    callbacks
        .into_iter()
        .map(|(event, callback)| register(event, callback))
        .collect()
}

#[cfg(target_os = "macos")]
mod macos {
    use super::{register_all, Callback, Event, WakeSignal};
    use block2::RcBlock;
    use objc2::{rc::Retained, runtime::ProtocolObject, MainThreadMarker};
    use objc2_app_kit::{
        NSWorkspace, NSWorkspaceDidWakeNotification, NSWorkspaceWillSleepNotification,
    };
    use objc2_foundation::{
        NSNotification, NSNotificationCenter, NSObjectProtocol, NSSystemClockDidChangeNotification,
    };
    use std::{cell::RefCell, ptr::NonNull};

    struct Observer {
        center: Retained<NSNotificationCenter>,
        token: Retained<ProtocolObject<dyn NSObjectProtocol>>,
    }
    impl Drop for Observer {
        fn drop(&mut self) {
            // Registered on the main thread and removed on that same thread.
            unsafe {
                self.center.removeObserver((*self.token).as_ref());
            }
        }
    }
    thread_local! { static OBSERVERS: RefCell<Option<Vec<Observer>>> = const { RefCell::new(None) }; }

    fn register(event: Event, callback: Callback) -> Observer {
        let workspace = NSWorkspace::sharedWorkspace();
        let center = match event {
            Event::ClockChanged => NSNotificationCenter::defaultCenter(),
            _ => workspace.notificationCenter(),
        };
        let name = unsafe {
            match event {
                Event::WillSleep => NSWorkspaceWillSleepNotification,
                Event::DidWake => NSWorkspaceDidWakeNotification,
                Event::ClockChanged => NSSystemClockDidChangeNotification,
            }
        };
        let block = RcBlock::new(move |_: NonNull<NSNotification>| callback());
        // The callback owns a Send + Sync signal and no AppKit state. Queue=nil
        // delivers on the posting thread, so will-sleep closes the gate synchronously.
        let token = unsafe {
            center.addObserverForName_object_queue_usingBlock(Some(name), None, None, &block)
        };
        Observer { center, token }
    }

    pub fn install(signal: WakeSignal) -> Result<(), String> {
        MainThreadMarker::new().ok_or("Power observers require the main thread")?;
        OBSERVERS.with(|slot| {
            if slot.borrow().is_some() {
                return Err("Power observers are already installed".into());
            }
            let observers = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                register_all(signal, |event, callback| Ok(register(event, callback)))
            }))
            .map_err(|_| "Power observer registration failed".to_string())??;
            *slot.borrow_mut() = Some(observers);
            Ok(())
        })
    }

    pub fn uninstall() -> Result<(), String> {
        MainThreadMarker::new().ok_or("Power observers require main-thread removal")?;
        OBSERVERS.with(|slot| {
            slot.borrow_mut().take();
        });
        Ok(())
    }
}

#[cfg(target_os = "macos")]
pub use macos::{install, uninstall};

#[cfg(not(target_os = "macos"))]
pub fn install(_signal: WakeSignal) -> Result<(), String> {
    Err("Power and clock notifications are unavailable on this platform".into())
}
#[cfg(not(target_os = "macos"))]
pub fn uninstall() -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::scheduler::WakeSignal;
    use std::sync::{Arc, Mutex};

    struct Token(Arc<Mutex<usize>>);
    impl Drop for Token {
        fn drop(&mut self) {
            *self.0.lock().unwrap() += 1;
        }
    }

    #[test]
    fn partial_registration_failure_removes_earlier_observers() {
        for fail_at in 1..=3 {
            let drops = Arc::new(Mutex::new(0));
            let mut count = 0;
            let result = register_all(WakeSignal::default(), |_, _| {
                count += 1;
                if count == fail_at {
                    return Err("observer unavailable".to_string());
                }
                Ok(Token(drops.clone()))
            });
            assert!(result.is_err());
            assert_eq!(*drops.lock().unwrap(), fail_at - 1);
        }
    }

    #[test]
    fn registered_sleep_and_wake_callbacks_rearm_the_signal_and_cleanup() {
        let signal = WakeSignal::default();
        let old = signal.dispatch_lease().unwrap();
        let drops = Arc::new(Mutex::new(0));
        let mut callbacks = Vec::new();
        let tokens = register_all(signal.clone(), |event, callback| {
            callbacks.push((event, callback));
            Ok(Token(drops.clone()))
        })
        .unwrap();
        assert_eq!(
            callbacks
                .iter()
                .map(|(event, _)| *event)
                .collect::<Vec<_>>(),
            vec![Event::WillSleep, Event::DidWake, Event::ClockChanged]
        );
        (callbacks[0].1)();
        assert!(!old.is_current());
        (callbacks[1].1)();
        assert!(!old.is_current());
        assert!(signal.dispatch_lease().unwrap().is_current());
        drop(tokens);
        assert_eq!(*drops.lock().unwrap(), 3);
    }
}
