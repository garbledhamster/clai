import { beforeEach, describe, expect, it, vi } from 'vitest';
import { act, render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { createRef } from 'react';

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));
vi.mock('@tauri-apps/plugin-dialog', () => ({ open: vi.fn() }));
vi.mock('../../workspace/client', () => ({ setWorkspaceTitle: vi.fn() }));

import { open } from '@tauri-apps/plugin-dialog';
import { invoke } from '@tauri-apps/api/core';
import { GeneralPage, SchedulePage } from './WorkspacePages';
import type { SectionHandle } from './sectionHandle';
import type { ScriptApproval, ScriptResult, WorkspaceDetails } from '../../generated/bindings';

const mockOpen = vi.mocked(open);
const mockInvoke = vi.mocked(invoke);

const DETAILS = {
  workspaceId: 'ws-1',
  title: 'Backend',
  rootPath: '/home/u/backend',
  scheduleEnabled: false,
  schedulePaused: false,
  scheduleKind: null,
} as unknown as WorkspaceDetails;

const EMPTY_HOOKS = { before: null, beforeUnattended: false, beforeUnattendedApprovalId: null, after: null, afterUnattended: false, afterUnattendedApprovalId: null };

const submit = async (ref: React.RefObject<SectionHandle | null>) => {
  let result: { ok: boolean; error?: string } | undefined;
  await act(async () => { result = await ref.current!.submit(); });
  return result;
};

beforeEach(() => {
  mockInvoke.mockReset();
  mockOpen.mockReset();
  mockInvoke.mockImplementation((cmd: string) => {
    if (cmd === 'workspace_host_timezone') return Promise.resolve('Europe/Madrid');
    if (cmd === 'workspace_preview_schedule') return Promise.resolve([Date.now() + 3_600_000]);
    return Promise.resolve(null);
  });
});

describe('GeneralPage', () => {
  it('shows where the workspace lives and rejects an empty title', async () => {
    const user = userEvent.setup();
    const ref = createRef<SectionHandle>();
    render(<GeneralPage ref={ref} workspaceId="ws-1" details={DETAILS} saving={false} />);
    expect(screen.getByText('/home/u/backend')).toBeInTheDocument();

    await user.clear(screen.getByLabelText(/Title/));
    let verdict: { ok: boolean; error?: string } | undefined;
    act(() => { verdict = ref.current!.validate(); });

    expect(verdict).toEqual({ ok: false, error: 'Workspace title cannot be empty.' });
    expect(screen.getByLabelText(/Title/)).toHaveAttribute('aria-invalid', 'true');
  });
});

