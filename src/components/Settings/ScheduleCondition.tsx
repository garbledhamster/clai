import { useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { open } from '@tauri-apps/plugin-dialog';
import type { ScriptApproval, ScriptConfig, ScriptResult, ScriptStatus } from '../../generated/bindings';
import { Switch } from './ListEditors';
import form from './SettingsForm.module.css';
import styles from './ScheduleCondition.module.css';

export type ScheduleScriptPhase = 'condition' | 'before' | 'after';

const phaseLabel = (phase: ScheduleScriptPhase) => phase === 'condition' ? 'Condition' : phase === 'before' ? 'Before' : 'After';

export const defaultScript = (phase: ScheduleScriptPhase = 'condition'): ScriptConfig => ({
  source: { type: 'inline', content: '' }, interpreter: 'sh', timeoutSeconds: phase === 'condition' ? 30 : 300,
});
export const defaultCondition = () => defaultScript();

export const scriptError = (script: ScriptConfig | null, phase: ScheduleScriptPhase = 'condition'): string | null => {
  const label = phaseLabel(phase);
  if (!script) return null;
  if (!Number.isInteger(script.timeoutSeconds) || script.timeoutSeconds <= 0 || script.timeoutSeconds > 4294967295) {
    return `${label} timeout must be a positive whole number of seconds.`;
  }
  if (!script.interpreter.trim()) return `Choose a ${phase} interpreter.`;
  if (script.source.type === 'file' && !script.source.path.trim()) return `Select a ${phase} script file.`;
  return null;
};

export const conditionError = (script: ScriptConfig | null) => scriptError(script);

const message = (error: unknown) => typeof error === 'string' ? error : error instanceof Error ? error.message : 'The script action could not be completed.';

interface Props {
  workspaceId: string;
  phase?: ScheduleScriptPhase;
  approvalDisabled?: boolean;
  previewBlocked?: boolean;
  cleanupIntentId?: string | null;
  cleanupInterrupted?: boolean;
  value: ScriptConfig | null;
  onChange: (value: ScriptConfig | null) => void;
  status: ScriptStatus | null | undefined;
  disabled: boolean;
  unattended: boolean;
  onUnattendedChange: (checked: boolean, approvalId?: string) => void;
  onScheduledAuthorize: () => void;
  onDetailsRefresh?: () => void;
}

export default function ScheduleCondition({ workspaceId, phase = 'condition', value, onChange, status, disabled, approvalDisabled = disabled, previewBlocked = false, cleanupIntentId, cleanupInterrupted = false, unattended, onUnattendedChange, onScheduledAuthorize, onDetailsRefresh }: Props) {
  const label = phaseLabel(phase);
  const cleanup = phase === 'after';
  // Keep a preview visible immediately, before the next workspace poll returns it.
  const [preview, setPreview] = useState<ScriptApproval | null>(null);
  const [dismissedId, setDismissedId] = useState<string | null>(null);
  const [result, setResult] = useState<{ value: ScriptResult; previousSaved: string } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [operation, setOperation] = useState<'preview' | 'run' | 'skip' | 'authorize' | null>(null);
  const busy = operation !== null;
  const pending = preview ?? (status?.pending?.id !== dismissedId ? status?.pending : null);
  const savedResult = JSON.stringify(status?.result ? { ...status.result, durationMs: Number(status.result.durationMs) } : null);
  const latestResult = result && result.previousSaved === savedResult ? result.value : status?.result;
  const visibleError = error || (status?.error === latestResult?.error ? null : status?.error);
  const actionLocked = approvalDisabled || busy || !!status?.running;
  const locked = disabled || busy || !!status?.running || pending?.purpose === 'unattended';

  const pickFile = async () => {
    setError(null);
    try {
      const path = await open({ title: `Select ${phase} script`, multiple: false, directory: false });
      if (typeof path === 'string' && value) onChange({ ...value, source: { type: 'file', path } });
    } catch (err) { setError(message(err)); }
  };

  const prepare = async (purpose: 'test' | 'unattended') => {
    const problem = scriptError(value, phase);
    if (problem || !value) { setError(problem); return; }
    setOperation('preview');
    setError(null);
    try {
      const approval = await invoke<ScriptApproval>('workspace_script_preview', { workspaceId, script: value, ...(phase !== 'condition' ? { phase } : {}), ...(purpose === 'unattended' ? { purpose } : {}) });
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
      // Bound cleanup keeps its original request when input/access changes.
      // Leave that request visible so Restore/Run once/Skip remain actionable.
      if (!cleanup || pending.purpose === 'test' || pending.purpose === 'unattended') setDismissedId(pending.id);
      setPreview(null);
      setError(message(err));
      onDetailsRefresh?.();
    } finally { setOperation(null); }
  };

  const skipUnavailableCleanup = async () => {
    if (!cleanupIntentId) return;
    setOperation('skip');
    setError(null);
    try {
      await invoke('workspace_cleanup_skip', { workspaceId, intentId: cleanupIntentId });
      onDetailsRefresh?.();
    } catch (err) { setError(message(err)); onDetailsRefresh?.(); }
    finally { setOperation(null); }
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
      if (!cleanup || pending.purpose === 'test' || pending.purpose === 'unattended') setDismissedId(pending.id);
      setPreview(null);
      setError(message(err));
      onDetailsRefresh?.();
    } finally { setOperation(null); }
  };

  return (
    <section className={form.panel} aria-labelledby={`${phase}-heading`}>
      <header className={form.panelHeader}>
        <h3 id={`${phase}-heading`} className={form.panelTitle}>{label} script</h3>
      </header>
      <div className={form.panelBody}>
        <Switch label={phase === 'condition' ? 'Check for work before the agent runs' : phase === 'before' ? 'Prepare before the agent runs' : 'Clean up after the scheduled attempt'} checked={!!value}
          hint={phase === 'condition' ? 'Exit 0 starts the scheduled agent; exit 1 skips quietly. Other failures are shown here.' : phase === 'before' ? 'Exit 0 allows the agent to start. A failure, timeout or skipped approval stops the attempt.' : 'Runs after attempted preparation or a started agent, including failure and cancellation. Any nonzero exit is a failure.'}
          onChange={(checked) => onChange(checked ? defaultScript(phase) : null)} disabled={locked} />
        {value && <>
          <div className={form.segmented} role="group" aria-label={`${label} script source`}>
            <button type="button" className={form.segment} aria-pressed={value.source.type === 'inline'} disabled={locked}
              onClick={() => { if (value.source.type !== 'inline') onChange({ ...value, source: { type: 'inline', content: '' } }); }}>Inline script</button>
            <button type="button" className={form.segment} aria-pressed={value.source.type === 'file'} disabled={locked}
              onClick={() => { if (value.source.type !== 'file') onChange({ ...value, source: { type: 'file', path: '' } }); }}>Script file</button>
          </div>
          {value.source.type === 'inline' ? <div className={form.field}>
            <label className={form.label} htmlFor={`${phase}-content`}>{label} script contents</label>
            <textarea id={`${phase}-content`} className={`${form.textarea} ${form.mono}`} value={value.source.content}
              onChange={(event) => onChange({ ...value, source: { type: 'inline', content: event.target.value } })}
              disabled={locked} spellCheck={false} autoCapitalize="off" autoCorrect="off" />
          </div> : <div className={form.field}>
            <label className={form.label} htmlFor={`${phase}-path`}>{label} script file</label>
            <div className={form.addRow}>
              <input id={`${phase}-path`} className={form.input} value={value.source.path} disabled={locked}
                onChange={(event) => onChange({ ...value, source: { type: 'file', path: event.target.value } })} spellCheck={false} />
              <button type="button" className={form.secondaryButton} onClick={pickFile} disabled={locked}>Choose file…</button>
            </div>
            <p className={form.hint}>Use an absolute path or a path relative to the workspace folder.</p>
          </div>}
          <div className={styles.options}>
            <div className={form.field}>
              <label className={form.label} htmlFor={`${phase}-interpreter`}>{label} interpreter</label>
              <input id={`${phase}-interpreter`} className={form.input} value={value.interpreter} list={`${phase}-interpreters`}
                onChange={(event) => onChange({ ...value, interpreter: event.target.value })} disabled={locked} spellCheck={false} />
              <datalist id={`${phase}-interpreters`}>{['sh', 'bash', 'zsh', 'python', 'python3', 'node'].map((name) => <option key={name} value={name} />)}</datalist>
              <p className={form.hint}>Interpreter name or absolute executable path.</p>
            </div>
            <div className={form.field}>
              <label className={form.label} htmlFor={`${phase}-timeout`}>{label} timeout (seconds)</label>
              <input id={`${phase}-timeout`} className={form.input} type="number" min={1} step={1} value={value.timeoutSeconds}
                onChange={(event) => onChange({ ...value, timeoutSeconds: Number(event.target.value) })} disabled={locked} />
            </div>
          </div>
          <div className={styles.actions}>
            <button type="button" className={form.secondaryButton} onClick={() => prepare('test')} disabled={locked || !!pending || previewBlocked} aria-label={`Run ${phase} now`}>Run now</button>
            <p className={form.hint}>{previewBlocked ? 'Finish or explicitly skip outstanding cleanup before testing a draft.' : 'Tests this draft after confirmation. Does not save or start the agent.'}</p>
          </div>
          <Switch label="Run unattended" checked={unattended}
            hint={pending?.purpose === 'scheduled' && unattended ? 'Automatic execution is waiting for fresh approval below.' : unattended ? 'Save to retain unattended execution. Run now still asks for confirmation.' : 'Review and approve the exact script and access to run this phase without asking each time.'}
            onChange={(checked) => {
              if (checked) void prepare('unattended');
              else onUnattendedChange(false);
            }} disabled={locked || !!pending || (previewBlocked && !unattended)} />
          <p className={form.hint}>{unattended ? 'Changed script contents, interpreter, Main agent or effective access require fresh approval.' : 'With Run unattended clear, this phase asks for Run once or Skip each time. Save changes to apply this choice.'} Scripts use the Main agent’s execution permissions.</p>
          {cleanup && <p className={form.hint}>Cleanup uses at-least-once execution: an interruption can cause it to run again, even if its work already finished. A crash immediately after intent is saved can also leave no preparation to undo. Write this script so it tolerates both cases.</p>}
        </>}
        {pending && <section className={styles.approval} aria-labelledby={`${phase}-approval-heading`}>
          <h4 id={`${phase}-approval-heading`}>{pending.purpose === 'recovery' ? 'Interrupted cleanup needs fresh approval' : pending.purpose === 'test' ? `Confirm ${phase} test` : pending.purpose === 'unattended' ? `Approve unattended ${phase} script` : `Scheduled ${phase} needs approval`}</h4>
          <p className={form.lede}>Review the captured script and current Main agent’s access. {pending.purpose === 'unattended' ? 'Approve unattended permits future scheduled executions of this phase with these exact contents after Save.' : 'Run once executes these exact contents.'}</p>
          <dl className={styles.facts}>
            <dt>Interpreter</dt><dd>{pending.interpreter}</dd>
            <dt>Timeout</dt><dd>{pending.timeoutSeconds} seconds</dd>
          </dl>
          <pre className={styles.output} aria-label={`Captured ${phase} script`}>{pending.content}</pre>
          <h4>Effective access</h4>
          <pre className={styles.access}>{pending.access}</pre>
          <p className={form.hint}>Approval does not override execution restrictions. Invocation checks do not inspect every command reached inside an interpreter. Files and services read by the script can change; approving it does not verify every external dependency.</p>
          {cleanup && <p className={form.hint}>Interrupted cleanup may run again, even if it already finished. Keep this script safe to repeat. A scheduled cleanup approval stays available outside allowed blocks and after future checks are disabled. The workspace waits until cleanup finishes or you explicitly choose Skip.</p>}
          <div className={styles.actions}>
            {pending.purpose !== 'unattended' && <button type="button" className={form.primaryButton} onClick={() => decide(true)} disabled={actionLocked}>Run once</button>}
            {pending.purpose !== 'test' && pending.purpose !== 'recovery' && <button type="button" className={pending.purpose === 'unattended' ? form.primaryButton : form.secondaryButton} onClick={authorize} disabled={actionLocked}>Approve unattended</button>}
            <button type="button" className={form.secondaryButton} onClick={() => decide(false)} disabled={actionLocked}>{pending.purpose === 'scheduled' || pending.purpose === 'recovery' ? 'Skip' : 'Cancel'}</button>
            {pending.purpose === 'scheduled' && <p className={form.hint}>Approve unattended saves approval for this saved {phase} script and lets the waiting phase continue.</p>}
          </div>
        </section>}
        {cleanup && cleanupInterrupted && <p className={form.hint} role="status">Cleanup was interrupted. Preparation or the agent may not have started, and cleanup may already have finished. Retrying can repeat its effects. A previous Run once approval does not authorize this retry.</p>}
        {cleanup && cleanupInterrupted && cleanupIntentId && !pending && !status?.running && <div className={styles.actions}>
          <button type="button" className={form.secondaryButton} onClick={skipUnavailableCleanup} disabled={actionLocked}>Skip outstanding cleanup</button>
          <p className={form.hint}>Records an explicit Skip for this outstanding cleanup and lets the workspace continue.</p>
        </div>}
        <div aria-live="polite">
          {(busy || status?.running) && <p className={form.hint}>{status?.running || operation === 'run' ? `${label} running…` : operation === 'skip' || operation === 'authorize' ? 'Updating approval…' : `Preparing ${phase}…`}</p>}
          {latestResult && <section className={styles.result} aria-label={`${label} result`}>
            <p className={form.label}>{latestResult.timedOut ? `${label} timed out` : latestResult.error ? `${label} failed` : latestResult.exitCode === 0 ? phase === 'condition' ? 'Condition true' : `${label} succeeded` : phase === 'condition' && latestResult.exitCode === 1 ? 'Condition false — skipped' : `${label} failed`} · {latestResult.exitCode == null ? 'No exit status' : `Exit ${latestResult.exitCode}`} · {Number(latestResult.durationMs)} ms</p>
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
