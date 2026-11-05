import { beforeEach, expect, it, vi } from 'vitest';
import { act, fireEvent, render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { createRef, useState } from 'react';
import ScheduleCondition from './ScheduleCondition';
import type { ScriptConfig } from '../../generated/bindings';
import type { VariableMetadata, SaveVariable } from '../../generated/bindings';
import { GeneralPage } from './WorkspacePages';
import VariablesSettings from './VariablesSettings';
import type { SectionHandle } from './sectionHandle';

const fields = vi.hoisted(() => ({ records: [] as VariableMetadata[] }));
vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));
import { invoke } from '@tauri-apps/api/core';
import { saveVariable, VARIABLES_CHANGED_EVENT } from '../../variables';

function ScriptEditor() {
  const [value, onChange] = useState<ScriptConfig | null>({ source: { type: 'inline', content: 'start END finish' }, interpreter: 'sh', timeoutSeconds: 30 });
  return <ScheduleCondition workspaceId="alpha" value={value} onChange={onChange} status={null} disabled={false} unattended={false} onUnattendedChange={() => {}} onScheduledAuthorize={() => {}} />;
}

beforeEach(() => {
  vi.mocked(invoke).mockClear();
  vi.mocked(invoke).mockImplementation(async (cmd: string, args?: unknown) => {
    if (cmd === 'workspace_variables' || cmd === 'variables_list') return structuredClone(fields.records);
    if (cmd === 'workspace_list') return [{ id: 'alpha', title: 'Alpha' }];
    if (cmd === 'variables_save') {
      const input = (args as { input: SaveVariable }).input;
      if (input.value.type !== 'text') throw new Error('This fixture saves Text only');
      const definition: VariableMetadata = { id: input.id ?? 'new', revision: (input.expectedRevision ?? 0) + 1,
        name: input.name, kind: 'text', value: input.value.value, workspaceIds: input.workspaceIds };
      fields.records = [...fields.records.filter((field) => field.id !== definition.id), definition];
      return { definition, warning: null };
    }
    return [];
  });
  fields.records = [
    { id: 'text', revision: 1, name: 'MESSAGE', kind: 'text', value: 'literal $HOME', workspaceIds: ['alpha'] },
    { id: 'protected', revision: 1, name: 'TOKEN', kind: 'protected', value: null, workspaceIds: ['alpha'] },
    { id: 'denied', revision: 1, name: 'OTHER', kind: 'text', value: 'not assigned', workspaceIds: ['beta'] },
  ];
});

// A successful app-Settings commit must refresh workspace metadata without replacing drafts.
it('refreshes assigned metadata after Settings saves while retaining the workspace draft', async () => {
  const user = userEvent.setup();
  render(<><GeneralPage ref={createRef<SectionHandle>()} workspaceId="alpha" details={null} saving={false} /><VariablesSettings /></>);
  await screen.findByRole('button', { name: 'MESSAGE · Text' });
  await user.type(screen.getByLabelText(/Title/), 'Unsaved title');
  await user.click(await screen.findByRole('button', { name: 'Add field' }));
  await user.type(screen.getByLabelText('Name', { exact: true }), 'NEW_MESSAGE');
  await user.type(screen.getByLabelText('Value', { exact: true }), 'new value');
  await user.click(screen.getByRole('checkbox', { name: 'Alpha' }));
  await user.click(screen.getByRole('button', { name: 'Save' }));
  expect(await screen.findByRole('button', { name: 'NEW_MESSAGE · Text' })).toBeVisible();
  expect(screen.getByLabelText(/Title/)).toHaveValue('Unsaved title');
});

it('searches assigned references and replaces the real script selection without inserting payloads', async () => {
  const user = userEvent.setup();
  render(<ScriptEditor />);
  const editor = screen.getByLabelText('Condition script contents') as HTMLTextAreaElement;
  editor.setSelectionRange(6, 9);
  await user.click(screen.getByRole('button', { name: 'Insert condition variable' }));
  await user.type(screen.getByRole('searchbox', { name: 'Search condition variables' }), 'message');
  expect(screen.queryByRole('button', { name: /Insert OTHER/ })).not.toBeInTheDocument();
  expect(screen.queryByText('literal $HOME')).not.toBeInTheDocument();
  await user.click(await screen.findByRole('button', { name: 'Insert MESSAGE · Text' }));
  expect(editor).toHaveValue('start "${CLAI_VAR_MESSAGE?Missing or unavailable}" finish');
});

