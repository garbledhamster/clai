import { invoke } from '@tauri-apps/api/core';
import type { SaveVariable, VariableMetadata, WorkspaceListEntry } from './generated/bindings';

export const listVariables = () => invoke<VariableMetadata[]>('variables_list');
export const workspaceVariables = (workspaceId: string) =>
  invoke<VariableMetadata[]>('workspace_variables', { workspaceId });
export const saveVariable = (input: SaveVariable) =>
  invoke<VariableMetadata>('variables_save', { input });
export const variableWorkspaces = () =>
  invoke<Pick<WorkspaceListEntry, 'id' | 'title'>[]>('workspace_list');
