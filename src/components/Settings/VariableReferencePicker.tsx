import { useEffect, useId, useRef, useState } from 'react';
import { useWorkspaceVariables } from '../../useWorkspaceVariables';
import { requiredVariableReference } from '../../variableReferences';
import { openGlobalSettings } from '../../utils/globalSettings';
import { variableTypeLabel } from './WorkspaceVariables';
import form from './SettingsForm.module.css';
import styles from './WorkspaceVariables.module.css';
import { useOverlayLayer } from '../../hooks/useOverlayLayer';

export default function VariableReferencePicker({ workspaceId, interpreter, phase, disabled, onInsert }: { workspaceId: string; interpreter: string; phase: string; disabled: boolean; onInsert: (reference: string) => void }) {
  const { fields, status } = useWorkspaceVariables(workspaceId);
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState('');
  const search = useRef<HTMLInputElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const pickerId = useId();
  useOverlayLayer(open, () => { setOpen(false); trigger.current?.focus(); }, { lockScroll: false });
  useEffect(() => { if (open) search.current?.focus(); }, [open]);
  const results = fields.filter((field) => `${field.name} ${variableTypeLabel(field.kind)}`.toLowerCase().includes(query.trim().toLowerCase()));
  const supported = requiredVariableReference('EXAMPLE', interpreter) !== null;
  return <div className={styles.picker}>
    <button ref={trigger} type="button" className={form.secondaryButton} aria-label={`Insert ${phase} variable`}
      aria-expanded={open} aria-controls={pickerId} disabled={disabled} onClick={() => setOpen(!open)}>Insert variable…</button>
    {open && <div id={pickerId} className={styles.pickerBody} role="region" aria-label={`${phase} variable references`}>
      <input ref={search} type="search" className={form.input} value={query} onChange={(event) => setQuery(event.target.value)} aria-label={`Search ${phase} variables`} placeholder="Find an assigned variable…" />
      {!supported ? <p className={form.hint}>Choose sh, bash, zsh, python, python3 or node to insert a reference.</p>
        : status === 'loading' ? <p className={form.hint} role="status">Loading variables…</p>
        : status === 'unavailable' ? <p className={form.hint} role="status">Variables unavailable. Check access in app Settings.</p>
        : results.length === 0 ? <p className={form.hint} role="status">{fields.length === 0 ? 'No variables assigned to this workspace.' : 'Missing or unavailable. No assigned variable matches.'}</p>
        : <ul className={styles.list} aria-label={`Assigned ${phase} variables`}>{results.map((field) => <li key={field.id}>
          <button type="button" className={styles.row} disabled={disabled} aria-label={`Insert ${field.name} · ${variableTypeLabel(field.kind)}`}
            onClick={() => { const reference = requiredVariableReference(field.name, interpreter); if (reference) { onInsert(reference); setOpen(false); } }}>
            <code>{field.name}</code><span>{variableTypeLabel(field.kind)}</span>
          </button>
        </li>)}</ul>}
      <p className={form.hint}>Missing values stop the script; empty values are allowed. Inserts a reference, with no value copied into the script.</p>
      <button type="button" className={form.secondaryButton} onClick={() => openGlobalSettings({ tab: 'variables' })}>Manage in Settings</button>
    </div>}
  </div>;
}
