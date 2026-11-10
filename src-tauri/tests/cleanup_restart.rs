//! Opt-in real process-kill/restart check with disposable workspace storage.
//! No Tauri app, provider connection, installed application or live data is used.
#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use clai_lib::scheduler_verification::{
    persist_cleanup, recover_cleanup, recover_cleanup_before_record, ScriptConfig, ScriptSource,
};
use clai_lib::{
    workspace_config, AppConfig, AppState, ConfigManager, WorkspaceConfig, WorkspaceIndex,
};

fn state(root: &Path) -> AppState {
    let workspace = workspace_config::load(root).unwrap();
    let mut index = WorkspaceIndex::default();
    index.insert_config(root.to_path_buf(), &workspace);
    AppState::new_for_tests(
        ConfigManager::new_for_tests(AppConfig::default(), root.join("isolated-app.json")),
        index,
    )
    .unwrap()
}

struct DisposableChild(Child);
impl Drop for DisposableChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn child(root: &Path, phase: &str) -> DisposableChild {
    DisposableChild(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "cleanup_process_child",
                "--ignored",
                "--nocapture",
            ])
            .env("CLAI_CLEANUP_PROBE_ROOT", root)
            .env("CLAI_CLEANUP_PROBE_PHASE", phase)
            .spawn()
            .unwrap(),
    )
}

#[test]
#[ignore = "disposable real-process crash verification; run during final QA"]
fn cleanup_process_child() {
    let Some(root) = std::env::var_os("CLAI_CLEANUP_PROBE_ROOT") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let state = state(&root);
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        if std::env::var("CLAI_CLEANUP_PROBE_PHASE").unwrap() == "before-record" {
            recover_cleanup_before_record(&state, "probe", async {
                std::fs::write(root.join("cleanup-exited.ready"), "ready").unwrap();
                std::future::pending::<()>().await;
            })
            .await
            .unwrap();
        } else {
            recover_cleanup(&state, "probe").await.unwrap();
        }
    });
}

#[test]
#[ignore = "disposable real-process crash verification; run during final QA"]
fn cleanup_process_kill_after_exit_repeats_bound_cleanup_on_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut workspace = WorkspaceConfig::new(
        "probe".into(),
        "Cleanup restart probe".into(),
        0,
        "main".into(),
    );
    let execution = &mut workspace.main_agent.as_mut().unwrap().execution;
    execution.shell.mode = serde_json::from_str("\"full\"").unwrap();
    execution.sandbox.network_access.mode = serde_json::from_str("\"off\"").unwrap();
    workspace.schedule.enabled = false;
    workspace_config::save(root, &workspace).unwrap();
    let state = state(root);
    let intent = persist_cleanup(
        &state,
        "probe",
        "isolated-session",
        "isolated-run",
        &ScriptConfig {
            source: ScriptSource::Inline {
                content: "printf 'cleanup\\n' >> cleanup-effects.log".into(),
            },
            interpreter: "/bin/sh".into(),
            timeout_seconds: 10,
        },
        true,
    )
    .unwrap();
    let mut first = child(root, "before-record");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !root.join("cleanup-exited.ready").exists() {
        assert!(
            first.0.try_wait().unwrap().is_none(),
            "child exited before checkpoint"
        );
        assert!(Instant::now() < deadline, "cleanup checkpoint timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        std::fs::read_to_string(root.join("cleanup-effects.log")).unwrap(),
        "cleanup\n"
    );
    first.0.kill().unwrap();
    first.0.wait().unwrap();
    assert_eq!(
        workspace_config::load(root)
            .unwrap()
            .schedule
            .cleanup_intent
            .unwrap()
            .id,
        intent.id
    );
    let mut restarted = child(root, "retry");
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = restarted.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "restart recovery timed out");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(status.success());
    let saved = workspace_config::load(root).unwrap();
    assert!(saved.schedule.cleanup_intent.is_none());
    assert_eq!(
        saved
            .schedule
            .cleanup_completion
            .unwrap()
            .result
            .unwrap()
            .exit_code,
        Some(0)
    );
    assert_eq!(
        std::fs::read_to_string(root.join("cleanup-effects.log")).unwrap(),
        "cleanup\ncleanup\n"
    );
}
