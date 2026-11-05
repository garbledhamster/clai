use super::*;

const WORKSPACE_A: &str = "b147151d-ddb3-45ac-9e7f-e1aa48c0f3de";
const WORKSPACE_B: &str = "443481a5-1b84-44bd-9d2c-9a8d3b41c688";

fn known() -> HashSet<String> {
    [WORKSPACE_A.into(), WORKSPACE_B.into()].into()
}

// Catches lost persistence or grants, and interpolation of literal text.
#[test]
fn text_save_reload_and_workspace_filter_preserve_literal_value() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("variables");
    let registry = VariablesRegistry::open(path.clone()).unwrap();
    assert!(registry.list(None).unwrap().is_empty());
    let result = registry.save(
        SaveVariable {
            id: None,
            expected_revision: None,
            name: "MESSAGE".into(),
            value: VariableInput::Text {
                value: "literal '$HOME'\n$(printf inert)".into(),
            },
            workspace_ids: vec![WORKSPACE_A.into()],
        },
        &known(),
    );
    assert!(result.is_ok(), "Text definition must save: {result:?}");
    let saved = result.unwrap().definition;
    assert_eq!(saved.revision, 1);
    assert!(!saved.id.is_empty());
    drop(registry);
    let reopened = VariablesRegistry::open(path).unwrap();
    assert_eq!(
        reopened.list(Some(WORKSPACE_A)).unwrap(),
        vec![saved.clone()]
    );
    assert!(reopened.list(Some(WORKSPACE_B)).unwrap().is_empty());
    assert_eq!(
        saved.value,
        Some(PlainValue::Text("literal '$HOME'\n$(printf inert)".into()))
    );
}

#[cfg(unix)]
#[test]
fn unanchored_profile_cannot_begin_writes_after_external_repair_without_reopening() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("unavailable");
    let alias = temp.path().join("alias");
    std::os::unix::fs::symlink(&target, &alias).unwrap();
    let directory = alias.join("variables");
    let registry = VariablesRegistry::open(directory.clone()).unwrap();
    std::fs::create_dir(target).unwrap();
    assert!(registry
        .save(create("MESSAGE", "must-not-write"), &known())
        .is_err());
    assert!(!directory.join("registry.json").exists());
    assert!(VariablesRegistry::open(directory)
        .unwrap()
        .save(create("MESSAGE", "available-after-reopen"), &known())
        .is_ok());
}

fn create(name: &str, value: &str) -> SaveVariable {
    SaveVariable {
        id: None,
        expected_revision: None,
        name: name.into(),
        value: VariableInput::Text {
            value: value.into(),
        },
        workspace_ids: vec![WORKSPACE_A.into()],
    }
}

// Catches accepting names that cannot safely become namespaced environment keys.
#[test]
fn invalid_names_and_nul_values_never_change_saved_state() {
    let temp = tempfile::tempdir().unwrap();
    let registry = VariablesRegistry::open(temp.path().join("variables")).unwrap();
    for name in ["", "lowercase", "1NUMBER", "HAS SPACE", "A=B", "A;B", "É"] {
        assert!(
            registry.save(create(name, "text"), &known()).is_err(),
            "invalid key was accepted"
        );
    }
    assert!(registry.save(create("VALID", "a\0b"), &known()).is_err());
    assert!(registry.list(None).unwrap().is_empty());
}

// Catches lost-update overwrite and duplicate key collisions across definitions.
#[test]
fn updates_require_current_revision_and_unique_name() {
    let temp = tempfile::tempdir().unwrap();
    let registry = VariablesRegistry::open(temp.path().join("variables")).unwrap();
    let original = registry
        .save(create("FIRST", "one"), &known())
        .unwrap()
        .definition;
    assert!(registry
        .save(create("FIRST", "duplicate"), &known())
        .is_err());
    let mut edit = create("RENAMED", "two");
    edit.id = Some(original.id.clone());
    edit.expected_revision = Some(original.revision);
    let updated = registry.save(edit, &known()).unwrap().definition;
    assert_eq!(updated.revision, 2);
    let mut stale = create("FIRST", "stale");
    stale.id = Some(original.id);
    stale.expected_revision = Some(1);
    assert!(registry.save(stale, &known()).is_err());
    assert_eq!(registry.list(None).unwrap(), vec![updated]);
}

