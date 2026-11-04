import { useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { open } from '@tauri-apps/plugin-dialog';
import type { ScriptApproval, ScriptConfig, ScriptResult, ScriptStatus } from '../../generated/bindings';
import { Switch } from './ListEditors';
import form from './SettingsForm.module.css';
import styles from './ScheduleCondition.module.css';

export const defaultCondition = (): ScriptConfig => ({
  source: { type: 'inline', content: '' }, interpreter: 'sh', timeoutSeconds: 30,
});

export const conditionError = (script: ScriptConfig | null): string | null => {
  if (!script) return null;
  if (!Number.isInteger(script.timeoutSeconds) || script.timeoutSeconds <= 0 || script.timeoutSeconds > 4294967295) {
    return 'Condition timeout must be a positive whole number of seconds.';
  }
  if (!script.interpreter.trim()) return 'Choose a condition interpreter.';
  if (script.source.type === 'file' && !script.source.path.trim()) return 'Select a condition script file.';
  return null;
};

const message = (error: unknown) => typeof error === 'string' ? error : error instanceof Error ? error.message : 'The condition action could not be completed.';

interface Props {
  workspaceId: string;
  value: ScriptConfig | null;
  onChange: (value: ScriptConfig | null) => void;
  status: ScriptStatus | null | undefined;
  disabled: boolean;
  unattended: boolean;
  onUnattendedChange: (checked: boolean, approvalId?: string) => void;
  onScheduledAuthorize: () => void;
  onDetailsRefresh?: () => void;
}

export default function ScheduleCondition({ workspaceId, value, onChange, status, disabled, unattended, onUnattendedChange, onScheduledAuthorize, onDetailsRefresh }: Props) {
  // Keep a preview visible immediately, before the next workspace poll returns it.
  const [preview, setPreview] = useState<ScriptApproval | null>(null);
  const [dismissedId, setDismissedId] = useState<string | null>(null);
  const [result, setResult] = useState<{ value: ScriptResult; previousSaved: string } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [operation, setOperation] = useState<'preview' | 'run' | 'skip' | 'authorize' | null>(null);
  const busy = operation !== null;
  const pending = preview ?? (status?.pending?.id !== dismissedId ? status?.pending : null);
  const savedResult = JSON.stringify(status?.result ?? null);
  const latestResult = result && result.previousSaved === savedResult ? result.value : status?.result;
  const visibleError = error || (status?.error === latestResult?.error ? null : status?.error);
  const actionLocked = disabled || busy || !!status?.running;
  const locked = actionLocked || pending?.purpose === 'unattended';

  const pickFile = async () => {
    setError(null);
    try {
      const path = await open({ title: 'Select condition script', multiple: false, directory: false });
      if (typeof path === 'string' && value) onChange({ ...value, source: { type: 'file', path } });
    } catch (err) { setError(message(err)); }
  };

  const prepare = async (purpose: 'test' | 'unattended') => {
    const problem = conditionError(value);
    if (problem || !value) { setError(problem); return; }
    setOperation('preview');
    setError(null);
    try {
      const approval = await invoke<ScriptApproval>('workspace_script_preview', { workspaceId, script: value, ...(purpose === 'unattended' ? { purpose } : {}) });
      setPreview(approval);
      setResult(null);
      onDetailsRefresh?.();
    } catch (err) { setError(message(err)); }
    finally { setOperation(null); }
  };

  const decide = async (run: boolean) => {
    if (!pending) return;
    setOperation(run ? 'run' : 'skip');
    setError(null);
    try {
      const outcome = await invoke<ScriptResult | null>('workspace_script_decide', { workspaceId, approvalId: pending.id, run });
      setDismissedId(pending.id);
      setPreview(null);
      if (outcome) setResult({ value: outcome, previousSaved: savedResult });
      onDetailsRefresh?.();
    } catch (err) {
      // A stale approval must be reviewed again using the backend's new snapshot.
      setDismissedId(pending.id);
      setPreview(null);
      setError(message(err));
      onDetailsRefresh?.();
    } finally { setOperation(null); }
  };

  const authorize = async () => {
    if (!pending || pending.purpose === 'test') return;
    setOperation('authorize');
    setError(null);
    try {
      const approvalId = await invoke<string>('workspace_script_authorize', { workspaceId, approvalId: pending.id });
      if (pending.purpose === 'unattended') onUnattendedChange(true, approvalId);
      else onScheduledAuthorize();
      setDismissedId(pending.id);
      setPreview(null);
      onDetailsRefresh?.();
    } catch (err) {
      setDismissedId(pending.id);
      setPreview(null);
      setError(message(err));
      onDetailsRefresh?.();
    } finally { setOperation(null); }
  };

  return (
    <section className={form.panel} aria-labelledby="condition-heading">
      <header className={form.panelHeader}>
        <h3 id="condition-heading" className={form.panelTitle}>Condition script</h3>
      </header>
      <div className={form.panelBody}>
        <Switch label="Check for work before the agent runs" checked={!!value}
          hint="Exit 0 starts the scheduled agent; exit 1 skips quietly. Other failures are shown here."
          onChange={(checked) => onChange(checked ? defaultCondition() : null)} disabled={locked} />
        {value && <>
          <div className={form.segmented} role="group" aria-label="Condition script source">
            <button type="button" className={form.segment} aria-pressed={value.source.type === 'inline'} disabled={locked}
              onClick={() => onChange({ ...value, source: { type: 'inline', content: '' } })}>Inline script</button>
            <button type="button" className={form.segment} aria-pressed={value.source.type === 'file'} disabled={locked}
              onClick={() => onChange({ ...value, source: { type: 'file', path: '' } })}>Script file</button>
          </div>
          {value.source.type === 'inline' ? <div className={form.field}>
            <label className={form.label} htmlFor="condition-content">Condition script contents</label>
            <textarea id="condition-content" className={`${form.textarea} ${form.mono}`} value={value.source.content}
              onChange={(event) => onChange({ ...value, source: { type: 'inline', content: event.target.value } })}
              disabled={locked} spellCheck={false} autoCapitalize="off" autoCorrect="off" />
          </div> : <div className={form.field}>
            <label className={form.label} htmlFor="condition-path">Condition script file</label>
            <div className={form.addRow}>
              <input id="condition-path" className={form.input} value={value.source.path} disabled={locked}
                onChange={(event) => onChange({ ...value, source: { type: 'file', path: event.target.value } })} spellCheck={false} />
              <button type="button" className={form.secondaryButton} onClick={pickFile} disabled={locked}>Choose file…</button>
            </div>
            <p className={form.hint}>Use an absolute path or a path relative to the workspace folder.</p>
          </div>}
          <div className={styles.options}>
            <div className={form.field}>
              <label className={form.label} htmlFor="condition-interpreter">Condition interpreter</label>
              <input id="condition-interpreter" className={form.input} value={value.interpreter} list="condition-interpreters"
                onChange={(event) => onChange({ ...value, interpreter: event.target.value })} disabled={locked} spellCheck={false} />
              <datalist id="condition-interpreters">{['sh', 'bash', 'zsh', 'python', 'python3', 'node'].map((name) => <option key={name} value={name} />)}</datalist>
              <p className={form.hint}>Interpreter name or absolute executable path.</p>
            </div>
            <div className={form.field}>
              <label className={form.label} htmlFor="condition-timeout">Condition timeout (seconds)</label>
              <input id="condition-timeout" className={form.input} type="number" min={1} step={1} value={value.timeoutSeconds}
                onChange={(event) => onChange({ ...value, timeoutSeconds: Number(event.target.value) })} disabled={locked} />
            </div>
          </div>
          <div className={styles.actions}>
            <button type="button" className={form.secondaryButton} onClick={() => prepare('test')} disabled={locked || !!pending} aria-label="Run condition now">Run now</button>
            <p className={form.hint}>Tests this draft after confirmation. Does not save or start the agent.</p>
          </div>
          <Switch label="Run unattended" checked={unattended}
            hint={pending?.purpose === 'scheduled' && unattended ? 'Automatic checks are waiting for fresh approval below.' : unattended ? 'Save to retain unattended execution. Run now still asks for confirmation.' : 'Review and approve the exact script and access to run scheduled checks without asking each time.'}
            onChange={(checked) => {
              if (checked) void prepare('unattended');
              else onUnattendedChange(false);
            }} disabled={locked || !!pending} />
          <p className={form.hint}>{unattended ? 'Changed script contents, interpreter, Main agent or effective access require fresh approval.' : 'With Run unattended clear, scheduled checks ask for Run once or Skip each time. Save changes to apply this choice.'} Scripts use the Main agent’s execution permissions.</p>
        </>}
        {pending && <section className={styles.approval} aria-labelledby="condition-approval-heading">
          <h4 id="condition-approval-heading">{pending.purpose === 'test' ? 'Confirm condition test' : pending.purpose === 'unattended' ? 'Approve unattended condition checks' : 'Scheduled condition needs approval'}</h4>
          <p className={form.lede}>Review the captured script and current Main agent’s access. {pending.purpose === 'unattended' ? 'Approve unattended permits future scheduled checks with these exact contents after Save.' : 'Run once executes these exact contents.'}</p>
          <dl className={styles.facts}>
            <dt>Interpreter</dt><dd>{pending.interpreter}</dd>
            <dt>Timeout</dt><dd>{pending.timeoutSeconds} seconds</dd>
          </dl>
          <pre className={styles.output} aria-label="Captured condition script">{pending.content}</pre>
          <h4>Effective access</h4>
          <pre className={styles.access}>{pending.access}</pre>
          <p className={form.hint}>Approval does not override execution restrictions. Invocation checks do not inspect every command reached inside an interpreter. Files and services read by the script can change; approving it does not verify every external dependency.</p>
          <div className={styles.actions}>
            {pending.purpose !== 'unattended' && <button type="button" className={form.primaryButton} onClick={() => decide(true)} disabled={actionLocked}>Run once</button>}
            {pending.purpose !== 'test' && <button type="button" className={pending.purpose === 'unattended' ? form.primaryButton : form.secondaryButton} onClick={authorize} disabled={actionLocked}>Approve unattended</button>}
            <button type="button" className={form.secondaryButton} onClick={() => decide(false)} disabled={actionLocked}>{pending.purpose === 'scheduled' ? 'Skip' : 'Cancel'}</button>
            {pending.purpose === 'scheduled' && <p className={form.hint}>Approve unattended saves approval for the current saved condition and lets this waiting check continue.</p>}
          </div>
        </section>}
        <div aria-live="polite">
          {(busy || status?.running) && <p className={form.hint}>{status?.running || operation === 'run' ? 'Condition running…' : operation === 'skip' || operation === 'authorize' ? 'Updating approval…' : 'Preparing condition…'}</p>}
          {latestResult && <section className={styles.result} aria-label="Condition result">
            <p className={form.label}>{latestResult.timedOut ? 'Condition timed out' : latestResult.error ? 'Condition failed' : latestResult.exitCode === 0 ? 'Condition true' : latestResult.exitCode === 1 ? 'Condition false — skipped' : 'Condition failed'} · {latestResult.exitCode == null ? 'No exit status' : `Exit ${latestResult.exitCode}`} · {Number(latestResult.durationMs)} ms</p>
            {latestResult.error && <p className={form.errorBanner}>{latestResult.error}</p>}
            <h4>Standard output</h4><pre className={styles.output}>{latestResult.stdout || '(empty)'}</pre>
            <h4>Standard error</h4><pre className={styles.output}>{latestResult.stderr || '(empty)'}</pre>
            <p className={form.hint}>Retained output is limited; long output may be truncated.</p>
          </section>}
        </div>
        {visibleError && <p className={form.errorBanner} role="alert">{visibleError}</p>}
      </div>
    </section>
  );
}
