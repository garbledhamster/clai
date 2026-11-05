use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, export_to = "bindings.ts")]
pub enum VariableKind {
    #[default]
    Text,
    Boolean,
    Number,
    Protected,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(untagged)]
#[ts(export, export_to = "bindings.ts")]
pub enum PlainValue {
    Text(String),
    Boolean(bool),
    Number(f64),
}

#[derive(Deserialize, TS)]
#[serde(tag = "type", rename_all = "camelCase")]
#[ts(export, export_to = "bindings.ts")]
pub enum VariableInput {
    Text { value: String },
    Boolean { value: bool },
    Number { value: String },
    Protected { replacement: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, export_to = "bindings.ts")]
pub struct VariableMetadata {
    pub id: String,
    pub revision: u32,
    pub name: String,
    #[serde(default)]
    pub kind: VariableKind,
    pub value: Option<PlainValue>,
    pub workspace_ids: Vec<String>,
}

#[derive(Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, export_to = "bindings.ts")]
pub struct SaveVariable {
    pub id: Option<String>,
    pub expected_revision: Option<u32>,
    pub name: String,
    pub value: VariableInput,
    pub workspace_ids: Vec<String>,
}

#[derive(Debug, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, export_to = "bindings.ts")]
pub struct VariableSaveResult {
    pub definition: VariableMetadata,
    pub warning: Option<String>,
}