// Catches accidental implicit grants or acceptance of a deleted workspace ID.
#[test]
fn unknown_workspace_is_rejected_and_revocation_is_saved_atomically() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("variables");
    let registry = VariablesRegistry::open(path.clone()).unwrap();
    let mut unknown = create("FIRST", "one");
    unknown.workspace_ids = vec!["5ce04a09-cd03-4228-8cd9-e072d17ae5fc".into()];
    assert!(registry.save(unknown, &known()).is_err());
    let original = registry
        .save(create("FIRST", "one"), &known())
        .unwrap()
        .definition;
    let mut edit = create("FIRST", "one");
    edit.id = Some(original.id);
    edit.expected_revision = Some(1);
    edit.workspace_ids.clear();
    registry.save(edit, &known()).unwrap();
    assert!(VariablesRegistry::open(path)
        .unwrap()
        .list(Some(WORKSPACE_A))
        .unwrap()
        .is_empty());
}

// Real rename failure must preserve both cached readers and the reopened registry.
#[test]
fn failed_disk_commit_preserves_previous_working_definition() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("variables");
    let registry = VariablesRegistry::open(path.clone()).unwrap();
    let original = registry
        .save(create("FIRST", "one"), &known())
        .unwrap()
        .definition;
    std::fs::rename(path.join("registry.json"), path.join("previous.json")).unwrap();
    std::fs::create_dir(path.join("registry.json")).unwrap();
    assert!(registry.save(create("SECOND", "two"), &known()).is_err());
    assert_eq!(registry.list(None).unwrap(), vec![original.clone()]);
    std::fs::remove_dir(path.join("registry.json")).unwrap();
    std::fs::rename(path.join("previous.json"), path.join("registry.json")).unwrap();
    assert_eq!(
        VariablesRegistry::open(path).unwrap().list(None).unwrap(),
        vec![original]
    );
}

// Catches a Boolean being rejected or coerced into free-form text by the Settings contract.
#[test]
fn boolean_false_round_trips_as_typed_data() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("variables");
    let registry = VariablesRegistry::open(path.clone()).unwrap();
    let input = serde_json::from_value::<SaveVariable>(serde_json::json!({
        "id": null, "expectedRevision": null, "name": "ENABLED",
        "value": {"type": "boolean", "value": false}, "workspaceIds": [WORKSPACE_A]
    }));
    assert!(
        input.is_ok(),
        "Boolean input must be accepted as typed data"
    );
    let saved = registry.save(input.unwrap(), &known()).unwrap().definition;
    let metadata = serde_json::to_value(saved).unwrap();
    assert_eq!(metadata["kind"], "boolean");
    assert_eq!(metadata["value"], false);
    let reopened = VariablesRegistry::open(path).unwrap();
    assert_eq!(
        serde_json::to_value(reopened.list(None).unwrap()).unwrap()[0]["value"],
        false
    );
}

#[test]
fn numbers_round_trip_as_finite_numeric_values() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("variables");
    let registry = VariablesRegistry::open(path.clone()).unwrap();
    for (name, source, expected) in [
        ("ZERO", "0", 0.0),
        ("NEGATIVE", "-12.5", -12.5),
        ("DECIMAL", "1.25", 1.25),
    ] {
        let input = serde_json::from_value::<SaveVariable>(
            serde_json::json!({"id":null,"expectedRevision":null,"name":name,"value":{"type":"number","value":source},"workspaceIds":[]}),
        );
        assert!(input.is_ok(), "Number input must be accepted");
        let saved =
            serde_json::to_value(registry.save(input.unwrap(), &known()).unwrap().definition)
                .unwrap();
        assert_eq!(saved["kind"], "number");
        assert_eq!(saved["value"].as_f64(), Some(expected));
    }
    let reopened = VariablesRegistry::open(path).unwrap();
    assert_eq!(reopened.list(None).unwrap().len(), 3);
}

#[test]
fn invalid_numeric_and_boolean_input_cannot_change_registry() {
    let temp = tempfile::tempdir().unwrap();
    let registry = VariablesRegistry::open(temp.path().join("variables")).unwrap();
    for text in ["", " ", "twelve", "NaN", "inf", "-inf", "1e999"] {
        let parsed = serde_json::from_value::<SaveVariable>(
            serde_json::json!({"id":null,"expectedRevision":null,"name":"NUMBER","value":{"type":"number","value":text},"workspaceIds":[]}),
        );
        if let Ok(input) = parsed {
            assert!(
                registry.save(input, &known()).is_err(),
                "invalid numeric value accepted"
            );
        }
    }
    assert!(serde_json::from_value::<SaveVariable>(serde_json::json!({"id":null,"expectedRevision":null,"name":"FLAG","value":{"type":"boolean","value":"false"},"workspaceIds":[]})).is_err());
    assert!(registry.list(None).unwrap().is_empty());
}