describe('SchedulePage', () => {
  it('saves an interval schedule once it is switched on', async () => {
    const user = userEvent.setup();
    const ref = createRef<SectionHandle>();
    const onDirtyChange = vi.fn();
    render(<SchedulePage ref={ref} workspaceId="ws-1" details={DETAILS} saving={false} onDirtyChange={onDirtyChange} />);

    await user.click(screen.getByRole('switch', { name: 'Run on a recurring schedule' }));
    expect(onDirtyChange).toHaveBeenLastCalledWith(true);

    expect(await submit(ref)).toEqual({ ok: true });
    expect(mockInvoke).toHaveBeenCalledWith('workspace_set_schedule', {
      workspaceId: 'ws-1',
      kind: { type: 'interval', intervalMinutes: 30 },
    });
  });

  it('requires explicit enhanced scheduling and validates its 15-second minimum', async () => {
    const user = userEvent.setup();
    const ref = createRef<SectionHandle>();
    render(<SchedulePage ref={ref} workspaceId="ws-1" details={DETAILS} saving={false} />);
    await user.click(screen.getByRole('switch', { name: 'Run on a recurring schedule' }));
    await user.click(screen.getByRole('switch', { name: 'Use enhanced scheduling' }));
    const input = screen.getByLabelText('Interval in seconds');
    await user.clear(input);
    await user.type(input, '14');
    expect((await submit(ref))?.ok).toBe(false);
    expect(mockInvoke).not.toHaveBeenCalledWith('workspace_set_schedule', expect.anything());
    await user.clear(input);
    await user.type(input, '15');
    expect(await submit(ref)).toEqual({ ok: true });
    expect(mockInvoke).toHaveBeenCalledWith('workspace_set_schedule', {
      workspaceId: 'ws-1', kind: { type: 'enhancedInterval', intervalSeconds: 15 },
      allowedBlocks: { timezone: 'UTC', slots: [] }, condition: null, unattended: false, unattendedApprovalId: null, ...EMPTY_HOOKS,
    });
  });

  it('shows one due attempt waiting for the busy workspace without calling it paused', () => {
    const details = { ...DETAILS, scheduleEnabled: true,
      scheduleKind: { type: 'enhancedInterval', intervalSeconds: 15 },
      scheduleWaitingReason: 'Waiting for the workspace to become available.' } as WorkspaceDetails;
    render(<SchedulePage ref={createRef()} workspaceId="ws-1" details={details} saving={false} />);
    expect(screen.getByText('Waiting')).toBeInTheDocument();
    expect(screen.getByText('Waiting for the workspace to become available.')).toBeInTheDocument();
    expect(screen.queryByText('Paused')).not.toBeInTheDocument();
  });

  it('shows why an enhanced schedule is paused when wake detection is unavailable', () => {
    const details = { ...DETAILS, scheduleEnabled: true,
      scheduleKind: { type: 'enhancedInterval', intervalSeconds: 15 },
      scheduleBlockedReason: 'Wake detection is unavailable.' } as unknown as WorkspaceDetails;
    render(<SchedulePage ref={createRef()} workspaceId="ws-1" details={details} saving={false} />);
    expect(screen.getByText('Wake detection is unavailable.')).toBeInTheDocument();
    expect(screen.queryByText('Running')).not.toBeInTheDocument();
  });

  it.each([false, true])('shows pending cleanup when schedule enabled is %s', (enabled) => {
    const details = { ...DETAILS, scheduleEnabled: enabled, scheduleBlockedReason: 'Cancellation failed; messages may wait while cleanup retries.' } as WorkspaceDetails;
    render(<SchedulePage ref={createRef()} workspaceId="ws-1" details={details} saving={false} />);
    expect(screen.getByText('Cancellation failed; messages may wait while cleanup retries.')).toBeInTheDocument();
    if (!enabled) {
      expect(screen.queryByText(/The main agent runs when you message it/)).not.toBeInTheDocument();
    }
  });

  it('preserves an enhanced cron draft across status refreshes', async () => {
    const user = userEvent.setup();
    const ref = createRef<SectionHandle>();
    const details = { ...DETAILS, scheduleEnabled: true, scheduleKind: { type: 'enhancedCron', expression: '0 9 * * *', timezone: 'UTC' } } as unknown as WorkspaceDetails;
    const page = (value: WorkspaceDetails) => <SchedulePage ref={ref} workspaceId="ws-1" details={value} saving={false} />;
    const { rerender } = render(page(details));
    const input = screen.getByLabelText('Cron expression');
    await user.clear(input);
    await user.type(input, '0 10 * * *');
    await act(async () => { rerender(page({ ...details, schedulePaused: true })); });
    expect(screen.getByLabelText('Cron expression')).toHaveValue('0 10 * * *');
    expect(await submit(ref)).toEqual({ ok: true });
    expect(mockInvoke).toHaveBeenCalledWith('workspace_set_schedule', {
      workspaceId: 'ws-1', kind: { type: 'enhancedCron', expression: '0 10 * * *', timezone: 'UTC' },
      allowedBlocks: { timezone: 'UTC', slots: [] }, condition: null, unattended: false, unattendedApprovalId: null, ...EMPTY_HOOKS,
    });
  });

  it('switches to cron in the host timezone, previews the next runs and validates the expression', async () => {
    const user = userEvent.setup();
    const ref = createRef<SectionHandle>();
    render(<SchedulePage ref={ref} workspaceId="ws-1" details={{ ...DETAILS, scheduleEnabled: true } as WorkspaceDetails} saving={false} />);

    await user.click(screen.getByRole('button', { name: 'Cron' }));
    expect(screen.getByRole('button', { name: 'Cron' })).toHaveAttribute('aria-pressed', 'true');
    expect(screen.getByLabelText('Timezone')).toHaveValue('Europe/Madrid');
    await user.click(screen.getByRole('button', { name: 'Weekdays 9am' }));
    expect(screen.getByLabelText('Cron expression')).toHaveValue('0 9 * * 1-5');
    expect(await screen.findByText('in 1h')).toBeInTheDocument();

    await user.clear(screen.getByLabelText('Cron expression'));
    let verdict: { ok: boolean; error?: string } | undefined;
    act(() => { verdict = ref.current!.validate(); });
    expect(verdict).toEqual({ ok: false, error: 'Cron expression is required.' });
    expect(screen.getByRole('alert')).toHaveTextContent('Cron expression is required.');
  });

  it('pauses at once, without going through the save bar', async () => {
    const user = userEvent.setup();
    const onDetailsRefresh = vi.fn();
    const onDirtyChange = vi.fn();
    render(
      <SchedulePage
        ref={createRef()}
        workspaceId="ws-1"
        details={{ ...DETAILS, scheduleEnabled: true, scheduleKind: { type: 'interval', intervalMinutes: 60 } } as WorkspaceDetails}
        saving={false}
        onDirtyChange={onDirtyChange}
        onDetailsRefresh={onDetailsRefresh}
      />
    );
    expect(screen.getByText('Running')).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: 'Pause' }));

    expect(mockInvoke).toHaveBeenCalledWith('workspace_set_schedule_paused', { workspaceId: 'ws-1', paused: true });
    expect(onDetailsRefresh).toHaveBeenCalledTimes(1);
    expect(onDirtyChange).not.toHaveBeenCalledWith(true);
  });
  it('keeps an unsaved edit when details are re-fetched with the same schedule', async () => {
    const user = userEvent.setup();
    const ref = createRef<SectionHandle>();
    const onDirtyChange = vi.fn();
    const onDetailsRefresh = vi.fn();
    const saved = (extra: Partial<WorkspaceDetails> = {}) => ({
      ...DETAILS,
      scheduleEnabled: true,
      scheduleKind: { type: 'cron', expression: '0 9 * * *', timezone: 'Europe/Madrid' },
      ...extra,
    }) as WorkspaceDetails;
    const page = (details: WorkspaceDetails) => (
      <SchedulePage ref={ref} workspaceId="ws-1" details={details} saving={false}
        onDirtyChange={onDirtyChange} onDetailsRefresh={onDetailsRefresh} />
    );
    const { rerender } = render(page(saved()));

    const input = screen.getByLabelText('Cron expression');
    await user.clear(input);
    await user.type(input, '0 10 * * *');
    expect(onDirtyChange).toHaveBeenLastCalledWith(true);

    // The 5 s poll: same values, new object.
    await act(async () => { rerender(page(saved())); });
    expect(screen.getByLabelText('Cron expression')).toHaveValue('0 10 * * *');

    // Pause refreshes details with only the paused flag changed.
    await user.click(screen.getByRole('button', { name: 'Pause' }));
    expect(onDetailsRefresh).toHaveBeenCalledTimes(1);
    await act(async () => { rerender(page(saved({ schedulePaused: true }))); });
    expect(screen.getByLabelText('Cron expression')).toHaveValue('0 10 * * *');
    expect(onDirtyChange).toHaveBeenLastCalledWith(true);
  });
});


describe('weekly allowed blocks', () => {
  it.each(['enhancedInterval', 'enhancedCron'])('saves %s and blocks atomically and preserves the draft on refresh', async (type) => {
    const user = userEvent.setup();
    const ref = createRef<SectionHandle>();
    const kind = type === 'enhancedInterval' ? { type, intervalSeconds: 15 } : { type, expression: '* * * * *', timezone: 'UTC' };
    const details = { ...DETAILS, scheduleEnabled: true, scheduleKind: kind } as WorkspaceDetails;
    const page = (value: WorkspaceDetails) => <SchedulePage ref={ref} workspaceId="ws-1" details={value} saving={false} />;
    const { rerender } = render(page(details));
    const zone = screen.getByLabelText('Timezone');
    await user.clear(zone);
    await user.type(zone, 'America/Chicago');
    await user.click(screen.getByRole('button', { name: 'Monday 09:00–09:15' }));
    await act(async () => { rerender(page({ ...details, schedulePaused: true })); });
    expect(screen.getByLabelText('Timezone')).toHaveValue('America/Chicago');
    expect(screen.getByRole('button', { name: 'Monday 09:00–09:15' })).toHaveAttribute('aria-pressed', 'true');
    expect(await submit(ref)).toEqual({ ok: true });
    expect(mockInvoke).toHaveBeenCalledWith('workspace_set_schedule', {
      workspaceId: 'ws-1', kind: type === 'enhancedCron' ? { ...kind, timezone: 'America/Chicago' } : kind,
      allowedBlocks: { timezone: 'America/Chicago', slots: [36] }, condition: null, unattended: false, unattendedApprovalId: null, ...EMPTY_HOOKS,
    });
  });
});


