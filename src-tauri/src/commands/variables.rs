//! Settings-only management. These commands are never registered as agent tools.
use crate::variables::{SaveVariable, VariableMetadata};
use crate::AppState;
use std::collections::HashSet;
use tauri::State;

#[tauri::command]
pub fn variables_list(state: State<'_, AppState>) -> Result<Vec<VariableMetadata>, String> {
    state.variables.list(None)
}

#[tauri::command]
pub fn variables_save(
    state: State<'_, AppState>,
    input: SaveVariable,
) -> Result<VariableMetadata, String> {
    let workspaces: HashSet<String> = state
        .workspace_index
        .read()
        .map_err(|_| "Workspace index is unavailable")?
        .by_id
        .keys()
        .cloned()
        .collect();
    state.variables.save(input, &workspaces)
}

#[tauri::command]
pub fn workspace_variables(
    state: State<'_, AppState>,
    workspace_id: String,
) -> Result<Vec<VariableMetadata>, String> {
    if state.workspace_root(&workspace_id).is_none() {
        return Err("Workspace is unavailable".into());
    }
    state.variables.list(Some(&workspace_id))
}