#[derive(Default)]
struct TestCredentials {
    values: Mutex<std::collections::HashMap<String, String>>,
    reads: std::sync::atomic::AtomicUsize,
    fail_write: std::sync::atomic::AtomicBool,
    fail_read: std::sync::atomic::AtomicBool,
    fail_remove: std::sync::atomic::AtomicBool,
}

impl CredentialStore for TestCredentials {
    fn set(&self, reference: &str, value: &str) -> Result<(), String> {
        if self.fail_write.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("Native protected storage unavailable".into());
        }
        self.values
            .lock()
            .unwrap()
            .insert(reference.into(), value.into());
        Ok(())
    }
    fn get(&self, reference: &str) -> Result<String, String> {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.fail_read.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("Native protected storage unavailable".into());
        }
        self.values
            .lock()
            .unwrap()
            .get(reference)
            .cloned()
            .ok_or("Protected value missing".into())
    }
    fn remove(&self, reference: &str) -> Result<(), String> {
        if self.fail_remove.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("Credential cleanup unavailable".into());
        }
        self.values.lock().unwrap().remove(reference);
        Ok(())
    }
}

fn protected(name: &str, replacement: Option<&str>) -> SaveVariable {
    SaveVariable {
        id: None,
        expected_revision: None,
        name: name.into(),
        value: VariableInput::Protected {
            replacement: replacement.map(String::from),
        },
        workspace_ids: vec![WORKSPACE_A.into()],
    }
}

#[test]
fn protected_metadata_never_reads_or_contains_payload_or_credential_reference() {
    let temp = tempfile::tempdir().unwrap();
    let credentials = Arc::new(TestCredentials::default());
    let registry = VariablesRegistry::open_with_credentials(
        temp.path().join("variables"),
        credentials.clone(),
    )
    .unwrap();
    let saved = registry.save(
        protected("TOKEN", Some("disposable-secret-value")),
        &known(),
    );
    assert!(saved.is_ok(), "Protected definition must save");
    let saved = saved.unwrap().definition;
    credentials
        .fail_read
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let metadata = serde_json::to_value(registry.list(Some(WORKSPACE_A)).unwrap()).unwrap();
    assert_eq!(metadata[0]["kind"], "protected");
    assert!(metadata[0]["value"].is_null());
    let serialized = serde_json::to_string(&metadata).unwrap();
    assert!(!serialized.contains("disposable-secret-value"));
    assert!(!serialized.contains("credential"));
    assert_eq!(
        credentials.reads.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert!(registry.reveal(&saved.id, saved.revision).is_err());
    credentials
        .fail_read
        .store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        registry.reveal(&saved.id, saved.revision).unwrap(),
        "disposable-secret-value"
    );
    let bytes = std::fs::read(temp.path().join("variables/registry.json")).unwrap();
    assert!(!String::from_utf8(bytes)
        .unwrap()
        .contains("disposable-secret-value"));
}

