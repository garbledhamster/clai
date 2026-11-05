import { beforeEach, expect, it, vi } from 'vitest';
import { act, render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import VariablesSettings from './VariablesSettings';

const backend = vi.hoisted(() => ({
  records: [] as Record<string, unknown>[],
  failure: false,
  holdSave: null as Promise<void> | null,
  holdReveal: null as Promise<void> | null,
  warning: null as string | null,
}));
vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(async (command: string, args?: { input: Record<string, unknown> }) => {
    if (command === 'variables_reveal') {
      if (backend.holdReveal) await backend.holdReveal;
      return 'disposable-ui-secret';
    }
    if (command === 'variables_list') return structuredClone(backend.records);
    if (command === 'workspace_list')
      return [
        { id: 'workspace-a', title: 'Alpha' },
        { id: 'workspace-b', title: 'Beta' },
      ];
    if (command === 'variables_save') {
      if (backend.holdSave) await backend.holdSave;
      if (backend.failure) throw new Error('Storage unavailable');
      const input = args!.input;
      const value = input.value as {
        type: string;
        value: string | boolean;
        replacement?: string | null;
      };
      const saved = {
        ...input,
        kind: value.type,
        value:
          value.type === 'protected'
            ? null
            : value.type === 'number'
              ? Number(value.value)
              : value.value,
        id: input.id ?? `field-${backend.records.length + 1}`,
        revision: Number(input.expectedRevision ?? 0) + 1,
      };
      backend.records = [...backend.records.filter((r) => r.id !== saved.id), saved];
      return { definition: saved, warning: backend.warning };
    }
    return [];
  }),
}));

beforeEach(() => {
  backend.records = [];
  backend.failure = false;
  backend.holdSave = null;
  backend.holdReveal = null;
  backend.warning = null;
});

// Catches a visible form that does not actually commit its values and grants.
it('creates a Text field with selected workspace access and reloads it', async () => {
  const user = userEvent.setup();
  const view = render(<VariablesSettings />);
  await user.click(await screen.findByRole('button', { name: 'Add field' }));
  await user.type(screen.getByLabelText('Name', { exact: true }), 'MESSAGE');
  await user.type(screen.getByLabelText('Value', { exact: true }), 'literal $HOME');
  await user.click(screen.getByRole('checkbox', { name: 'Alpha' }));
  await user.click(screen.getByRole('button', { name: 'Save' }));
  expect(await screen.findByRole('status')).toHaveTextContent('Saved');
  view.unmount();
  render(<VariablesSettings />);
  await user.click(await screen.findByRole('button', { name: 'MESSAGE' }));
  expect(screen.getByLabelText('Value', { exact: true })).toHaveValue('literal $HOME');
  expect(screen.getByRole('checkbox', { name: 'Alpha' })).toBeChecked();
  expect(screen.getByRole('checkbox', { name: 'Beta' })).not.toBeChecked();
});

it('keeps an unsaved draft when changing field selection and supports discard', async () => {
  backend.records = [
    { id: 'one', revision: 1, kind: 'text', name: 'FIRST', value: 'original', workspaceIds: [] },
    { id: 'two', revision: 1, kind: 'text', name: 'SECOND', value: 'second', workspaceIds: [] },
  ];
  const user = userEvent.setup();
  render(<VariablesSettings />);
  await user.click(await screen.findByRole('button', { name: 'FIRST' }));
  await user.type(screen.getByLabelText('Value', { exact: true }), ' edit');
  await user.click(screen.getByRole('button', { name: 'SECOND' }));
  await user.click(screen.getByRole('button', { name: 'Keep editing' }));
  expect(screen.getByLabelText('Value', { exact: true })).toHaveValue('original edit');
  await user.click(screen.getByRole('button', { name: 'SECOND' }));
  await user.click(screen.getByRole('button', { name: 'Discard' }));
  expect(screen.getByLabelText('Value', { exact: true })).toHaveValue('second');
});

