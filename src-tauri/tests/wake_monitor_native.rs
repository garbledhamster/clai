//! Synthetic in-process notifications; this never sleeps or changes the host.

#[cfg(target_os = "macos")]
pub mod scheduler {
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    pub struct WakeSignal(pub Arc<Mutex<[usize; 3]>>);
    impl WakeSignal {
        pub fn will_sleep(&self) {
            self.0.lock().unwrap()[0] += 1;
        }
        pub fn did_wake(&self) {
            self.0.lock().unwrap()[1] += 1;
        }
        pub fn clock_changed(&self) {
            self.0.lock().unwrap()[2] += 1;
        }
    }
}

#[cfg(target_os = "macos")]
mod agents {
    pub use crate::scheduler;
}

#[cfg(target_os = "macos")]
#[allow(dead_code, unused_imports)]
#[path = "../src/agents/wake_monitor.rs"]
mod wake_monitor;

#[cfg(target_os = "macos")]
fn main() {
    use objc2_app_kit::{
        NSWorkspace, NSWorkspaceDidWakeNotification, NSWorkspaceWillSleepNotification,
    };
    use objc2_foundation::{NSNotificationCenter, NSSystemClockDidChangeNotification};
    use scheduler::WakeSignal;

    let off_main = std::thread::spawn(|| wake_monitor::install(WakeSignal::default()));
    assert!(off_main.join().unwrap().is_err());
    let signal = WakeSignal::default();
    wake_monitor::install(signal.clone()).unwrap();
    assert!(wake_monitor::install(signal.clone()).is_err());
    let workspace = NSWorkspace::sharedWorkspace().notificationCenter();
    let clock = NSNotificationCenter::defaultCenter();
    let post = || unsafe {
        workspace.postNotificationName_object(NSWorkspaceWillSleepNotification, None);
        workspace.postNotificationName_object(NSWorkspaceDidWakeNotification, None);
        clock.postNotificationName_object(NSSystemClockDidChangeNotification, None);
    };
    post();
    assert_eq!(*signal.0.lock().unwrap(), [1, 1, 1]);
    wake_monitor::uninstall().unwrap();
    post();
    assert_eq!(*signal.0.lock().unwrap(), [1, 1, 1]);
    println!("Synthetic macOS sleep, wake, clock and removal notifications passed");
}

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("macOS notification test skipped on this platform");
}