#[test]
fn protected_grant_only_save_keeps_unrevealed_value_and_rejects_stale_reveal() {
    let temp = tempfile::tempdir().unwrap();
    let credentials = Arc::new(TestCredentials::default());
    let registry = VariablesRegistry::open_with_credentials(
        temp.path().join("variables"),
        credentials.clone(),
    )
    .unwrap();
    let saved = registry
        .save(protected("TOKEN", Some("original-disposable")), &known())
        .unwrap()
        .definition;
    let mut edit = protected("TOKEN", None);
    edit.id = Some(saved.id.clone());
    edit.expected_revision = Some(saved.revision);
    edit.workspace_ids = vec![WORKSPACE_B.into()];
    let updated = registry.save(edit, &known()).unwrap().definition;
    assert_eq!(
        credentials.reads.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert!(registry.reveal(&saved.id, saved.revision).is_err());
    assert_eq!(
        registry.reveal(&updated.id, updated.revision).unwrap(),
        "original-disposable"
    );
    assert!(registry.list(Some(WORKSPACE_A)).unwrap().is_empty());
}

// Stale script approval must be rejected before unlocking even a replacement credential.
#[test]
fn approved_resolution_checks_revisions_before_any_protected_read() {
    let temp = tempfile::tempdir().unwrap();
    let credentials = Arc::new(TestCredentials::default());
    let registry = VariablesRegistry::open_with_credentials(
        temp.path().join("variables"),
        credentials.clone(),
    )
    .unwrap();
    let first = registry
        .save(protected("TOKEN", Some("old-disposable")), &known())
        .unwrap()
        .definition;
    let approved = vec![VariableRevision {
        id: first.id.clone(),
        revision: first.revision,
    }];
    let mut replacement = protected("TOKEN", Some("new-disposable"));
    replacement.id = Some(first.id);
    replacement.expected_revision = Some(first.revision);
    registry.save(replacement, &known()).unwrap();
    credentials
        .fail_read
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(registry.resolve_approved(WORKSPACE_A, &approved).is_err());
    assert_eq!(
        credentials.reads.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the unapproved replacement must not be read from native storage"
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn script_approval_and_cleanup_records_never_resolve_or_serialize_protected_payloads() {
    use crate::agents::scripts::{self, ScriptConfig, ScriptPhase, ScriptSource};
    use crate::config::{
        workspace_config, AppConfig, ConfigManager, ShellAccessMode, WorkspaceConfig,
    };
    let temp = tempfile::tempdir().unwrap();
    let mut workspace =
        WorkspaceConfig::new(WORKSPACE_A.into(), "Variables QA".into(), 0, "main".into());
    workspace.main_agent.as_mut().unwrap().execution.shell.mode = ShellAccessMode::Full;
    workspace
        .main_agent
        .as_mut()
        .unwrap()
        .execution
        .shell
        .blocked_command_prefixes
        .clear();
    workspace_config::save(temp.path(), &workspace).unwrap();
    let mut index = crate::WorkspaceIndex::default();
    index.insert_config(temp.path().into(), &workspace);
    let mut state = crate::AppState::new_for_tests(
        ConfigManager::new_for_tests(AppConfig::default(), temp.path().join("app.json")),
        index,
    )
    .unwrap();
    let credentials = Arc::new(TestCredentials::default());
    state.variables = VariablesRegistry::open_with_credentials(
        temp.path().join("variables"),
        credentials.clone(),
    )
    .unwrap();
    let saved = state
        .variables
        .save(
            protected("TOKEN", Some("disposable-record-secret")),
            &known(),
        )
        .unwrap()
        .definition;
    credentials
        .fail_read
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let script = ScriptConfig {
        source: ScriptSource::Inline {
            content: "printf '%s' \"${CLAI_VAR_TOKEN?}\"".into(),
        },
        interpreter: "/bin/sh".into(),
        timeout_seconds: 5,
    };
    let snapshot = scripts::capture(&state, WORKSPACE_A, &script).unwrap();
    let consent = snapshot.consent();
    let approval = state
        .scheduler
        .lock()
        .await
        .scripts
        .entry(WORKSPACE_A.into())
        .or_default()
        .request_for(snapshot.clone(), "test", ScriptPhase::After, None);
    let intent = crate::agents::hooks::persist_intent(
        &state,
        WORKSPACE_A,
        "session",
        "run",
        &snapshot,
        Some(consent.clone()),
    )
    .unwrap();
    let persisted = std::fs::read_to_string(workspace_config::config_path(temp.path())).unwrap();
    let records = serde_json::to_string(&(
        consent,
        approval,
        intent,
        workspace_config::load(temp.path()).unwrap(),
        persisted,
    ))
    .unwrap();
    assert!(!records.contains("disposable-record-secret"));
    assert!(!records.contains("credentialReference"));
    assert!(records.contains(&saved.id));
    assert_eq!(
        credentials.reads.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    let result = snapshot.execute(&state, WORKSPACE_A).await;
    assert_eq!(result.started, Some(false));
    assert!(result.error.is_some());
    assert!(
        !result.condition_allows(),
        "credential failure must not become a successful condition"
    );
}

#[test]
fn failed_protected_registry_commit_removes_staged_replacement_and_preserves_previous() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("variables");
    let credentials = Arc::new(TestCredentials::default());
    let registry =
        VariablesRegistry::open_with_credentials(path.clone(), credentials.clone()).unwrap();
    let saved = registry
        .save(protected("TOKEN", Some("original-disposable")), &known())
        .unwrap()
        .definition;
    std::fs::rename(path.join("registry.json"), path.join("previous.json")).unwrap();
    std::fs::create_dir(path.join("registry.json")).unwrap();
    let mut edit = protected("TOKEN", Some("replacement-disposable"));
    edit.id = Some(saved.id.clone());
    edit.expected_revision = Some(saved.revision);
    assert!(registry.save(edit, &known()).is_err());
    assert_eq!(
        credentials.values.lock().unwrap().len(),
        1,
        "failed save must retire uncommitted staging"
    );
    assert_eq!(
        registry.reveal(&saved.id, saved.revision).unwrap(),
        "original-disposable"
    );
    std::fs::remove_dir(path.join("registry.json")).unwrap();
    std::fs::rename(path.join("previous.json"), path.join("registry.json")).unwrap();
    let reopened = VariablesRegistry::open_with_credentials(path, credentials).unwrap();
    assert_eq!(
        reopened.reveal(&saved.id, saved.revision).unwrap(),
        "original-disposable"
    );
}

#[test]
fn committed_replacement_retires_old_credential_and_reports_cleanup_failure_as_warning() {
    let temp = tempfile::tempdir().unwrap();
    let credentials = Arc::new(TestCredentials::default());
    let registry = VariablesRegistry::open_with_credentials(
        temp.path().join("variables"),
        credentials.clone(),
    )
    .unwrap();
    let saved = registry
        .save(protected("TOKEN", Some("original-disposable")), &known())
        .unwrap()
        .definition;
    let mut edit = protected("TOKEN", Some("replacement-disposable"));
    edit.id = Some(saved.id.clone());
    edit.expected_revision = Some(saved.revision);
    let updated = registry.save(edit, &known()).unwrap().definition;
    assert_eq!(
        credentials.values.lock().unwrap().len(),
        1,
        "committed replacement must retire old credential"
    );
    credentials
        .fail_remove
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let mut edit = protected("TOKEN", Some("last-disposable"));
    edit.id = Some(updated.id.clone());
    edit.expected_revision = Some(updated.revision);
    let outcome = serde_json::to_value(registry.save(edit, &known()).unwrap()).unwrap();
    assert!(
        outcome["warning"].as_str().is_some(),
        "cleanup failure needs an explicit committed-save warning"
    );
    let current = registry.list(None).unwrap().remove(0);
    assert_eq!(
        registry.reveal(&current.id, current.revision).unwrap(),
        "last-disposable"
    );
}

#[test]
fn vault_write_failure_and_invalid_type_change_preserve_previous_definition() {
    let temp = tempfile::tempdir().unwrap();
    let credentials = Arc::new(TestCredentials::default());
    let registry = VariablesRegistry::open_with_credentials(
        temp.path().join("variables"),
        credentials.clone(),
    )
    .unwrap();
    let saved = registry
        .save(create("VALUE", "literal"), &known())
        .unwrap()
        .definition;
    let mut missing = protected("VALUE", None);
    missing.id = Some(saved.id.clone());
    missing.expected_revision = Some(saved.revision);
    assert!(registry.save(missing, &known()).is_err());
    credentials
        .fail_write
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let mut replacement = protected("VALUE", Some("disposable"));
    replacement.id = Some(saved.id);
    replacement.expected_revision = Some(saved.revision);
    assert!(registry.save(replacement, &known()).is_err());
    assert_eq!(
        registry.list(None).unwrap()[0].value,
        Some(PlainValue::Text("literal".into()))
    );
}

#[test]
fn protected_to_text_requires_explicit_text_and_never_reads_the_secret() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("variables");
    let credentials = Arc::new(TestCredentials::default());
    let registry =
        VariablesRegistry::open_with_credentials(path.clone(), credentials.clone()).unwrap();
    let saved = registry
        .save(protected("TOKEN", Some("original-disposable")), &known())
        .unwrap()
        .definition;
    credentials
        .fail_read
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let mut edit = create("TOKEN", "replacement plain text");
    edit.id = Some(saved.id);
    edit.expected_revision = Some(saved.revision);
    let updated = registry.save(edit, &known()).unwrap().definition;
    assert_eq!(
        updated.value,
        Some(PlainValue::Text("replacement plain text".into()))
    );
    assert_eq!(
        credentials.reads.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert!(credentials.values.lock().unwrap().is_empty());
    assert!(
        !String::from_utf8(std::fs::read(path.join("registry.json")).unwrap())
            .unwrap()
            .contains("original-disposable")
    );
}

#[test]
fn text_to_protected_commit_contains_neither_plaintext_nor_protected_payload() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("variables");
    let credentials = Arc::new(TestCredentials::default());
    let registry =
        VariablesRegistry::open_with_credentials(path.clone(), credentials.clone()).unwrap();
    let saved = registry
        .save(create("TOKEN", "previous plain value"), &known())
        .unwrap()
        .definition;
    let mut edit = protected("TOKEN", Some("new-disposable"));
    edit.id = Some(saved.id);
    edit.expected_revision = Some(saved.revision);
    registry.save(edit, &known()).unwrap();
    let encoded = String::from_utf8(std::fs::read(path.join("registry.json")).unwrap()).unwrap();
    assert!(!encoded.contains("previous plain value"));
    assert!(!encoded.contains("new-disposable"));
}

#[test]
fn malformed_protected_disk_metadata_cannot_leak_plaintext_through_lists() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("registry.json"), serde_json::to_vec(&serde_json::json!([{
        "id": uuid::Uuid::new_v4().to_string(), "revision": 1, "name": "TOKEN", "kind": "protected",
        "value": "malformed-plaintext", "workspaceIds": []
    }])).unwrap()).unwrap();
    assert!(
        VariablesRegistry::open(temp.path().into()).is_err(),
        "invalid protected disk data must fail closed before metadata is exposed"
    );
}

