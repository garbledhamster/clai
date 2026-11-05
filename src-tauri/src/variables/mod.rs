//! Static definitions managed by Settings and resolved for a bound workspace.
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
mod credentials;
mod redaction;
mod runtime;
pub(crate) use redaction::ProtectedOutput;
pub(crate) use runtime::ResolvedVariables;

mod types;
use credentials::CredentialStore;
pub use types::{
    PlainValue, SaveVariable, VariableInput, VariableKind, VariableMetadata, VariableSaveResult,
};

pub struct VariablesRegistry {
    directory: PathBuf,
    definitions: Arc<Mutex<Vec<StoredDefinition>>>,
    credentials: Arc<dyn CredentialStore>,
    directory_sync: fn(&std::path::Path) -> std::io::Result<()>,
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredDefinition {
    #[serde(flatten)]
    metadata: VariableMetadata,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credential_reference: Option<String>,
}

impl VariablesRegistry {
    pub(crate) fn resolve(&self, workspace_id: &str) -> Result<ResolvedVariables, String> {
        let definitions = self
            .definitions
            .lock()
            .map_err(|_| "Variables registry lock is unavailable")?;
        let mut values = std::collections::BTreeMap::new();
        let mut protected = Vec::new();
        for definition in definitions
            .iter()
            .filter(|d| d.metadata.workspace_ids.iter().any(|id| id == workspace_id))
        {
            let value = if definition.metadata.kind == VariableKind::Protected {
                let reference = definition
                    .credential_reference
                    .as_deref()
                    .ok_or("Protected value is unavailable")?;
                let value = self.credentials.get(reference)?;
                protected.push(value.clone());
                value
            } else {
                match definition
                    .metadata
                    .value
                    .as_ref()
                    .ok_or("Variable value is unavailable")?
                {
                    PlainValue::Text(value) => value.clone(),
                    PlainValue::Boolean(value) => value.to_string(),
                    PlainValue::Number(value) => value.to_string(),
                }
            };
            values.insert(format!("CLAI_VAR_{}", definition.metadata.name), value);
        }
        Ok(ResolvedVariables {
            values,
            protected,
            directory: self.directory.clone(),
            workspace_id: workspace_id.into(),
            revisions: runtime::identity_for(&definitions, workspace_id),
            in_use: !definitions.is_empty(),
            definitions: self.definitions.clone(),
        })
    }
    pub fn open(directory: PathBuf) -> Result<Self, String> {
        Self::open_with_credentials(directory, Arc::new(credentials::NativeCredentials))
    }

    pub fn reveal(&self, id: &str, expected_revision: u32) -> Result<String, String> {
        let definitions = self
            .definitions
            .lock()
            .map_err(|_| "Variables registry lock is unavailable")?;
        let field = definitions
            .iter()
            .find(|d| d.metadata.id == id && d.metadata.revision == expected_revision)
            .ok_or("This field changed; reload before revealing")?;
        let reference = field
            .credential_reference
            .as_deref()
            .filter(|_| field.metadata.kind == VariableKind::Protected)
            .ok_or("This field is not a protected string")?;
        self.credentials.get(reference)
    }

    fn open_with_credentials(
        directory: PathBuf,
        credentials: Arc<dyn CredentialStore>,
    ) -> Result<Self, String> {
        let definitions: Vec<StoredDefinition> =
            match std::fs::read(directory.join("registry.json")) {
                Ok(bytes) => serde_json::from_slice(&bytes)
                    .map_err(|_| "Variables registry could not be read".to_string())?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(_) => return Err("Variables registry is unavailable".into()),
            };
        let mut ids = HashSet::new();
        let mut names = HashSet::new();
        for definition in &definitions {
            let metadata = &definition.metadata;
            let shape_valid = match (
                &metadata.kind,
                &metadata.value,
                &definition.credential_reference,
            ) {
                (VariableKind::Text, Some(PlainValue::Text(value)), None) => !value.contains('\0'),
                (VariableKind::Boolean, Some(PlainValue::Boolean(_)), None) => true,
                (VariableKind::Number, Some(PlainValue::Number(value)), None) => value.is_finite(),
                (VariableKind::Protected, None, Some(reference)) => {
                    uuid::Uuid::parse_str(reference).is_ok()
                }
                _ => false,
            };
            if !shape_valid
                || metadata.revision == 0
                || uuid::Uuid::parse_str(&metadata.id).is_err()
                || validate_name(&metadata.name).is_err()
                || !ids.insert(&metadata.id)
                || !names.insert(&metadata.name)
                || metadata
                    .workspace_ids
                    .iter()
                    .any(|id| uuid::Uuid::parse_str(id).is_err())
            {
                return Err("Variables registry contains invalid definitions; restore a valid registry before continuing".into());
            }
        }
        Ok(Self {
            directory,
            definitions: Arc::new(Mutex::new(definitions)),
            credentials,
            directory_sync: |path| std::fs::File::open(path)?.sync_all(),
        })
    }
    pub fn list(&self, workspace_id: Option<&str>) -> Result<Vec<VariableMetadata>, String> {
        let definitions = self
            .definitions
            .lock()
            .map_err(|_| "Variables registry lock is unavailable")?;
        Ok(definitions
            .iter()
            .filter(|d| {
                workspace_id.is_none_or(|id| d.metadata.workspace_ids.iter().any(|w| w == id))
            })
            .map(|d| d.metadata.clone())
            .collect())
    }
    pub fn save(
        &self,
        input: SaveVariable,
        known_workspaces: &HashSet<String>,
    ) -> Result<VariableSaveResult, String> {
        let mut definitions = self
            .definitions
            .lock()
            .map_err(|_| "Variables registry lock is unavailable")?;
        let previous = validate_edit(&definitions, &input, known_workspaces)?;
        let old_reference = previous.and_then(|d| d.credential_reference.clone());
        let prepared = self.prepare_definition(input, previous)?;
        let saved = prepared.metadata.clone();
        let new_reference = prepared.credential_reference.clone();
        let mut candidate = definitions.clone();
        candidate.retain(|d| d.metadata.id != saved.id);
        candidate.push(prepared);
        let commit = match self.persist(&candidate) {
            Ok(commit) => commit,
            Err(error) => {
                if new_reference != old_reference {
                    if let Some(reference) = new_reference {
                        if self.credentials.remove(&reference).is_err() {
                            return Err(format!(
                                "{error}. Unused staged credential could not be removed"
                            ));
                        }
                    }
                }
                return Err(error);
            }
        };
        *definitions = candidate;
        let warning = match commit {
            RegistryCommit::Uncertain => Some("Saved locally; durable storage could not be confirmed. Review storage health before restarting. Previous protected credentials were retained.".into()),
            RegistryCommit::Durable if old_reference != new_reference => old_reference
                .filter(|reference| self.credentials.remove(reference).is_err())
                .map(|_| "Saved; unused previous protected credential could not be removed".to_string()),
            RegistryCommit::Durable => None,
        };
        Ok(VariableSaveResult {
            definition: saved,
            warning,
        })
    }

