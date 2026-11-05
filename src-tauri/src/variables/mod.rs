//! Static definitions managed by Settings and resolved for a bound workspace.
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;
use ts_rs::TS;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, export_to = "bindings.ts")]
pub struct VariableMetadata {
    pub id: String,
    pub revision: u32,
    pub name: String,
    pub value: String,
    pub workspace_ids: Vec<String>,
}

#[derive(Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, export_to = "bindings.ts")]
pub struct SaveVariable {
    pub id: Option<String>,
    pub expected_revision: Option<u32>,
    pub name: String,
    pub value: String,
    pub workspace_ids: Vec<String>,
}

pub struct VariablesRegistry {
    directory: PathBuf,
    definitions: Mutex<Vec<VariableMetadata>>,
}

impl VariablesRegistry {
    pub fn open(directory: PathBuf) -> Result<Self, String> {
        let definitions = match std::fs::read(directory.join("registry.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|_| "Variables registry could not be read".to_string())?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(_) => return Err("Variables registry is unavailable".into()),
        };
        Ok(Self {
            directory,
            definitions: Mutex::new(definitions),
        })
    }
    pub fn list(&self, workspace_id: Option<&str>) -> Result<Vec<VariableMetadata>, String> {
        let definitions = self
            .definitions
            .lock()
            .map_err(|_| "Variables registry lock is unavailable")?;
        Ok(definitions
            .iter()
            .filter(|d| workspace_id.is_none_or(|id| d.workspace_ids.iter().any(|w| w == id)))
            .cloned()
            .collect())
    }
    pub fn save(
        &self,
        input: SaveVariable,
        known_workspaces: &HashSet<String>,
    ) -> Result<VariableMetadata, String> {
        let mut definitions = self
            .definitions
            .lock()
            .map_err(|_| "Variables registry lock is unavailable")?;
        let mut chars = input.name.bytes();
        if !chars
            .next()
            .is_some_and(|b| b.is_ascii_uppercase() || b == b'_')
            || !chars.all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        {
            return Err("Use an uppercase name beginning with a letter or underscore".into());
        }
        if input.value.contains('\0') {
            return Err("Values cannot contain NUL characters".into());
        }
        if input
            .workspace_ids
            .iter()
            .any(|id| !known_workspaces.contains(id) || uuid::Uuid::parse_str(id).is_err())
        {
            return Err("A selected workspace is unavailable; reload and try again".into());
        }
        let previous = match input.id.as_ref() {
            Some(id) => Some(
                definitions
                    .iter()
                    .find(|d| &d.id == id)
                    .ok_or("This field no longer exists; reload before saving")?,
            ),
            None => None,
        };
        if previous.map(|d| d.revision) != input.expected_revision {
            return Err("This field changed; reload before saving".into());
        }
        if definitions
            .iter()
            .any(|d| d.name == input.name && Some(&d.id) != input.id.as_ref())
        {
            return Err("A field with this name already exists".into());
        }
        let revision = previous.map_or(Ok(1), |d| {
            d.revision
                .checked_add(1)
                .ok_or("Field revision limit reached")
        })?;
        let mut workspace_ids = input.workspace_ids;
        workspace_ids.sort();
        workspace_ids.dedup();
        let saved = VariableMetadata {
            id: input.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            revision,
            name: input.name,
            value: input.value,
            workspace_ids,
        };
        let mut candidate = definitions.clone();
        candidate.retain(|d| d.id != saved.id);
        candidate.push(saved.clone());
        std::fs::create_dir_all(&self.directory).map_err(|_| "Variables could not be saved")?;
        let staging = self.directory.join(format!("{}.tmp", uuid::Uuid::new_v4()));
        let bytes =
            serde_json::to_vec_pretty(&candidate).map_err(|_| "Variables could not be encoded")?;
        let result = std::fs::write(&staging, bytes)
            .and_then(|()| std::fs::rename(&staging, self.directory.join("registry.json")));
        if result.is_err() {
            let _ = std::fs::remove_file(&staging);
            return Err("Variables could not be saved; previous values remain unchanged".into());
        }
        *definitions = candidate;
        Ok(saved)
    }
}

#[cfg(test)]
mod tests;