// Removing metadata delivery/filtering or protected masking must fail through the workspace page.
it('shows assigned workspace metadata with plain hover/focus/touch details and protected masking', async () => {
  const user = userEvent.setup();
  render(<GeneralPage ref={createRef<SectionHandle>()} workspaceId="alpha" details={null} saving={false} />);
  const message = await screen.findByRole('button', { name: 'MESSAGE · Text' });
  expect(screen.queryByRole('button', { name: /OTHER/ })).not.toBeInTheDocument();
  fireEvent.mouseEnter(message);
  expect(screen.getByText('literal $HOME')).toBeVisible();
  const token = screen.getByRole('button', { name: 'TOKEN · Protected string' });
  act(() => token.focus());
  expect(await screen.findByText(/Protected string · ••••/)).toBeVisible();
  await user.click(message);
  expect(screen.getByText('literal $HOME')).toBeVisible();
  expect(vi.mocked(invoke).mock.calls.some(([cmd]) => cmd === 'variables_reveal')).toBe(false);
});

it('ignores an old workspace response after switching identity', async () => {
  let finishAlpha!: (fields: VariableMetadata[]) => void;
  const alpha = new Promise<VariableMetadata[]>((resolve) => { finishAlpha = resolve; });
  vi.mocked(invoke).mockImplementation(async (_cmd: string, args?: unknown) => (args as { workspaceId: string }).workspaceId === 'alpha' ? alpha : structuredClone(fields.records));
  const props = { ref: createRef<SectionHandle>(), details: null, saving: false };
  const view = render(<GeneralPage {...props} workspaceId="alpha" />);
  view.rerender(<GeneralPage {...props} workspaceId="beta" />);
  expect(await screen.findByRole('button', { name: 'OTHER · Text' })).toBeVisible();
  await act(async () => finishAlpha(structuredClone(fields.records)));
  expect(screen.queryByRole('button', { name: 'MESSAGE · Text' })).not.toBeInTheDocument();
  expect(screen.getByRole('button', { name: 'OTHER · Text' })).toBeVisible();
});

it('lets keyboard and touch users inspect the complete multiline plain value', async () => {
  const value = Array.from({ length: 30 }, (_, index) => `Line ${index + 1}`).join('\n');
  fields.records[0]!.value = value;
  const user = userEvent.setup();
  render(<GeneralPage ref={createRef<SectionHandle>()} workspaceId="alpha" details={null} saving={false} />);
  await user.click(await screen.findByRole('button', { name: 'MESSAGE · Text' }));
  const show = screen.getByRole('button', { name: 'View full value' });
  await user.click(show);
  const details = screen.getByRole('dialog', { name: 'MESSAGE value' });
  expect(within(details).getByLabelText('Full value').textContent).toBe(value);
  await user.keyboard('{Escape}');
  expect(screen.queryByRole('dialog', { name: 'MESSAGE value' })).not.toBeInTheDocument();
  expect(show).toHaveFocus();
});

it('ignores a late refresh and retains a dirty script while assigned metadata changes', async () => {
  const user = userEvent.setup();
  render(<ScriptEditor />);
  const editor = screen.getByLabelText('Condition script contents');
  await user.clear(editor);
  await user.type(editor, 'unsaved script');
  await user.click(screen.getByRole('button', { name: 'Insert condition variable' }));
  await screen.findByRole('button', { name: 'Insert MESSAGE · Text' });
  const stale = structuredClone(fields.records);
  let finishOld!: (fields: VariableMetadata[]) => void;
  const old = new Promise<VariableMetadata[]>((resolve) => { finishOld = resolve; });
  vi.mocked(invoke).mockImplementationOnce(() => old);
  act(() => window.dispatchEvent(new Event(VARIABLES_CHANGED_EVENT)));
  await act(async () => {
    await saveVariable({ id: 'text', expectedRevision: 1, name: 'RENAMED', value: { type: 'text', value: 'new payload' }, workspaceIds: ['alpha'] });
  });
  expect(await screen.findByRole('button', { name: 'Insert RENAMED · Text' })).toBeVisible();
  await act(async () => finishOld(stale));
  expect(screen.queryByRole('button', { name: 'Insert MESSAGE · Text' })).not.toBeInTheDocument();
  expect(editor).toHaveValue('unsaved script');
  await user.type(screen.getByRole('searchbox', { name: 'Search condition variables' }), 'not-assigned');
  expect(screen.getByRole('status')).toHaveTextContent('Missing or unavailable');
  await user.keyboard('{Escape}');
  expect(screen.getByRole('button', { name: 'Insert condition variable' })).toHaveFocus();
});
