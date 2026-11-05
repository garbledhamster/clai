use super::credentials::{CredentialStore, NativeCredentials};

/// Runs the actual platform credential adapter, never the test credential map.
#[test]
#[ignore = "Requires an unlocked native credential store; run explicitly during native QA"]
fn native_protected_storage_round_trip_and_cleanup() {
    let reference = uuid::Uuid::new_v4().to_string();
    struct Cleanup(String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = NativeCredentials.remove(&self.0);
        }
    }
    let _cleanup = Cleanup(reference.clone());
    let disposable = format!("disposable-{}", uuid::Uuid::new_v4());
    NativeCredentials
        .set(&reference, &disposable)
        .expect("native write failed");
    let readback = NativeCredentials
        .get(&reference)
        .expect("native read failed");
    assert!(readback == disposable, "native value readback differed");
    NativeCredentials
        .remove(&reference)
        .expect("native cleanup failed");
    assert!(
        NativeCredentials.get(&reference).is_err(),
        "native credential still readable after cleanup"
    );
}

/// Run alone so unrelated simultaneous launches do not affect the parent FD count.
#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "Requires real Linux/Flatpak sandbox execution; run this exact probe alone during platform QA"]
async fn linux_argument_input_is_closed_after_success_failure_timeout_and_cancellation() {
    use super::{SaveVariable, VariableInput, VariablesRegistry};
    use std::sync::Arc;
    fn argument_inputs() -> usize {
        std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .flatten()
            .filter_map(|entry| std::fs::read_link(entry.path()).ok())
            .filter(|path| path.to_string_lossy().contains("memfd:clai-sandbox-args"))
            .count()
    }
    assert_eq!(argument_inputs(), 0, "run the exact probe alone");
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let registry =
        Arc::new(VariablesRegistry::open(temp.path().join("profile/variables")).unwrap());
    let workspace_id = "249c0490-2c32-4944-8c09-75ca21318ffe";
    registry
        .save(
            SaveVariable {
                id: None,
                expected_revision: None,
                name: "MESSAGE".into(),
                value: VariableInput::Text {
                    value: "descriptor-lifecycle-data".into(),
                },
                workspace_ids: vec![workspace_id.into()],
            },
            &[workspace_id.into()].into(),
        )
        .unwrap();
    for (script, timeout_ms, code, timed_out) in [
        (
            "test \"${CLAI_VAR_MESSAGE?}\" = descriptor-lifecycle-data",
            5000,
            Some(0),
            false,
        ),
        ("exit 7", 5000, Some(7), false),
        ("sleep 30", 30, None, true),
    ] {
        let mut command = super::tests::native_command(&workspace, temp.path(), script);
        command.timeout_ms = timeout_ms;
        let output =
            crate::assistant::tools::local::run_workspace_command(&registry, workspace_id, command)
                .await
                .unwrap();
        assert_eq!(output.exit_code, code);
        assert_eq!(output.timed_out, timed_out);
        assert_eq!(
            argument_inputs(),
            0,
            "parent must release anonymous input after a completed outcome"
        );
    }
    let command = super::tests::native_command(&workspace, temp.path(), "touch ready; sleep 30");
    let pending = tokio::spawn({
        let registry = registry.clone();
        async move {
            crate::assistant::tools::local::run_workspace_command(&registry, workspace_id, command)
                .await
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !workspace.join("ready").exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    // Caller cancellation and parent FD release do not establish host-child teardown.
    assert_eq!(
        argument_inputs(),
        0,
        "cancellation must not retain owned argument input"
    );
}