const ENHANCED = { ...DETAILS, scheduleEnabled: true,
  scheduleKind: { type: 'enhancedInterval', intervalSeconds: 30 },
} as WorkspaceDetails;
const APPROVAL: ScriptApproval = { id: 'approval-1', purpose: 'test', content: 'exit 1',
  interpreter: '/bin/sh', timeoutSeconds: 30, access: 'Restricted shell. Workspace read/write. Network off.' };
const RESULT: ScriptResult = { stdout: 'no work', stderr: '', exitCode: 1, durationMs: 12n, timedOut: false, error: null };

describe('schedule condition', () => {
  it('saves the condition, cadence and blocks together and preserves its draft during polling', async () => {
    const user = userEvent.setup();
    const ref = createRef<SectionHandle>();
    const page = (details: WorkspaceDetails) => <SchedulePage ref={ref} workspaceId="ws-1" details={details} saving={false} />;
    const { rerender } = render(page(ENHANCED));
    await user.click(screen.getByRole('switch', { name: 'Check for work before the agent runs' }));
    expect(screen.getByLabelText('Condition timeout (seconds)')).toHaveValue(30);
    await user.type(screen.getByLabelText('Condition script contents'), 'exit 1');
    await user.clear(screen.getByLabelText('Condition interpreter'));
    await user.type(screen.getByLabelText('Condition interpreter'), 'bash');
    await act(async () => rerender(page({ ...ENHANCED, schedulePaused: true })));
    expect(screen.getByLabelText('Condition script contents')).toHaveValue('exit 1');
    expect(await submit(ref)).toEqual({ ok: true });
    expect(mockInvoke).toHaveBeenCalledWith('workspace_set_schedule', {
      workspaceId: 'ws-1', kind: { type: 'enhancedInterval', intervalSeconds: 30 },
      allowedBlocks: { timezone: 'UTC', slots: [] },
      condition: { source: { type: 'inline', content: 'exit 1' }, interpreter: 'bash', timeoutSeconds: 30 },
      unattended: false, unattendedApprovalId: null, ...EMPTY_HOOKS,
    });
    expect(screen.getByRole('switch', { name: 'Run unattended' })).not.toBeChecked();
  });

  it('rejects non-positive timeout without saving or previewing a script', async () => {
    const user = userEvent.setup();
    const ref = createRef<SectionHandle>();
    render(<SchedulePage ref={ref} workspaceId="ws-1" details={ENHANCED} saving={false} />);
    await user.click(screen.getByRole('switch', { name: 'Check for work before the agent runs' }));
    await user.clear(screen.getByLabelText('Condition timeout (seconds)'));
    expect(await submit(ref)).toEqual({ ok: false, error: 'Condition timeout must be a positive whole number of seconds.' });
    expect(mockInvoke).not.toHaveBeenCalledWith('workspace_set_schedule', expect.anything());
    await user.click(screen.getByRole('button', { name: 'Run condition now' }));
    expect(mockInvoke).not.toHaveBeenCalledWith('workspace_script_preview', expect.anything());
  });

  it('tests an unsaved draft only after confirming the captured contents and access', async () => {
    const user = userEvent.setup();
    mockInvoke.mockImplementation((command) => Promise.resolve(command === 'workspace_script_preview' ? APPROVAL : command === 'workspace_script_decide' ? RESULT : null));
    render(<SchedulePage ref={createRef()} workspaceId="ws-1" details={ENHANCED} saving={false} />);
    await user.click(screen.getByRole('switch', { name: 'Check for work before the agent runs' }));
    await user.type(screen.getByLabelText('Condition script contents'), 'exit 1');
    await user.click(screen.getByRole('button', { name: 'Run condition now' }));
    expect(mockInvoke).toHaveBeenCalledWith('workspace_script_preview', { workspaceId: 'ws-1', script: {
      source: { type: 'inline', content: 'exit 1' }, interpreter: 'sh', timeoutSeconds: 30,
    } });
    expect(screen.getByLabelText('Captured condition script')).toHaveTextContent('exit 1');
    expect(screen.getByText(APPROVAL.access)).toBeInTheDocument();
    expect(screen.getByText('/bin/sh')).toBeInTheDocument();
    expect(mockInvoke).not.toHaveBeenCalledWith('workspace_script_decide', expect.anything());
    // Editing the draft after preview cannot change the approved snapshot.
    await user.clear(screen.getByLabelText('Condition script contents'));
    await user.type(screen.getByLabelText('Condition script contents'), 'exit 0');
    expect(screen.getByLabelText('Captured condition script')).toHaveTextContent('exit 1');
    await user.click(screen.getByRole('button', { name: 'Run once' }));
    expect(mockInvoke).toHaveBeenCalledWith('workspace_script_decide', { workspaceId: 'ws-1', approvalId: 'approval-1', run: true });
    expect(screen.getByRole('region', { name: 'Condition result' })).toHaveTextContent('Condition false — skipped · Exit 1 · 12 ms');
    expect(screen.getByText('no work')).toBeInTheDocument();
    expect(mockInvoke).not.toHaveBeenCalledWith('workspace_set_schedule', expect.anything());
    expect(mockInvoke).not.toHaveBeenCalledWith('workspace_run_now', expect.anything());
    expect(screen.getByLabelText('Condition script contents')).toHaveValue('exit 0');
  });

  it('replaces an immediate test result with a later scheduled result from polling', async () => {
    const user = userEvent.setup();
    const initial = { ...ENHANCED, scheduleScriptStatus: {
      pending: APPROVAL, running: false, result: null, error: null,
    } } as WorkspaceDetails;
    mockInvoke.mockImplementation((command) => Promise.resolve(command === 'workspace_script_decide' ? RESULT : null));
    const page = (details: WorkspaceDetails) => <SchedulePage ref={createRef()} workspaceId="ws-1" details={details} saving={false} />;
    const { rerender } = render(page(initial));
    await user.click(screen.getByRole('button', { name: 'Run once' }));
    expect(screen.getByText('no work')).toBeInTheDocument();
    await act(async () => rerender(page({ ...ENHANCED, scheduleScriptStatus: {
      pending: null, running: false, result: { ...RESULT, exitCode: 0, stdout: 'new work' }, error: null,
    } })));
    expect(screen.getByText('new work')).toBeInTheDocument();
    expect(screen.queryByText('no work')).not.toBeInTheDocument();
    expect(screen.getByRole('region', { name: 'Condition result' })).toHaveTextContent('Condition true · Exit 0 · 12 ms');
  });

  it('shows a renewed scheduled snapshot after a stale approval is rejected', async () => {
    const user = userEvent.setup();
    const initial = { ...ENHANCED, scheduleScriptStatus: {
      pending: { ...APPROVAL, purpose: 'scheduled' }, running: false, result: null, error: null,
    } } as WorkspaceDetails;
    mockInvoke.mockImplementation((command) => command === 'workspace_script_decide' ? Promise.reject('Script access changed. Review the new approval.') : Promise.resolve(null));
    const page = (details: WorkspaceDetails) => <SchedulePage ref={createRef()} workspaceId="ws-1" details={details} saving={false} />;
    const { rerender } = render(page(initial));
    await user.click(screen.getByRole('button', { name: 'Run once' }));
    expect(screen.getByRole('alert')).toHaveTextContent('Script access changed. Review the new approval.');
    expect(screen.queryByLabelText('Captured condition script')).not.toBeInTheDocument();
    await act(async () => rerender(page({ ...initial, scheduleScriptStatus: {
      pending: { ...APPROVAL, purpose: 'scheduled', id: 'renewed', content: 'exit 0' }, running: false, result: null, error: null,
    } })));
    expect(screen.getByLabelText('Captured condition script')).toHaveTextContent('exit 0');
    expect(screen.getByRole('button', { name: 'Run once' })).toBeEnabled();
  });

  it('picks a file and shows unavailable interpreter errors without executing or saving', async () => {
    const user = userEvent.setup();
    mockOpen.mockResolvedValue('/tmp/check.py');
    mockInvoke.mockImplementation((command) => command === 'workspace_script_preview' ? Promise.reject('Interpreter is unavailable: python3') : Promise.resolve(null));
    render(<SchedulePage ref={createRef()} workspaceId="ws-1" details={ENHANCED} saving={false} />);
    await user.click(screen.getByRole('switch', { name: 'Check for work before the agent runs' }));
    await user.click(screen.getByRole('button', { name: 'Script file' }));
    await user.click(screen.getByRole('button', { name: 'Choose file…' }));
    expect(mockOpen).toHaveBeenCalledWith({ title: 'Select condition script', multiple: false, directory: false });
    expect(screen.getByLabelText('Condition script file')).toHaveValue('/tmp/check.py');
    await user.clear(screen.getByLabelText('Condition interpreter'));
    await user.type(screen.getByLabelText('Condition interpreter'), 'python3');
    await user.click(screen.getByRole('button', { name: 'Run condition now' }));
    expect(mockInvoke).toHaveBeenCalledWith('workspace_script_preview', { workspaceId: 'ws-1', script: {
      source: { type: 'file', path: '/tmp/check.py' }, interpreter: 'python3', timeoutSeconds: 30,
    } });
    expect(screen.getByRole('alert')).toHaveTextContent('Interpreter is unavailable: python3');
    expect(mockInvoke).not.toHaveBeenCalledWith('workspace_script_decide', expect.anything());
    expect(mockInvoke).not.toHaveBeenCalledWith('workspace_set_schedule', expect.anything());
  });

  it.each(['test', 'scheduled'])('allows %s approval to be declined without execution', async (purpose) => {
    const user = userEvent.setup();
    render(<SchedulePage ref={createRef()} workspaceId="ws-1" details={{ ...ENHANCED,
      scheduleScriptStatus: { pending: { ...APPROVAL, purpose }, running: false, result: null, error: null },
    }} saving={false} />);
    await user.click(screen.getByRole('button', { name: purpose === 'test' ? 'Cancel' : 'Skip' }));
    expect(mockInvoke).toHaveBeenCalledWith('workspace_script_decide', { workspaceId: 'ws-1', approvalId: 'approval-1', run: false });
    expect(screen.queryByLabelText('Captured condition script')).not.toBeInTheDocument();
  });

  it('removes a scheduled approval when polling reports it expired or displaced', async () => {
    const initial = { ...ENHANCED, scheduleScriptStatus: {
      pending: { ...APPROVAL, purpose: 'scheduled' }, running: false, result: null, error: null,
    } } as WorkspaceDetails;
    const page = (details: WorkspaceDetails) => <SchedulePage ref={createRef()} workspaceId="ws-1" details={details} saving={false} />;
    const { rerender } = render(page(initial));
    expect(screen.getByRole('button', { name: 'Run once' })).toBeInTheDocument();
    await act(async () => rerender(page({ ...initial, scheduleScriptStatus: {
      pending: null, running: false, result: null, error: null,
    } })));
    expect(screen.queryByRole('button', { name: 'Run once' })).not.toBeInTheDocument();
    expect(screen.queryByLabelText('Captured condition script')).not.toBeInTheDocument();
    expect(mockInvoke).not.toHaveBeenCalledWith('workspace_script_decide', expect.anything());
  });

  it('shows timeout, output and execution errors from scheduled checks', () => {
    render(<SchedulePage ref={createRef()} workspaceId="ws-1" details={{ ...ENHANCED,
      scheduleScriptStatus: { pending: null, running: false, result: { ...RESULT, exitCode: null, timedOut: true,
        stderr: 'child output', error: 'Condition exceeded its timeout.' }, error: 'Condition execution failed.' },
    }} saving={false} />);
    expect(screen.getByRole('region', { name: 'Condition result' })).toHaveTextContent('Condition timed out · No exit status · 12 ms');
    expect(screen.getByText('child output')).toBeInTheDocument();
    expect(screen.getByText('Condition exceeded its timeout.')).toBeInTheDocument();
    expect(screen.getByRole('alert')).toHaveTextContent('Condition execution failed.');
  });
});


