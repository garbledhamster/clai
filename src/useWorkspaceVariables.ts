import { useEffect, useState } from 'react';
import type { VariableMetadata } from './generated/bindings';
import { VARIABLES_CHANGED_EVENT, workspaceVariables } from './variables';

type MetadataState = { workspaceId: string; fields: VariableMetadata[]; status: 'ready' | 'loading' | 'unavailable' };

export function useWorkspaceVariables(workspaceId: string) {
  const [state, setState] = useState<MetadataState>({ workspaceId: '', fields: [], status: 'loading' });
  useEffect(() => {
    let cancelled = false;
    let sequence = 0;
    const load = async () => {
      const request = ++sequence;
      try {
        const fields = await workspaceVariables(workspaceId);
        if (!Array.isArray(fields)) throw new Error('Variable metadata unavailable');
        if (!cancelled && request === sequence) setState({ workspaceId, fields: fields.filter((field) => field.workspaceIds.includes(workspaceId)), status: 'ready' });
      } catch {
        if (!cancelled && request === sequence) setState({ workspaceId, fields: [], status: 'unavailable' });
      }
    };
    const refresh = () => {
      setState({ workspaceId, fields: [], status: 'loading' });
      void load();
    };
    void load();
    window.addEventListener(VARIABLES_CHANGED_EVENT, refresh);
    return () => { cancelled = true; window.removeEventListener(VARIABLES_CHANGED_EVENT, refresh); };
  }, [workspaceId]);
  return state.workspaceId === workspaceId ? state : { workspaceId, fields: [], status: 'loading' as const };
}
