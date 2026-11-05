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
            value: "literal '$HOME'\n$(printf inert)".into(),
            workspace_ids: vec![WORKSPACE_A.into()],
        },
        &known(),
    );
    assert!(result.is_ok(), "Text definition must save: {result:?}");
    let saved = result.unwrap();
    assert_eq!(saved.revision, 1);
    assert!(!saved.id.is_empty());
    drop(registry);
    let reopened = VariablesRegistry::open(path).unwrap();
    assert_eq!(
        reopened.list(Some(WORKSPACE_A)).unwrap(),
        vec![saved.clone()]
    );
    assert!(reopened.list(Some(WORKSPACE_B)).unwrap().is_empty());
    assert_eq!(saved.value, "literal '$HOME'\n$(printf inert)");
}

fn create(name: &str, value: &str) -> SaveVariable {
    SaveVariable {
        id: None,
        expected_revision: None,
        name: name.into(),
        value: value.into(),
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
    let original = registry.save(create("FIRST", "one"), &known()).unwrap();
    assert!(registry
        .save(create("FIRST", "duplicate"), &known())
        .is_err());
    let mut edit = create("RENAMED", "two");
    edit.id = Some(original.id.clone());
    edit.expected_revision = Some(original.revision);
    let updated = registry.save(edit, &known()).unwrap();
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
    let original = registry.save(create("FIRST", "one"), &known()).unwrap();
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
    let original = registry.save(create("FIRST", "one"), &known()).unwrap();
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
