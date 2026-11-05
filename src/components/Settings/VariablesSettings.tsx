import { useEffect, useImperativeHandle, useRef, useState, type Ref } from 'react';
import type { VariableKind, VariableMetadata } from '../../generated/bindings';
import { listVariables, revealVariable, saveVariable, variableWorkspaces } from '../../variables';
import form from './SettingsForm.module.css';
import styles from './VariablesSettings.module.css';

type Draft = {
  id: string | null;
  revision: number | null;
  name: string;
  kind: VariableKind;
  value: string | boolean;
  replacing: boolean;
  workspaceIds: string[];
};
const empty = (): Draft => ({
  id: null,
  revision: null,
  name: '',
  kind: 'text',
  value: '',
  replacing: false,
  workspaceIds: [],
});
const fromField = (field: VariableMetadata): Draft => ({
  id: field.id,
  revision: field.revision,
  name: field.name,
  kind: field.kind,
  value: typeof field.value === 'number' ? String(field.value) : (field.value ?? ''),
  replacing: false,
  workspaceIds: field.workspaceIds,
});

export type VariablesHandle = { requestLeave: (next: () => void) => void };
const VariablesSettings = ({ ref }: { ref?: Ref<VariablesHandle> }) => {
  const [fields, setFields] = useState<VariableMetadata[]>([]);
  const [workspaces, setWorkspaces] = useState<{ id: string; title: string }[]>([]);
  const [draft, setDraftState] = useState<Draft | null>(null);
  const [query, setQuery] = useState('');
  const [error, setError] = useState('');
  const [status, setStatus] = useState('');
  const [warning, setWarning] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [selectedOnly, setSelectedOnly] = useState(false);
  const [pending, setPending] = useState<(() => void) | null>(null);
  const [section, setSection] = useState<'details' | 'workspaces'>('details');
  const [revealed, setRevealed] = useState<string | null>(null);
  const [revealing, setRevealing] = useState(false);
  const revealVersion = useRef(0);
  const setDraft = (next: Draft | null) => {
    if (
      next?.id !== draft?.id ||
      next?.revision !== draft?.revision ||
      next?.kind !== draft?.kind ||
      next?.replacing !== draft?.replacing
    ) {
      revealVersion.current += 1;
      setRevealed(null);
      setRevealing(false);
    }
    setDraftState(next);
  };
  useEffect(
    () => () => {
      revealVersion.current += 1;
    },
    []
  );
  const reveal = async () => {
    if (!draft?.id || draft.revision === null) return;
    const version = ++revealVersion.current;
    setRevealing(true);
    setError('');
    try {
      const value = await revealVariable(draft.id, draft.revision);
      if (version === revealVersion.current) setRevealed(value);
    } catch (e) {
      if (version === revealVersion.current) setError(String(e));
    } finally {
      if (version === revealVersion.current) setRevealing(false);
    }
  };
  const original = draft?.id ? fields.find((f) => f.id === draft.id) : null;
  const workspaceChoices = [
    ...workspaces,
    ...(draft?.workspaceIds ?? [])
      .filter((id) => !workspaces.some((w) => w.id === id))
      .map((id) => ({ id, title: 'Unavailable workspace' })),
  ];
  const dirty =
    draft !== null &&
    JSON.stringify(draft) !== JSON.stringify(original ? fromField(original) : empty());
  const requestLeave = (next: () => void) => {
    if (saving) return;
    if (dirty || warning) setPending(() => next);
    else next();
  };
  useImperativeHandle(ref, () => ({ requestLeave }));
  useEffect(() => {
    let cancelled = false;
    Promise.all([listVariables(), variableWorkspaces()])
      .then(([values, choices]) => {
        if (!cancelled) {
          setFields(values);
          setWorkspaces(choices);
        }
      })
      .catch((e) => {
        if (!cancelled) setError(String(e));
      });
    return () => {
      cancelled = true;
    };
  }, []);
  const save = async () => {
    if (!draft) return false;
    setStatus('');
    if (
      draft.kind === 'number' &&
      (String(draft.value).trim() === '' || !Number.isFinite(Number(draft.value)))
    ) {
      setError('Enter a finite number');
      return false;
    }
    setSaving(true);
    setError('');
    try {
      const outcome = await saveVariable({
        id: draft.id,
        expectedRevision: draft.revision,
        name: draft.name,
        value:
          draft.kind === 'boolean'
            ? { type: 'boolean', value: draft.value === true }
            : draft.kind === 'number'
              ? { type: 'number', value: String(draft.value) }
              : draft.kind === 'protected'
                ? { type: 'protected', replacement: draft.replacing ? String(draft.value) : null }
                : { type: 'text', value: String(draft.value) },
        workspaceIds: draft.workspaceIds,
      });
      const saved = outcome.definition;
      setFields((values) => [...values.filter((f) => f.id !== saved.id), saved]);
      setDraft(fromField(saved));
      setStatus(outcome.warning ? 'Saved with warning' : 'Saved');
      if (outcome.warning) setWarning(outcome.warning);
      // A committed warning must be acknowledged before pending navigation.
      return !outcome.warning && !warning;
    } catch (e) {
      setError(String(e));
      return false;
    } finally {
      setSaving(false);
    }
  };
  return (
    <div className={styles.page}>
      <section className={styles.fields} aria-label="Fields">
        <div className={styles.heading}>
          <h3>Fields</h3>
          <button
            type="button"
            disabled={saving}
            className={form.secondaryButton}
            onClick={() =>
              requestLeave(() => {
                setDraft(empty());
                setStatus('');
              })
            }
          >
            Add field
          </button>
        </div>
        <div className={styles.list}>
          {fields.map((field) => (
            <button
              key={field.id}
              type="button"
              disabled={saving}
              aria-pressed={draft?.id === field.id}
              onClick={() => {
                if (draft?.id === field.id) return;
                requestLeave(() => {
                  setDraft(fromField(field));
                  setStatus('');
                });
              }}
            >
              {field.name}
            </button>
          ))}
        </div>
      </section>
      <div className={styles.editor} data-leaving={pending !== null}>
        {draft ? (
          <>
            <div className={styles.sectionTabs} role="tablist" aria-label="Field sections">
              <button
                type="button"
                role="tab"
                aria-selected={section === 'details'}
                aria-controls="variable-details"
                id="variable-details-tab"
                onClick={() => setSection('details')}
              >
                Details
              </button>
              <button
                type="button"
                role="tab"
                aria-selected={section === 'workspaces'}
                aria-controls="variable-workspaces"
                id="variable-workspaces-tab"
                onClick={() => setSection('workspaces')}
              >
                Workspaces
              </button>
            </div>
            <fieldset className={styles.columns} data-section={section} disabled={saving}>
              <section className={styles.details} aria-label="Field details" id="variable-details">
                <label className={form.field}>
                  Name
                  <input
                    className={form.input}
                    value={draft.name}
                    onChange={(e) => setDraft({ ...draft, name: e.target.value })}
                  />
                </label>
                {original && original.name !== draft.name && (
                  <p className={styles.warning}>
                    Renaming changes the key; existing references are not rewritten.
                  </p>
                )}
                <label className={form.field}>
                  Type
                  <select
                    className={form.select}
                    value={draft.kind}
                    onChange={(e) =>
                      setDraft({
                        ...draft,
                        kind: e.target.value as VariableKind,
                        value: e.target.value === 'boolean' ? false : '',
                        replacing: e.target.value === 'protected',
                      })
                    }
                  >
                    <option value="text">Text</option>
                    <option value="protected">Protected string</option>
                    <option value="number">Number</option>
                    <option value="boolean">Boolean</option>
                  </select>
                </label>
                {draft.kind === 'boolean' ? (
                  <fieldset className={styles.radios}>
                    <legend>Value</legend>
                    <label>
                      <input
                        type="radio"
                        name="variable-boolean"
                        checked={draft.value === true}
                        onChange={() => setDraft({ ...draft, value: true })}
                      />
                      True
                    </label>
                    <label>
                      <input
                        type="radio"
                        name="variable-boolean"
                        checked={draft.value === false}
                        onChange={() => setDraft({ ...draft, value: false })}
                      />
                      False
                    </label>
                  </fieldset>
                ) : draft.kind === 'number' ? (
                  <label className={form.field}>
                    Value
                    <input
                      type="number"
                      step="any"
                      className={form.input}
                      value={String(draft.value)}
                      onChange={(e) => setDraft({ ...draft, value: e.target.value })}
                    />
                  </label>
                ) : draft.kind === 'protected' ? (
                  <div className={styles.protected}>
                    <label className={form.field}>
                      Value
                      <input
                        className={form.input}
                        type={revealed !== null && !draft.replacing ? 'text' : 'password'}
                        readOnly={!draft.replacing}
                        autoComplete="off"
                        value={draft.replacing ? String(draft.value) : (revealed ?? '••••••••')}
                        onChange={(e) => setDraft({ ...draft, value: e.target.value })}
                      />
                    </label>
                    {!draft.replacing && (
                      <div className={styles.heading}>
                        <button
                          type="button"
                          className={form.secondaryButton}
                          disabled={revealing}
                          onClick={() => {
                            if (revealed !== null) setRevealed(null);
                            else void reveal();
                          }}
                        >
                          {revealing ? 'Revealing…' : revealed !== null ? 'Hide' : 'Reveal'}
                        </button>
                        <button
                          type="button"
                          className={form.secondaryButton}
                          onClick={() => setDraft({ ...draft, value: '', replacing: true })}
                        >
                          Replace value
                        </button>
                      </div>
                    )}
                    <p className={styles.warning}>
                      {draft.replacing
                        ? 'Save replaces the protected value.'
                        : 'Stored in native protected storage. Grant changes keep the saved value.'}
                    </p>
                  </div>
                ) : (
                  <label className={form.field}>
                    Value
                    <textarea
                      className={form.textarea}
                      value={String(draft.value)}
                      onChange={(e) => setDraft({ ...draft, value: e.target.value })}
                    />
                  </label>
                )}
              </section>
              <section
                className={styles.workspaces}
                aria-label="Workspace access"
                id="variable-workspaces"
              >
                <h3>Workspaces</h3>
                <label className={form.field}>
                  Search workspaces
                  <input
                    className={form.input}
                    value={query}
                    onChange={(e) => setQuery(e.target.value)}
                  />
                </label>
                <div className={styles.heading}>
                  <span>{draft.workspaceIds.length} selected</span>
                  <label>
                    <input
                      type="checkbox"
                      checked={selectedOnly}
                      onChange={(e) => setSelectedOnly(e.target.checked)}
                    />
                    Selected only
                  </label>
                </div>
                <div className={styles.list}>
                  {workspaceChoices
                    .filter(
                      (w) =>
                        (!selectedOnly || draft.workspaceIds.includes(w.id)) &&
                        w.title.toLowerCase().includes(query.toLowerCase())
                    )
                    .map((w) => (
                      <label key={w.id}>
                        <input
                          type="checkbox"
                          checked={draft.workspaceIds.includes(w.id)}
                          onChange={(e) =>
                            setDraft({
                              ...draft,
                              workspaceIds: e.target.checked
                                ? [...draft.workspaceIds, w.id]
                                : draft.workspaceIds.filter((id) => id !== w.id),
                            })
                          }
                        />
                        {w.title}
                      </label>
                    ))}
                </div>
              </section>
            </fieldset>
            <footer className={styles.footer}>
              {error && <span role="alert">{error}</span>}
              {warning && <span role="alert">{warning}</span>}
              <span role="status">{status}</span>
              {pending && warning ? (
                <>
                  <button
                    type="button"
                    className={form.secondaryButton}
                    disabled={saving}
                    onClick={() => setPending(null)}
                  >
                    Keep editing
                  </button>
                  <button
                    type="button"
                    className={form.primaryButton}
                    disabled={saving}
                    onClick={() => {
                      const next = pending;
                      setPending(null);
                      setWarning(null);
                      next();
                    }}
                  >
                    Acknowledge and continue
                  </button>
                </>
              ) : pending ? (
                <>
                  <span>Save changes before leaving?</span>
                  <button
                    type="button"
                    className={form.secondaryButton}
                    autoFocus
                    disabled={saving}
                    onClick={() => setPending(null)}
                  >
                    Keep editing
                  </button>
                  <button
                    type="button"
                    className={form.secondaryButton}
                    disabled={saving}
                    onClick={() => {
                      const next = pending;
                      setPending(null);
                      next();
                    }}
                  >
                    Discard
                  </button>
                  <button
                    type="button"
                    className={form.primaryButton}
                    disabled={saving}
                    onClick={() => {
                      void save().then((ok) => {
                        if (ok) {
                          const next = pending;
                          setPending(null);
                          next();
                        }
                      });
                    }}
                  >
                    Save
                  </button>
                </>
              ) : (
                <button
                  type="button"
                  className={form.primaryButton}
                  disabled={saving}
                  onClick={() => void save()}
                >
                  {saving ? 'Saving…' : 'Save'}
                </button>
              )}
              {!pending && warning && (
                <button
                  type="button"
                  className={form.secondaryButton}
                  onClick={() => setWarning(null)}
                >
                  Acknowledge
                </button>
              )}
            </footer>
          </>
        ) : (
          <p>Select a field or add one to share a static value with workspaces.</p>
        )}
        {!draft && error && <p role="alert">{error}</p>}
      </div>
    </div>
  );
};
export default VariablesSettings;