it('keeps grant selection across filters and reports failed saves without losing the draft', async () => {
  const user = userEvent.setup();
  render(<VariablesSettings />);
  await user.click(await screen.findByRole('button', { name: 'Add field' }));
  await user.type(screen.getByLabelText('Name', { exact: true }), 'NEW_FIELD');
  await user.click(await screen.findByRole('checkbox', { name: 'Beta' }));
  await user.click(screen.getByRole('checkbox', { name: 'Selected only' }));
  expect(screen.queryByRole('checkbox', { name: 'Alpha' })).toBeNull();
  expect(screen.getByText('1 selected')).toBeVisible();
  await user.type(screen.getByLabelText('Search workspaces'), 'absent');
  await user.clear(screen.getByLabelText('Search workspaces'));
  expect(screen.getByRole('checkbox', { name: 'Beta' })).toBeChecked();
  backend.failure = true;
  await user.click(screen.getByRole('button', { name: 'Save' }));
  expect(await screen.findByRole('alert')).toHaveTextContent('Storage unavailable');
  expect(screen.getByLabelText('Name', { exact: true })).toHaveValue('NEW_FIELD');
  expect(screen.getByRole('checkbox', { name: 'Beta' })).toBeChecked();
});

it('warns when renaming and saves before carrying out a pending selection', async () => {
  backend.records = [
    { id: 'one', revision: 1, kind: 'text', name: 'FIRST', value: 'original', workspaceIds: [] },
  ];
  const user = userEvent.setup();
  render(<VariablesSettings />);
  await user.click(await screen.findByRole('button', { name: 'FIRST' }));
  await user.clear(screen.getByLabelText('Name', { exact: true }));
  await user.type(screen.getByLabelText('Name', { exact: true }), 'RENAMED');
  expect(screen.getByText(/existing references are not rewritten/i)).toBeVisible();
  await user.click(screen.getByRole('button', { name: 'Add field' }));
  await user.click(screen.getByRole('button', { name: 'Save' }));
  expect(await screen.findByRole('button', { name: 'RENAMED' })).toBeVisible();
  expect(screen.getByLabelText('Name', { exact: true })).toHaveValue('');
});

it('preserves details while switching to workspace access', async () => {
  const user = userEvent.setup();
  render(<VariablesSettings />);
  await user.click(await screen.findByRole('button', { name: 'Add field' }));
  await user.type(screen.getByLabelText('Name', { exact: true }), 'RETAINED');
  await user.click(screen.getByRole('tab', { name: 'Workspaces' }));
  await user.click(await screen.findByRole('checkbox', { name: 'Beta' }));
  await user.click(screen.getByRole('tab', { name: 'Details' }));
  expect(screen.getByLabelText('Name', { exact: true })).toHaveValue('RETAINED');
  await user.click(screen.getByRole('tab', { name: 'Workspaces' }));
  expect(screen.getByRole('checkbox', { name: 'Beta' })).toBeChecked();
});

it('prevents edits during an in-flight save from being silently overwritten', async () => {
  let finish!: () => void;
  backend.holdSave = new Promise<void>((resolve) => {
    finish = resolve;
  });
  const user = userEvent.setup();
  render(<VariablesSettings />);
  await user.click(await screen.findByRole('button', { name: 'Add field' }));
  await user.type(screen.getByLabelText('Name', { exact: true }), 'LOCKED');
  await user.click(screen.getByRole('button', { name: 'Save' }));
  expect(screen.getByLabelText('Name', { exact: true })).toBeDisabled();
  expect(screen.getByLabelText('Value', { exact: true })).toBeDisabled();
  expect(screen.getByRole('checkbox', { name: 'Alpha' })).toBeDisabled();
  await act(async () => {
    finish();
  });
  expect(await screen.findByRole('status')).toHaveTextContent('Saved');
  expect(screen.getByLabelText('Name', { exact: true })).toBeEnabled();
});