    fn prepare_definition(
        &self,
        input: SaveVariable,
        previous: Option<&StoredDefinition>,
    ) -> Result<StoredDefinition, String> {
        let revision = previous.map_or(Ok(1), |d| {
            d.metadata
                .revision
                .checked_add(1)
                .ok_or("Field revision limit reached")
        })?;
        let mut workspace_ids = input.workspace_ids;
        workspace_ids.sort();
        workspace_ids.dedup();
        let mut credential_reference = None;
        let (kind, value) = match input.value {
            VariableInput::Text { value } => (VariableKind::Text, Some(PlainValue::Text(value))),
            VariableInput::Boolean { value } => {
                (VariableKind::Boolean, Some(PlainValue::Boolean(value)))
            }
            VariableInput::Number { value } => {
                let number = value
                    .trim()
                    .parse::<f64>()
                    .map_err(|_| "Enter a finite number")?;
                if !number.is_finite() {
                    return Err("Enter a finite number".into());
                }
                (VariableKind::Number, Some(PlainValue::Number(number)))
            }
            VariableInput::Protected { replacement } => {
                credential_reference = Some(match replacement {
                    Some(value) => {
                        let reference = uuid::Uuid::new_v4().to_string();
                        self.credentials.set(&reference, &value)?;
                        reference
                    }
                    None => previous
                        .filter(|d| d.metadata.kind == VariableKind::Protected)
                        .and_then(|d| d.credential_reference.clone())
                        .ok_or("Enter a protected replacement for this type change")?,
                });
                (VariableKind::Protected, None)
            }
        };
        Ok(StoredDefinition {
            metadata: VariableMetadata {
                id: input.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                revision,
                name: input.name,
                kind,
                value,
                workspace_ids,
            },
            credential_reference,
        })
    }

    fn persist(&self, candidate: &[StoredDefinition]) -> Result<RegistryCommit, String> {
        use std::io::Write;
        std::fs::create_dir_all(&self.directory).map_err(|_| "Variables could not be saved")?;
        let staging = self.directory.join(format!("{}.tmp", uuid::Uuid::new_v4()));
        let bytes =
            serde_json::to_vec_pretty(&candidate).map_err(|_| "Variables could not be encoded")?;
        let result = (|| -> std::io::Result<()> {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&staging)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(&staging, self.directory.join("registry.json"))
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&staging);
            return Err("Variables could not be saved; previous values remain unchanged".into());
        }
        // Rename is committed locally. A directory sync failure must retain both
        // credential references: recovery may contain either registry revision.
        Ok(if (self.directory_sync)(&self.directory).is_ok() {
            RegistryCommit::Durable
        } else {
            RegistryCommit::Uncertain
        })
    }
}

#[derive(Clone, Copy)]
enum RegistryCommit {
    Durable,
    Uncertain,
}

fn validate_edit<'a>(
    definitions: &'a [StoredDefinition],
    input: &SaveVariable,
    known: &HashSet<String>,
) -> Result<Option<&'a StoredDefinition>, String> {
    validate_name(&input.name)?;
    if matches!(&input.value, VariableInput::Text {value} | VariableInput::Protected {replacement: Some(value)} if value.contains('\0'))
    {
        return Err("Values cannot contain NUL characters".into());
    }
    if input
        .workspace_ids
        .iter()
        .any(|id| !known.contains(id) || uuid::Uuid::parse_str(id).is_err())
    {
        return Err("A selected workspace is unavailable; reload and try again".into());
    }
    let previous = match input.id.as_ref() {
        Some(id) => Some(
            definitions
                .iter()
                .find(|d| &d.metadata.id == id)
                .ok_or("This field no longer exists; reload before saving")?,
        ),
        None => None,
    };
    if previous.map(|d| d.metadata.revision) != input.expected_revision {
        return Err("This field changed; reload before saving".into());
    }
    if definitions
        .iter()
        .any(|d| d.metadata.name == input.name && Some(&d.metadata.id) != input.id.as_ref())
    {
        return Err("A field with this name already exists".into());
    }
    Ok(previous)
}

fn validate_name(name: &str) -> Result<(), String> {
    let mut chars = name.bytes();
    if !chars
        .next()
        .is_some_and(|b| b.is_ascii_uppercase() || b == b'_')
        || !chars.all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err("Use an uppercase name beginning with a letter or underscore".into());
    }
    Ok(())
}

#[cfg(test)]
mod native_tests;
#[cfg(test)]
mod tests;