const SAVED_CONDITION = { source: { type: 'inline', content: 'exit 1' }, interpreter: 'sh', timeoutSeconds: 30 } as const;
const WITH_CONDITION = { ...ENHANCED, scheduleCondition: SAVED_CONDITION } as WorkspaceDetails;
const UNATTENDED_APPROVAL = { ...APPROVAL, purpose: 'unattended' };

describe('unattended condition consent', () => {
  it('reviews exact contents and access before checking unattended, then saves the opaque confirmation atomically', async () => {
    const user = userEvent.setup();
    const ref = createRef<SectionHandle>();
    const onDirtyChange = vi.fn();
    mockInvoke.mockImplementation((command) => Promise.resolve(command === 'workspace_script_preview' ? UNATTENDED_APPROVAL : command === 'workspace_script_authorize' ? 'confirmed-draft' : null));
    const page = (details: WorkspaceDetails) => <SchedulePage ref={ref} workspaceId="ws-1" details={details} saving={false} onDirtyChange={onDirtyChange} />;
    const { rerender } = render(page(WITH_CONDITION));
    await user.click(screen.getByRole('switch', { name: 'Run unattended' }));
    expect(mockInvoke).toHaveBeenCalledWith('workspace_script_preview', { workspaceId: 'ws-1', script: SAVED_CONDITION, purpose: 'unattended' });
    expect(screen.getByRole('switch', { name: 'Run unattended' })).not.toBeChecked();
    expect(screen.getByLabelText('Captured condition script')).toHaveTextContent('exit 1');
    expect(screen.getByText(APPROVAL.access)).toBeInTheDocument();
    expect(screen.getByLabelText('Condition script contents')).toBeDisabled();
    expect(screen.queryByRole('button', { name: 'Run once' })).not.toBeInTheDocument();
    expect(mockInvoke).not.toHaveBeenCalledWith('workspace_script_authorize', expect.anything());
    await user.click(screen.getByRole('button', { name: 'Approve unattended' }));
    expect(mockInvoke).toHaveBeenCalledWith('workspace_script_authorize', { workspaceId: 'ws-1', approvalId: 'approval-1' });
    expect(screen.getByRole('switch', { name: 'Run unattended' })).toBeChecked();
    expect(onDirtyChange).toHaveBeenLastCalledWith(true);
    await act(async () => rerender(page({ ...WITH_CONDITION, schedulePaused: true })));
    expect(screen.getByRole('switch', { name: 'Run unattended' })).toBeChecked();
    expect(await submit(ref)).toEqual({ ok: true });
    expect(mockInvoke).toHaveBeenCalledWith('workspace_set_schedule', {
      workspaceId: 'ws-1', kind: { type: 'enhancedInterval', intervalSeconds: 30 },
      allowedBlocks: { timezone: 'UTC', slots: [] }, condition: SAVED_CONDITION,
      unattended: true, unattendedApprovalId: 'confirmed-draft', ...EMPTY_HOOKS,
    });
    expect(mockInvoke).not.toHaveBeenCalledWith('workspace_script_decide', expect.anything());
    expect(mockInvoke).not.toHaveBeenCalledWith('workspace_run_now', expect.anything());
    await act(async () => rerender(page({ ...WITH_CONDITION, scheduleConditionUnattended: true })));
    expect(onDirtyChange).toHaveBeenLastCalledWith(false);
  });

  it('cancels unattended review without enabling, executing, or saving', async () => {
    const user = userEvent.setup();
    mockInvoke.mockImplementation((command) => Promise.resolve(command === 'workspace_script_preview' ? UNATTENDED_APPROVAL : null));
    render(<SchedulePage ref={createRef()} workspaceId="ws-1" details={WITH_CONDITION} saving={false} />);
    await user.click(screen.getByRole('switch', { name: 'Run unattended' }));
    await user.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(screen.getByRole('switch', { name: 'Run unattended' })).not.toBeChecked();
    expect(screen.getByLabelText('Condition script contents')).toBeEnabled();
    expect(mockInvoke).toHaveBeenCalledWith('workspace_script_decide', { workspaceId: 'ws-1', approvalId: 'approval-1', run: false });
    expect(mockInvoke).not.toHaveBeenCalledWith('workspace_script_authorize', expect.anything());
    expect(mockInvoke).not.toHaveBeenCalledWith('workspace_set_schedule', expect.anything());
  });

  it('clears saved unattended consent through Save and still confirms every Run now test', async () => {
    const user = userEvent.setup();
    const ref = createRef<SectionHandle>();
    mockInvoke.mockImplementation((command) => Promise.resolve(command === 'workspace_script_preview' ? APPROVAL : null));
    render(<SchedulePage ref={ref} workspaceId="ws-1" details={{ ...WITH_CONDITION, scheduleConditionUnattended: true }} saving={false} />);
    await user.click(screen.getByRole('button', { name: 'Run condition now' }));
    expect(mockInvoke).toHaveBeenCalledWith('workspace_script_preview', { workspaceId: 'ws-1', script: SAVED_CONDITION });
    expect(screen.getByRole('button', { name: 'Run once' })).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Approve unattended' })).not.toBeInTheDocument();
    expect(mockInvoke).not.toHaveBeenCalledWith('workspace_script_decide', expect.anything());
    await user.click(screen.getByRole('button', { name: 'Cancel' }));
    await user.click(screen.getByRole('switch', { name: 'Run unattended' }));
    expect(await submit(ref)).toEqual({ ok: true });
    expect(mockInvoke).toHaveBeenCalledWith('workspace_set_schedule', expect.objectContaining({ unattended: false, unattendedApprovalId: null }));
  });

  it.each(['Condition script contents', 'Condition interpreter', 'Condition timeout (seconds)'])('clears consent when %s changes', async (label) => {
    const user = userEvent.setup();
    const ref = createRef<SectionHandle>();
    mockInvoke.mockImplementation((command) => Promise.resolve(command === 'workspace_script_preview' ? UNATTENDED_APPROVAL : command === 'workspace_script_authorize' ? 'confirmed-draft' : null));
    render(<SchedulePage ref={ref} workspaceId="ws-1" details={WITH_CONDITION} saving={false} />);
    await user.click(screen.getByRole('switch', { name: 'Run unattended' }));
    await user.click(screen.getByRole('button', { name: 'Approve unattended' }));
    await user.clear(screen.getByLabelText(label));
    await user.type(screen.getByLabelText(label), label === 'Condition script contents' ? 'exit 0' : label === 'Condition interpreter' ? 'bash' : '60');
    expect(screen.getByRole('switch', { name: 'Run unattended' })).not.toBeChecked();
    expect(await submit(ref)).toEqual({ ok: true });
    expect(mockInvoke).toHaveBeenCalledWith('workspace_set_schedule', expect.objectContaining({ unattended: false, unattendedApprovalId: null }));
  });

  it('clears saved consent when selecting another script file and shows unreadable input errors', async () => {
    const user = userEvent.setup();
    const ref = createRef<SectionHandle>();
    mockInvoke.mockImplementation((command) => command === 'workspace_script_preview' ? Promise.reject('Cannot read condition script: /tmp/missing.sh') : Promise.resolve(null));
    render(<SchedulePage ref={ref} workspaceId="ws-1" details={{ ...WITH_CONDITION, scheduleConditionUnattended: true }} saving={false} />);
    await user.click(screen.getByRole('button', { name: 'Script file' }));
    await user.type(screen.getByLabelText('Condition script file'), '/tmp/missing.sh');
    expect(screen.getByRole('switch', { name: 'Run unattended' })).not.toBeChecked();
    await user.click(screen.getByRole('switch', { name: 'Run unattended' }));
    expect(screen.getByRole('alert')).toHaveTextContent('Cannot read condition script: /tmp/missing.sh');
    expect(screen.getByRole('switch', { name: 'Run unattended' })).not.toBeChecked();
    expect(screen.queryByRole('button', { name: 'Approve unattended' })).not.toBeInTheDocument();
    expect(mockInvoke).not.toHaveBeenCalledWith('workspace_script_authorize', expect.anything());
    expect(mockInvoke).not.toHaveBeenCalledWith('workspace_run_now', expect.anything());
  });

  it('shows a fresh scheduled approval after changed access is rejected and preserves an unsaved condition while renewing', async () => {
    const user = userEvent.setup();
    const ref = createRef<SectionHandle>();
    const refresh = vi.fn();
    const initial = { ...WITH_CONDITION, scheduleConditionUnattended: true, scheduleScriptStatus: {
      pending: { ...APPROVAL, purpose: 'scheduled' }, running: false, result: null, error: 'Condition access changed. Review fresh approval.',
    } } as WorkspaceDetails;
    const page = (details: WorkspaceDetails) => <SchedulePage ref={ref} workspaceId="ws-1" details={details} saving={false} onDetailsRefresh={refresh} />;
    const { rerender } = render(page(initial));
    mockInvoke.mockRejectedValueOnce('Condition access changed. Review fresh approval.');
    await user.click(screen.getByRole('button', { name: 'Approve unattended' }));
    expect(screen.getByRole('alert')).toHaveTextContent('Condition access changed. Review fresh approval.');
    expect(screen.queryByLabelText('Captured condition script')).not.toBeInTheDocument();
    expect(refresh).toHaveBeenCalled();
    await act(async () => rerender(page({ ...initial, scheduleScriptStatus: { ...initial.scheduleScriptStatus!, pending: { ...APPROVAL, id: 'renewed', purpose: 'scheduled', access: 'Full shell access.' } } })));
    expect(screen.getByText('Full shell access.')).toBeInTheDocument();
    await user.clear(screen.getByLabelText('Condition script contents'));
    await user.type(screen.getByLabelText('Condition script contents'), 'exit 0');
    mockInvoke.mockResolvedValueOnce('renewed-saved-condition');
    await user.click(screen.getByRole('button', { name: 'Approve unattended' }));
    expect(mockInvoke).toHaveBeenCalledWith('workspace_script_authorize', { workspaceId: 'ws-1', approvalId: 'renewed' });
    expect(screen.getByLabelText('Condition script contents')).toHaveValue('exit 0');
    expect(screen.getByRole('switch', { name: 'Run unattended' })).not.toBeChecked();
    expect(await submit(ref)).toEqual({ ok: true });
    expect(mockInvoke).toHaveBeenCalledWith('workspace_set_schedule', expect.objectContaining({ unattended: false, unattendedApprovalId: null }));
  });
});