it('keeps the current dirty field when its selected list entry is clicked', async () => {
  backend.records = [
    { id: 'one', revision: 1, kind: 'text', name: 'FIRST', value: 'original', workspaceIds: [] },
  ];
  const user = userEvent.setup();
  render(<VariablesSettings />);
  await user.click(await screen.findByRole('button', { name: 'FIRST' }));
  await user.type(screen.getByLabelText('Value', { exact: true }), ' edited');
  await user.click(screen.getByRole('button', { name: 'FIRST' }));
  expect(screen.queryByText('Save changes before leaving?')).toBeNull();
  await user.click(screen.getByRole('button', { name: 'Save' }));
  expect(await screen.findByRole('status')).toHaveTextContent('Saved');
  expect(screen.getByLabelText('Value', { exact: true })).toHaveValue('original edited');
});

it('allows an unavailable saved workspace grant to be removed', async () => {
  backend.records = [
    {
      id: 'one',
      revision: 1,
      name: 'FIRST',
      value: 'original',
      workspaceIds: ['deleted-workspace'],
    },
  ];
  const user = userEvent.setup();
  render(<VariablesSettings />);
  await user.click(await screen.findByRole('button', { name: 'FIRST' }));
  const unavailable = screen.getByRole('checkbox', { name: 'Unavailable workspace' });
  expect(unavailable).toBeChecked();
  await user.click(unavailable);
  await user.click(screen.getByRole('button', { name: 'Save' }));
  expect(await screen.findByRole('status')).toHaveTextContent('Saved');
  expect(screen.getByText('0 selected')).toBeVisible();
  expect(screen.queryByRole('checkbox', { name: 'Unavailable workspace' })).toBeNull();
});

it('edits Boolean values with True and False radio buttons', async () => {
  const user = userEvent.setup();
  render(<VariablesSettings />);
  await user.click(await screen.findByRole('button', { name: 'Add field' }));
  await user.type(screen.getByLabelText('Name', { exact: true }), 'ENABLED');
  await user.selectOptions(screen.getByLabelText('Type'), 'boolean');
  expect(screen.getByRole('radio', { name: 'False' })).toBeChecked();
  await user.click(screen.getByRole('radio', { name: 'True' }));
  expect(screen.getByRole('radio', { name: 'True' })).toBeChecked();
  await user.keyboard('{ArrowRight}');
  expect(screen.getByRole('radio', { name: 'False' })).toBeChecked();
  await user.click(screen.getByRole('button', { name: 'Save' }));
  expect(await screen.findByRole('status')).toHaveTextContent('Saved');
  expect(screen.getByRole('radio', { name: 'False' })).toBeChecked();
});

it('rejects blank numeric input and saves a finite decimal Number', async () => {
  const user = userEvent.setup();
  render(<VariablesSettings />);
  await user.click(await screen.findByRole('button', { name: 'Add field' }));
  await user.type(screen.getByLabelText('Name', { exact: true }), 'AMOUNT');
  await user.selectOptions(screen.getByLabelText('Type'), 'number');
  expect(screen.getByRole('spinbutton', { name: 'Value' })).toBeVisible();
  await user.click(screen.getByRole('button', { name: 'Save' }));
  expect(await screen.findByRole('alert')).toHaveTextContent('Enter a finite number');
  await user.type(screen.getByRole('spinbutton', { name: 'Value' }), '-12.5');
  await user.click(screen.getByRole('button', { name: 'Save' }));
  expect(await screen.findByRole('status')).toHaveTextContent('Saved');
  expect(screen.getByRole('spinbutton', { name: 'Value' })).toHaveValue(-12.5);
});

it('masks saved protected strings until deliberate reveal and remasks on selection', async () => {
  backend.records = [
    { id: 'secret', revision: 1, kind: 'protected', name: 'SECRET', value: null, workspaceIds: [] },
    { id: 'text', revision: 1, kind: 'text', name: 'TEXT', value: 'literal', workspaceIds: [] },
  ];
  const user = userEvent.setup();
  render(<VariablesSettings />);
  await user.click(await screen.findByRole('button', { name: 'SECRET' }));
  expect(screen.getByLabelText('Value', { exact: true })).toHaveAttribute('type', 'password');
  expect(screen.queryByDisplayValue('disposable-ui-secret')).toBeNull();
  await user.click(screen.getByRole('button', { name: 'Reveal' }));
  expect(await screen.findByDisplayValue('disposable-ui-secret')).toHaveAttribute('type', 'text');
  await user.click(screen.getByRole('button', { name: 'TEXT' }));
  await user.click(screen.getByRole('button', { name: 'SECRET' }));
  expect(screen.queryByDisplayValue('disposable-ui-secret')).toBeNull();
  expect(screen.getByLabelText('Value', { exact: true })).toHaveAttribute('type', 'password');
});

