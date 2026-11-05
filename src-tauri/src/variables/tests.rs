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