const HOOK = { source: { type: 'inline', content: 'echo hook' }, interpreter: 'sh', timeoutSeconds: 300 } as const;
const hookPanel = (phase: 'before' | 'after') => within(screen.getByRole('region', { name: `${phase === 'before' ? 'Before' : 'After'} script` }));

describe('scheduled preparation and cleanup controls', () => {
  it('saves all script drafts, cadence and blocks atomically and preserves them through status polling', async () => {
    const user = userEvent.setup();
    const ref = createRef<SectionHandle>();
    const dirty = vi.fn();
    const page = (details: WorkspaceDetails) => <SchedulePage ref={ref} workspaceId="ws-1" details={details} saving={false} onDirtyChange={dirty} />;
    const { rerender } = render(page(ENHANCED));
    await user.click(screen.getByRole('switch', { name: 'Prepare before the agent runs' }));
    await user.click(screen.getByRole('switch', { name: 'Clean up after the scheduled attempt' }));
    expect(screen.getByLabelText('Before timeout (seconds)')).toHaveValue(300);
    expect(screen.getByLabelText('After timeout (seconds)')).toHaveValue(300);
    await user.type(screen.getByLabelText('Before script contents'), 'mkdir -p scratch');
    await user.type(screen.getByLabelText('After script contents'), 'rm -rf scratch');
    await user.click(screen.getByRole('button', { name: 'Monday 09:00–09:15' }));
    await act(async () => rerender(page({ ...ENHANCED, scheduleWaitingReason: 'Waiting outside allowed blocks.',
      scheduleBeforeStatus: { pending: null, running: false, result: null, error: 'Previous preparation failed.' },
    })));
    expect(screen.getByLabelText('Before script contents')).toHaveValue('mkdir -p scratch');
    expect(screen.getByLabelText('After script contents')).toHaveValue('rm -rf scratch');
    expect(dirty).toHaveBeenLastCalledWith(true);
    expect(await submit(ref)).toEqual({ ok: true });
    expect(mockInvoke).toHaveBeenCalledWith('workspace_set_schedule', {
      workspaceId: 'ws-1', kind: ENHANCED.scheduleKind,
      allowedBlocks: { timezone: 'UTC', slots: [36] }, condition: null, unattended: false, unattendedApprovalId: null,
      before: { ...HOOK, source: { type: 'inline', content: 'mkdir -p scratch' } },
      after: { ...HOOK, source: { type: 'inline', content: 'rm -rf scratch' } },
      beforeUnattended: false, afterUnattended: false, beforeUnattendedApprovalId: null, afterUnattendedApprovalId: null,
    });
    expect(hookPanel('after').getByText(/at-least-once execution/)).toBeInTheDocument();
  });

  it.each(['before', 'after'] as const)('validates the %s timeout before saving or testing', async (phase) => {
    const user = userEvent.setup();
    const ref = createRef<SectionHandle>();
    render(<SchedulePage ref={ref} workspaceId="ws-1" details={{ ...ENHANCED, scheduleBefore: HOOK, scheduleAfter: HOOK }} saving={false} />);
    const label = phase === 'before' ? 'Before' : 'After';
    await user.clear(screen.getByLabelText(`${label} timeout (seconds)`));
    expect(await submit(ref)).toEqual({ ok: false, error: `${label} timeout must be a positive whole number of seconds.` });
    await user.click(screen.getByRole('button', { name: `Run ${phase} now` }));
    expect(mockInvoke).not.toHaveBeenCalledWith('workspace_set_schedule', expect.anything());
    expect(mockInvoke).not.toHaveBeenCalledWith('workspace_script_preview', expect.anything());
  });

  it('confirms unsaved before and after tests independently, retaining each output and draft', async () => {
    const user = userEvent.setup();
    const ref = createRef<SectionHandle>();
    mockInvoke.mockImplementation((command, args) => {
      if (command === 'workspace_script_preview') {
        const phase = (args as { phase: 'before' | 'after' }).phase;
        return Promise.resolve({ ...APPROVAL, id: phase, phase, content: `echo ${phase}`, timeoutSeconds: 300 });
      }
      if (command === 'workspace_script_decide') {
        const phase = (args as { approvalId: string }).approvalId;
        return Promise.resolve({ ...RESULT, stdout: `${phase} output` });
      }
      return Promise.resolve(null);
    });
    render(<SchedulePage ref={ref} workspaceId="ws-1" details={{ ...ENHANCED, scheduleBefore: HOOK, scheduleAfter: HOOK }} saving={false} />);
    for (const phase of ['before', 'after'] as const) {
      const label = phase === 'before' ? 'Before' : 'After';
      await user.clear(screen.getByLabelText(`${label} script contents`));
      await user.type(screen.getByLabelText(`${label} script contents`), `echo ${phase}`);
      await user.click(screen.getByRole('button', { name: `Run ${phase} now` }));
      expect(mockInvoke).toHaveBeenCalledWith('workspace_script_preview', { workspaceId: 'ws-1', phase, script: { ...HOOK, source: { type: 'inline', content: `echo ${phase}` } } });
      expect(hookPanel(phase).getByLabelText(`Captured ${phase} script`)).toHaveTextContent(`echo ${phase}`);
      expect(hookPanel(phase).getByText(APPROVAL.access)).toBeInTheDocument();
      await user.type(screen.getByLabelText(`${label} script contents`), ' changed');
      expect(hookPanel(phase).getByLabelText(`Captured ${phase} script`)).toHaveTextContent(`echo ${phase}`);
      await user.click(hookPanel(phase).getByRole('button', { name: 'Run once' }));
      expect(screen.getByRole('region', { name: `${label} result` })).toHaveTextContent(`${label} failed · Exit 1 · 12 ms`);
      expect(screen.getByLabelText(`${label} script contents`)).toHaveValue(`echo ${phase} changed`);
    }
    expect(screen.getByText('before output')).toBeInTheDocument();
    expect(screen.getByText('after output')).toBeInTheDocument();
    expect(mockInvoke).not.toHaveBeenCalledWith('workspace_set_schedule', expect.anything());
    expect(mockInvoke).not.toHaveBeenCalledWith('workspace_run_now', expect.anything());
  });

  it('uses the file chooser and interpreter for the selected hook only', async () => {
    const user = userEvent.setup();
    mockOpen.mockResolvedValue('/tmp/cleanup.py');
    mockInvoke.mockImplementation((command) => command === 'workspace_script_preview' ? Promise.reject('Interpreter is unavailable: python3') : Promise.resolve(null));
    render(<SchedulePage ref={createRef()} workspaceId="ws-1" details={{ ...ENHANCED, scheduleBefore: HOOK, scheduleAfter: HOOK }} saving={false} />);
    await user.click(hookPanel('after').getByRole('button', { name: 'Script file' }));
    await user.click(hookPanel('after').getByRole('button', { name: 'Choose file…' }));
    expect(mockOpen).toHaveBeenCalledWith({ title: 'Select after script', multiple: false, directory: false });
    await user.clear(screen.getByLabelText('After interpreter'));
    await user.type(screen.getByLabelText('After interpreter'), 'python3');
    await user.click(screen.getByRole('button', { name: 'Run after now' }));
    expect(mockInvoke).toHaveBeenCalledWith('workspace_script_preview', { workspaceId: 'ws-1', phase: 'after', script: {
      source: { type: 'file', path: '/tmp/cleanup.py' }, interpreter: 'python3', timeoutSeconds: 300,
    } });
    expect(hookPanel('after').getByRole('alert')).toHaveTextContent('Interpreter is unavailable: python3');
    expect(screen.getByLabelText('Before script contents')).toHaveValue('echo hook');
  });

  it('keeps unattended confirmations separate and clears only the edited phase consent', async () => {
    const user = userEvent.setup();
    const ref = createRef<SectionHandle>();
    mockInvoke.mockImplementation((command, args) => {
      if (command === 'workspace_script_preview') {
        const phase = (args as { phase: 'before' | 'after' }).phase;
        return Promise.resolve({ ...APPROVAL, id: phase, phase, purpose: 'unattended', content: 'echo hook', timeoutSeconds: 300 });
      }
      if (command === 'workspace_script_authorize') return Promise.resolve(`confirmed-${(args as { approvalId: string }).approvalId}`);
      return Promise.resolve(null);
    });
    const initial = { ...WITH_CONDITION, scheduleConditionUnattended: true, scheduleBefore: HOOK, scheduleAfter: HOOK };
    const page = (details: WorkspaceDetails) => <SchedulePage ref={ref} workspaceId="ws-1" details={details} saving={false} />;
    const { rerender } = render(page(initial));
    for (const phase of ['before', 'after'] as const) {
      await user.click(hookPanel(phase).getByRole('switch', { name: 'Run unattended' }));
      expect(hookPanel(phase).getByRole('switch', { name: 'Run unattended' })).not.toBeChecked();
      await user.click(hookPanel(phase).getByRole('button', { name: 'Approve unattended' }));
      expect(hookPanel(phase).getByRole('switch', { name: 'Run unattended' })).toBeChecked();
    }
    await act(async () => rerender(page({ ...initial, schedulePaused: true })));
    expect(await submit(ref)).toEqual({ ok: true });
    expect(mockInvoke).toHaveBeenLastCalledWith('workspace_set_schedule', expect.objectContaining({
      condition: SAVED_CONDITION, unattended: true,
      before: HOOK, beforeUnattended: true, beforeUnattendedApprovalId: 'confirmed-before',
      after: HOOK, afterUnattended: true, afterUnattendedApprovalId: 'confirmed-after',
    }));
    await user.type(screen.getByLabelText('Before script contents'), ' changed');
    expect(hookPanel('before').getByRole('switch', { name: 'Run unattended' })).not.toBeChecked();
    expect(hookPanel('after').getByRole('switch', { name: 'Run unattended' })).toBeChecked();
    expect(await submit(ref)).toEqual({ ok: true });
    expect(mockInvoke).toHaveBeenLastCalledWith('workspace_set_schedule', expect.objectContaining({
      unattended: true, beforeUnattended: false, beforeUnattendedApprovalId: null,
      afterUnattended: true, afterUnattendedApprovalId: 'confirmed-after',
    }));
  });

  it.each([true, false])('keeps cleanup approval actionable after disabling and switching to legacy mode (run=%s)', async (run) => {
    const user = userEvent.setup();
    const initial = { ...ENHANCED, scheduleAfter: HOOK, scheduleCleanupPending: true, scheduleAfterStatus: {
      pending: { ...APPROVAL, id: 'cleanup', phase: 'after', purpose: 'scheduled' }, running: false, result: null, error: null,
    } } as WorkspaceDetails;
    const page = (details: WorkspaceDetails) => <SchedulePage ref={createRef()} workspaceId="ws-1" details={details} saving={false} />;
    const { rerender } = render(page(initial));
    await act(async () => rerender(page({ ...initial, scheduleEnabled: false, scheduleKind: { type: 'interval', intervalMinutes: 30 },
      scheduleWaitingReason: 'Outside allowed blocks.',
    })));
    expect(hookPanel('after').getByText(/A scheduled cleanup approval stays available outside allowed blocks/)).toBeInTheDocument();
    expect(hookPanel('after').getByRole('button', { name: 'Run once' })).toBeEnabled();
    expect(hookPanel('after').getByRole('button', { name: 'Skip' })).toBeEnabled();
    await user.click(hookPanel('after').getByRole('button', { name: run ? 'Run once' : 'Skip' }));
    expect(mockInvoke).toHaveBeenCalledWith('workspace_script_decide', { workspaceId: 'ws-1', approvalId: 'cleanup', run });
    expect(mockInvoke).not.toHaveBeenCalledWith('workspace_set_schedule', expect.anything());
  });

  it('removes an expired before approval without losing either hook draft', async () => {
    const user = userEvent.setup();
    const initial = { ...ENHANCED, scheduleBefore: HOOK, scheduleAfter: HOOK, scheduleBeforeStatus: {
      pending: { ...APPROVAL, phase: 'before', purpose: 'scheduled' }, running: false, result: null, error: null,
    } } as WorkspaceDetails;
    const page = (details: WorkspaceDetails) => <SchedulePage ref={createRef()} workspaceId="ws-1" details={details} saving={false} />;
    const { rerender } = render(page(initial));
    await user.type(screen.getByLabelText('Before script contents'), ' draft');
    await user.type(screen.getByLabelText('After script contents'), ' draft');
    await act(async () => rerender(page({ ...initial, scheduleBeforeStatus: { pending: null, running: false, result: null, error: null } })));
    expect(hookPanel('before').queryByRole('button', { name: 'Run once' })).not.toBeInTheDocument();
    expect(screen.getByLabelText('Before script contents')).toHaveValue('echo hook draft');
    expect(screen.getByLabelText('After script contents')).toHaveValue('echo hook draft');
  });

  it('shows outstanding cleanup attention and independent failed/timed out phase results', () => {
    render(<SchedulePage ref={createRef()} workspaceId="ws-1" details={{ ...ENHANCED, scheduleCleanupPending: true,
      scheduleBeforeStatus: { pending: null, running: false, result: { ...RESULT, stdout: 'prepare output' }, error: null },
      scheduleAfterStatus: { pending: null, running: false, result: { ...RESULT, timedOut: true, exitCode: null,
        stdout: 'cleanup output', error: 'After script exceeded its timeout.' }, error: 'Interrupted cleanup requires attention.' },
    }} saving={false} />);
    expect(screen.getByRole('status')).toHaveTextContent('Cleanup pending.');
    expect(screen.getByRole('status')).toHaveTextContent('A settings test does not complete this cleanup.');
    expect(screen.getByRole('region', { name: 'Before result' })).toHaveTextContent('Before failed · Exit 1');
    expect(screen.getByRole('region', { name: 'After result' })).toHaveTextContent('After timed out · No exit status');
    expect(screen.getByText('prepare output')).toBeInTheDocument();
    expect(screen.getByText('cleanup output')).toBeInTheDocument();
    expect(hookPanel('after').getByRole('alert')).toHaveTextContent('Interrupted cleanup requires attention.');
  });
  it('keeps an explicit Skip available for interrupted cleanup with an unavailable bound input', async () => {
    const user = userEvent.setup();
    render(<SchedulePage ref={createRef()} workspaceId="ws-1" details={{ ...DETAILS,
      scheduleCleanupPending: true, scheduleCleanupId: 'intent-8', scheduleCleanupInterrupted: true,
      scheduleAfterStatus: { pending: null, result: null, running: false, error: 'Cannot read original cleanup file.' },
    }} saving={false} />);
    expect(hookPanel('after').getByText(/Preparation or the agent may not have started/)).toBeInTheDocument();
    await user.click(hookPanel('after').getByRole('button', { name: 'Skip outstanding cleanup' }));
    expect(mockInvoke).toHaveBeenCalledWith('workspace_cleanup_skip', { workspaceId: 'ws-1', intentId: 'intent-8' });
    expect(mockInvoke).not.toHaveBeenCalledWith('workspace_set_schedule', expect.anything());
  });

  it('requires fresh Run once after restart and retains a bound approval when revalidation fails', async () => {
    const user = userEvent.setup();
    mockInvoke.mockImplementation((command) => command === 'workspace_script_decide'
      ? Promise.reject('The bound cleanup input changed. Restore it or explicitly Skip cleanup.') : Promise.resolve(null));
    render(<SchedulePage ref={createRef()} workspaceId="ws-1" details={{ ...DETAILS,
      scheduleCleanupPending: true, scheduleCleanupId: 'intent-8', scheduleCleanupInterrupted: true,
      scheduleAfterStatus: { pending: { ...APPROVAL, id: 'recovery-8', phase: 'after', purpose: 'recovery' },
        result: null, running: false, error: null },
    }} saving={false} />);
    expect(hookPanel('after').getByRole('heading', { name: 'Interrupted cleanup needs fresh approval' })).toBeInTheDocument();
    expect(hookPanel('after').queryByRole('button', { name: 'Approve unattended' })).not.toBeInTheDocument();
    await user.click(hookPanel('after').getByRole('button', { name: 'Run once' }));
    expect(hookPanel('after').getByRole('button', { name: 'Run once' })).toBeEnabled();
    expect(hookPanel('after').getByRole('button', { name: 'Skip' })).toBeEnabled();
    expect(hookPanel('after').getByRole('alert')).toHaveTextContent('Restore it or explicitly Skip cleanup.');
  });

});