it('discards a stale reveal response after switching fields', async () => {
  let finish!: () => void;
  backend.holdReveal = new Promise<void>((resolve) => {
    finish = resolve;
  });
  backend.records = [
    { id: 'secret', revision: 1, kind: 'protected', name: 'SECRET', value: null, workspaceIds: [] },
    { id: 'text', revision: 1, kind: 'text', name: 'TEXT', value: 'literal', workspaceIds: [] },
  ];
  const user = userEvent.setup();
  render(<VariablesSettings />);
  await user.click(await screen.findByRole('button', { name: 'SECRET' }));
  await user.click(screen.getByRole('button', { name: 'Reveal' }));
  await user.click(screen.getByRole('button', { name: 'TEXT' }));
  await act(async () => {
    finish();
  });
  expect(screen.getByLabelText('Value', { exact: true })).toHaveValue('literal');
  await user.click(screen.getByRole('button', { name: 'SECRET' }));
  expect(screen.queryByDisplayValue('disposable-ui-secret')).toBeNull();
});

it('preserves a saved protected value through grant-only saves and requires explicit replacement', async () => {
  backend.records = [
    { id: 'secret', revision: 1, kind: 'protected', name: 'SECRET', value: null, workspaceIds: [] },
  ];
  const user = userEvent.setup();
  render(<VariablesSettings />);
  await user.click(await screen.findByRole('button', { name: 'SECRET' }));
  expect(screen.getByLabelText('Value', { exact: true })).toHaveAttribute('readonly');
  await user.click(screen.getByRole('checkbox', { name: 'Alpha' }));
  await user.click(screen.getByRole('button', { name: 'Save' }));
  expect(await screen.findByRole('status')).toHaveTextContent('Saved');
  expect(screen.getByLabelText('Value', { exact: true })).toHaveAttribute('readonly');
  await user.click(screen.getByRole('button', { name: 'Replace value' }));
  expect(screen.getByLabelText('Value', { exact: true })).not.toHaveAttribute('readonly');
  await user.type(screen.getByLabelText('Value', { exact: true }), 'new-disposable-secret');
  await user.click(screen.getByRole('button', { name: 'Save' }));
  expect(await screen.findByRole('status')).toHaveTextContent('Saved');
  expect(screen.queryByDisplayValue('new-disposable-secret')).toBeNull();
  expect(screen.getByLabelText('Value', { exact: true })).toHaveAttribute('type', 'password');
});

it('requires acknowledgement of a committed-save warning before pending navigation', async () => {
  backend.records = [
    { id: 'one', revision: 1, kind: 'text', name: 'FIRST', value: 'original', workspaceIds: [] },
  ];
  backend.warning = 'Saved; unused protected credential could not be removed';
  const user = userEvent.setup();
  render(<VariablesSettings />);
  await user.click(await screen.findByRole('button', { name: 'FIRST' }));
  await user.type(screen.getByLabelText('Value', { exact: true }), ' edited');
  await user.click(screen.getByRole('button', { name: 'Add field' }));
  await user.click(screen.getByRole('button', { name: 'Save' }));
  expect(await screen.findByRole('alert')).toHaveTextContent(
    'unused protected credential could not be removed'
  );
  expect(screen.getByLabelText('Name', { exact: true })).toHaveValue('FIRST');
  await user.click(screen.getByRole('button', { name: 'Acknowledge and continue' }));
  expect(screen.getByLabelText('Name', { exact: true })).toHaveValue('');
});