#[test]
fn uncertain_directory_durability_preserves_both_credentials_and_reports_committed_state() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("variables");
    let credentials = Arc::new(TestCredentials::default());
    let mut registry =
        VariablesRegistry::open_with_credentials(path.clone(), credentials.clone()).unwrap();
    let saved = registry
        .save(protected("TOKEN", Some("original-disposable")), &known())
        .unwrap()
        .definition;
    registry.directory_sync = |_| Err(std::io::Error::other("fixture directory sync failure"));
    let mut edit = protected("TOKEN", Some("replacement-disposable"));
    edit.id = Some(saved.id.clone());
    edit.expected_revision = Some(saved.revision);
    let result = registry.save(edit, &known()).unwrap();
    assert!(
        result.warning.is_some(),
        "uncertain durability must be reported"
    );
    assert_eq!(
        credentials.values.lock().unwrap().len(),
        2,
        "both recoverable references must remain available"
    );
    assert_eq!(
        registry
            .reveal(&result.definition.id, result.definition.revision)
            .unwrap(),
        "replacement-disposable"
    );
    let reopened = VariablesRegistry::open_with_credentials(path, credentials).unwrap();
    assert_eq!(reopened.list(None).unwrap(), vec![result.definition]);
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn native_command(
    workspace: &std::path::Path,
    home: &std::path::Path,
    script: &str,
) -> crate::assistant::sandbox::SandboxCommand {
    use crate::assistant::sandbox::*;
    SandboxCommand {
        argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
        cwd: workspace.into(),
        timeout_ms: 5000,
        max_output_chars: 2000,
        profile: SandboxProfile::for_test(
            workspace.into(),
            vec![SandboxPathGrant {
                host_path: home.into(),
                access: SandboxPathAccess::ReadWrite,
            }],
            SandboxNetworkMode::Disabled,
            SandboxSessionBusMode::Deny,
            SandboxEnv::filtered_from_iter(
                [
                    ("PATH", "/usr/bin:/bin"),
                    ("CLAI_VAR_TOKEN", "inherited-bypass"),
                ],
                home,
                SandboxSessionBusMode::Deny,
            ),
            None,
        ),
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn protected_shell_results_suppress_payload_and_keep_surrounding_output() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let registry = VariablesRegistry::open_with_credentials(
        temp.path().join("profile/variables"),
        Arc::new(TestCredentials::default()),
    )
    .unwrap();
    registry
        .save(
            protected("TOKEN", Some("disposable-shell-secret")),
            &known(),
        )
        .unwrap();
    let command = native_command(&workspace, temp.path(), "printf 'before:%s:after' \"$CLAI_VAR_TOKEN\"; printf 'before:%s:after' \"$CLAI_VAR_TOKEN\" >&2");
    let output =
        crate::assistant::tools::local::run_workspace_command(&registry, WORKSPACE_A, command)
            .await
            .unwrap();
    assert!(output.success);
    assert!(
        !output.stdout.contains("disposable-shell-secret"),
        "stdout must suppress protected values"
    );
    assert!(
        !output.stderr.contains("disposable-shell-secret"),
        "stderr must suppress protected values"
    );
    assert!(output.stdout.starts_with("before:") && output.stdout.ends_with(":after"));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn unassigned_shell_cannot_read_registry_through_broad_grants_or_aliases() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let directory = temp.path().join("profile/variables");
    let registry = VariablesRegistry::open(directory.clone()).unwrap();
    registry
        .save(create("MESSAGE", "private-registry-text"), &known())
        .unwrap();
    let alias = temp.path().join("registry-alias");
    std::os::unix::fs::symlink(&directory, &alias).unwrap();
    for file in [directory.join("registry.json"), alias.join("registry.json")] {
        let command = native_command(
            &workspace,
            temp.path(),
            &format!("cat '{}'", file.display()),
        );
        let output =
            crate::assistant::tools::local::run_workspace_command(&registry, WORKSPACE_B, command)
                .await
                .unwrap();
        assert!(
            !output.success,
            "broad grants must not expose the Variables registry"
        );
        assert!(!output.stdout.contains("private-registry-text"));
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn revoked_snapshot_cannot_start_a_later_process() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let registry = VariablesRegistry::open(temp.path().join("profile/variables")).unwrap();
    let saved = registry
        .save(create("MESSAGE", "one"), &known())
        .unwrap()
        .definition;
    let snapshot = registry.resolve(WORKSPACE_A).unwrap();
    let mut edit = create("MESSAGE", "one");
    edit.id = Some(saved.id);
    edit.expected_revision = Some(saved.revision);
    edit.workspace_ids.clear();
    registry.save(edit, &known()).unwrap();
    let mut command = native_command(&workspace, temp.path(), "touch SHOULD_NOT_START");
    command.profile.variables = Some(snapshot);
    let result = crate::assistant::sandbox::run_command(command).await;
    assert!(
        result.is_err(),
        "revoked access must fail at actual process admission"
    );
    assert!(!workspace.join("SHOULD_NOT_START").exists());
}

#[test]
fn unassigned_resolution_does_not_read_credentials_or_leak_payloads_in_debug() {
    let temp = tempfile::tempdir().unwrap();
    let credentials = Arc::new(TestCredentials::default());
    let registry = VariablesRegistry::open_with_credentials(
        temp.path().join("variables"),
        credentials.clone(),
    )
    .unwrap();
    registry
        .save(
            protected("TOKEN", Some("disposable-shell-secret")),
            &known(),
        )
        .unwrap();
    let assigned = registry.resolve(WORKSPACE_A).unwrap();
    assert!(!format!("{assigned:?}").contains("disposable-shell-secret"));
    credentials
        .reads
        .store(0, std::sync::atomic::Ordering::SeqCst);
    credentials
        .fail_read
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let unassigned = registry.resolve(WORKSPACE_B).unwrap();
    assert_eq!(unassigned.iter().count(), 0);
    assert_eq!(
        credentials.reads.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert!(
        registry.resolve(WORKSPACE_A).is_err(),
        "assigned missing/unavailable credential must fail visibly"
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn protected_output_is_suppressed_before_character_truncation() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let registry = VariablesRegistry::open_with_credentials(
        temp.path().join("profile/variables"),
        Arc::new(TestCredentials::default()),
    )
    .unwrap();
    registry
        .save(
            protected("TOKEN", Some("disposable-shell-secret")),
            &known(),
        )
        .unwrap();
    let mut command = native_command(&workspace, temp.path(), "printf '%s' \"$CLAI_VAR_TOKEN\"");
    command.max_output_chars = 7;
    let output =
        crate::assistant::tools::local::run_workspace_command(&registry, WORKSPACE_A, command)
            .await
            .unwrap();
    assert!(
        !output.stdout.contains("disposa"),
        "retention must not preserve a truncated secret prefix"
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn registry_cannot_be_relocated_or_hardlinked_out_of_its_denial() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let profile = temp.path().join("profile");
    let directory = profile.join("variables");
    let registry = VariablesRegistry::open(directory.clone()).unwrap();
    registry
        .save(create("MESSAGE", "private-registry-text"), &known())
        .unwrap();
    let moved = temp.path().join("moved-profile");
    let command = native_command(
        &workspace,
        temp.path(),
        &format!(
            "mv '{}' '{}'; cat '{}/variables/registry.json'",
            profile.display(),
            moved.display(),
            moved.display()
        ),
    );
    let output =
        crate::assistant::tools::local::run_workspace_command(&registry, WORKSPACE_B, command)
            .await
            .unwrap();
    assert!(
        !output.stdout.contains("private-registry-text"),
        "moving a parent must not bypass registry isolation"
    );
    assert!(
        directory.join("registry.json").exists(),
        "registry ancestors must remain in place"
    );
    let link = workspace.join("registry-copy");
    let command = native_command(
        &workspace,
        temp.path(),
        &format!(
            "ln '{}' '{}'; cat '{}'",
            directory.join("registry.json").display(),
            link.display(),
            link.display()
        ),
    );
    let output =
        crate::assistant::tools::local::run_workspace_command(&registry, WORKSPACE_B, command)
            .await
            .unwrap();
    assert!(
        !output.stdout.contains("private-registry-text"),
        "hardlink aliases must not expose definitions"
    );
    assert!(
        !link.exists(),
        "registry files must not be linked into an accessible workspace"
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn child_serializes_boolean_number_and_preserves_base_environment() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let registry = VariablesRegistry::open(temp.path().join("variables")).unwrap();
    for (name, value) in [
        ("ENABLED", VariableInput::Boolean { value: false }),
        (
            "COUNT",
            VariableInput::Number {
                value: "-12.5".into(),
            },
        ),
    ] {
        registry
            .save(
                SaveVariable {
                    id: None,
                    expected_revision: None,
                    name: name.into(),
                    value,
                    workspace_ids: vec![WORKSPACE_A.into()],
                },
                &known(),
            )
            .unwrap();
    }
    let command = native_command(
        &workspace,
        temp.path(),
        "printf '%s|%s|%s' \"${CLAI_VAR_ENABLED?}\" \"${CLAI_VAR_COUNT?}\" \"$PATH\"",
    );
    let output =
        crate::assistant::tools::local::run_workspace_command(&registry, WORKSPACE_A, command)
            .await
            .unwrap();
    assert!(output.success);
    assert_eq!(output.stdout, "false|-12.5|/usr/bin:/bin");
    let command = native_command(
        &workspace,
        temp.path(),
        "printf '%s' \"${CLAI_VAR_TOKEN-unset}\"",
    );
    let output =
        crate::assistant::tools::local::run_workspace_command(&registry, WORKSPACE_B, command)
            .await
            .unwrap();
    assert_eq!(output.stdout, "unset");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn unused_variables_do_not_require_an_available_registry_path_for_legacy_shell() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let unavailable = temp.path().join("profile-alias");
    std::os::unix::fs::symlink(temp.path().join("missing-profile"), &unavailable).unwrap();
    let registry = VariablesRegistry::open(unavailable.join("variables")).unwrap();
    assert!(registry.list(None).unwrap().is_empty());
    let command = native_command(&workspace, temp.path(), "printf legacy");
    let result =
        crate::assistant::tools::local::run_workspace_command(&registry, WORKSPACE_A, command)
            .await;
    assert!(
        result.is_ok(),
        "unused Variables must not block existing shell execution"
    );
    assert_eq!(result.unwrap().stdout, "legacy");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn first_definition_invalidates_an_empty_snapshot_before_process_admission() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let registry = VariablesRegistry::open(temp.path().join("profile/variables")).unwrap();
    let snapshot = registry.resolve(WORKSPACE_A).unwrap();
    registry.save(create("MESSAGE", "new"), &known()).unwrap();
    let mut command = native_command(&workspace, temp.path(), "touch SHOULD_NOT_START");
    command.profile.variables = Some(snapshot);
    assert!(crate::assistant::sandbox::run_command(command)
        .await
        .is_err());
    assert!(!workspace.join("SHOULD_NOT_START").exists());
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn an_already_running_unassigned_child_cannot_read_a_later_first_definition() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let directory = temp.path().join("profile/variables");
    let registry = Arc::new(VariablesRegistry::open(directory.clone()).unwrap());
    let command = native_command(
        &workspace,
        temp.path(),
        &format!(
            "touch ready; while ! test -f continue; do sleep 0.01; done; cat '{}'",
            directory.join("registry.json").display()
        ),
    );
    let child = tokio::spawn({
        let registry = registry.clone();
        async move {
            crate::assistant::tools::local::run_workspace_command(&registry, WORKSPACE_B, command)
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
    registry
        .save(create("MESSAGE", "future-private-registry-value"), &known())
        .unwrap();
    std::fs::write(workspace.join("continue"), "").unwrap();
    let output = child.await.unwrap().unwrap();
    assert!(
        !output.stdout.contains("future-private-registry-value"),
        "registry isolation must survive first-definition creation after launch"
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn registry_profile_alias_cannot_redirect_app_writes_outside_running_child_isolation() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let original = temp.path().join("original-profile");
    let redirected = temp.path().join("redirected-profile");
    for directory in [&workspace, &original, &redirected] {
        std::fs::create_dir(directory).unwrap();
    }
    let alias = temp.path().join("profile-alias");
    std::os::unix::fs::symlink(&original, &alias).unwrap();
    let registry = Arc::new(VariablesRegistry::open(alias.join("variables")).unwrap());
    let command = native_command(&workspace, temp.path(), &format!(
        "rm '{}'; ln -s '{}' '{}'; touch ready; while ! test -f continue; do sleep 0.01; done; cat '{}'",
        alias.display(), redirected.display(), alias.display(), redirected.join("variables/registry.json").display()));
    let child = tokio::spawn({
        let registry = registry.clone();
        async move {
            crate::assistant::tools::local::run_workspace_command(&registry, WORKSPACE_B, command)
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
    registry
        .save(create("MESSAGE", "alias-private-registry-value"), &known())
        .unwrap();
    std::fs::write(workspace.join("continue"), "").unwrap();
    let output = child.await.unwrap().unwrap();
    assert!(
        !output.stdout.contains("alias-private-registry-value"),
        "a configured symlink must not redirect future app writes outside isolation"
    );
}
