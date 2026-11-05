import { useCallback, useEffect, useId, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { useOverlayLayer } from '../../hooks/useOverlayLayer';
import { useWorkspaceVariables } from '../../useWorkspaceVariables';
import { openGlobalSettings } from '../../utils/globalSettings';
import type { VariableKind } from '../../generated/bindings';
import form from './SettingsForm.module.css';
import styles from './WorkspaceVariables.module.css';

export const variableTypeLabel = (kind: VariableKind) => ({ text: 'Text', boolean: 'Boolean', number: 'Number', protected: 'Protected string' })[kind];

function FullValue({ name, value, onClose }: { name: string; value: string; onClose: (restoreFocus: boolean) => void }) {
  const panel = useRef<HTMLDivElement>(null);
  const close = useRef<HTMLButtonElement>(null);
  useOverlayLayer(true, () => onClose(true), { lockScroll: false });
  useEffect(() => {
    close.current?.focus();
    const outside = (event: PointerEvent) => { if (event.target instanceof Node && !panel.current?.contains(event.target)) onClose(false); };
    document.addEventListener('pointerdown', outside);
    return () => document.removeEventListener('pointerdown', outside);
  }, [onClose]);
  return createPortal(<div ref={panel} className={styles.fullValue} role="dialog" aria-label={`${name} value`}>
    <header className={styles.header}><h3 className={form.panelTitle}>{name}</h3>
      <button ref={close} type="button" className={form.secondaryButton} onClick={() => onClose(true)}>Close</button></header>
    <pre tabIndex={0} aria-label="Full value">{value}</pre>
  </div>, document.body);
}

export default function WorkspaceVariables({ workspaceId }: { workspaceId: string }) {
  const { fields, status } = useWorkspaceVariables(workspaceId);
  const [selected, setSelected] = useState<string | null>(null);
  const [fullValue, setFullValue] = useState<{ workspaceId: string; id: string; revision: number } | null>(null);
  const fullValueTrigger = useRef<HTMLButtonElement>(null);
  const detailId = useId();
  const field = fields.find((field) => field.id === selected);
  const inspected = fullValue?.workspaceId === workspaceId ? fields.find((item) => item.id === fullValue.id && item.revision === fullValue.revision) : undefined;
  const closeFullValue = useCallback((restore: boolean) => {
    setFullValue(null); if (restore) fullValueTrigger.current?.focus();
  }, []);
  return <section className={styles.section} aria-label="Workspace variables">
    <div className={styles.header}><h3 className={form.panelTitle}>Variables</h3>
      <button type="button" className={form.secondaryButton} onClick={() => openGlobalSettings({ tab: 'variables' })}>Manage in Settings</button>
    </div>
    {status === 'loading' ? <p className={form.hint} role="status">Loading variables…</p> : status === 'unavailable' ? <p className={form.hint} role="status">Variables unavailable. Open Settings to check access.</p> : fields.length === 0 ? <p className={form.hint}>No variables assigned to this workspace.</p> : <>
      <ul className={styles.list} aria-label="Assigned variables">
        {fields.map((item) => <li key={item.id}><button type="button" className={styles.row}
          aria-label={`${item.name} · ${variableTypeLabel(item.kind)}`} aria-describedby={field?.id === item.id ? detailId : undefined}
          onMouseEnter={() => setSelected(item.id)} onFocus={() => setSelected(item.id)} onClick={() => setSelected(item.id)}>
          <code>{item.name}</code><span>{variableTypeLabel(item.kind)}</span>
        </button></li>)}
      </ul>
      <div className={styles.details} id={detailId}>
        {field ? field.kind === 'protected' ? <p>Protected string · ••••<br />Reveal in app Settings.</p>
          : <><pre title={String(field.value ?? 'Missing or unavailable')}>{String(field.value ?? 'Missing or unavailable')}</pre>
            <button ref={fullValueTrigger} type="button" className={form.secondaryButton} onClick={() => setFullValue({ workspaceId, id: field.id, revision: field.revision })}>View full value</button></>
          : <p>Point at a field, focus it or tap it to see its details.</p>}
      </div>
    </>}
    {inspected && inspected.kind !== 'protected' && <FullValue name={inspected.name} value={String(inspected.value ?? 'Missing or unavailable')} onClose={closeFullValue} />}
  </section>;
}
