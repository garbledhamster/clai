use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct VariableRevision {
    pub id: String,
    pub revision: u32,
}

pub(super) fn identity_for(
    definitions: &[super::StoredDefinition],
    workspace_id: &str,
) -> Vec<VariableRevision> {
    let mut revisions = definitions
        .iter()
        .filter(|d| d.metadata.workspace_ids.iter().any(|id| id == workspace_id))
        .map(|d| VariableRevision {
            id: d.metadata.id.clone(),
            revision: d.metadata.revision,
        })
        .collect::<Vec<_>>();
    revisions.sort_by(|a, b| a.id.cmp(&b.id));
    revisions
}

/// Child-only data. Deliberately not serializable or value-bearing in Debug.
#[derive(Clone)]
pub(crate) struct ResolvedVariables {
    pub(super) values: BTreeMap<String, String>,
    pub(super) protected: Vec<String>,
    pub(super) directory: std::path::PathBuf,
    pub(super) workspace_id: String,
    pub(super) revisions: Vec<VariableRevision>,
    pub(super) in_use: bool,
    pub(super) definitions: Arc<Mutex<Vec<super::StoredDefinition>>>,
}

impl std::fmt::Debug for ResolvedVariables {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ResolvedVariables { values: [redacted] }")
    }
}

impl ResolvedVariables {
    pub(crate) fn protected_values(&self) -> Vec<String> {
        self.protected.clone()
    }
    pub(crate) fn in_use(&self) -> bool {
        self.in_use
    }
    pub(crate) fn registry_directory(&self) -> &std::path::Path {
        &self.directory
    }
    pub(crate) fn admit<T>(&self, spawn: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
        let definitions = self
            .definitions
            .lock()
            .map_err(|_| "Variables registry lock is unavailable")?;
        if self.in_use != !definitions.is_empty()
            || self.revisions != identity_for(&definitions, &self.workspace_id)
        {
            return Err(
                "Variable values or workspace access changed; resolve again before starting".into(),
            );
        }
        // The same lock orders Settings publication with this synchronous spawn.
        spawn()
    }
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.values
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }
}
