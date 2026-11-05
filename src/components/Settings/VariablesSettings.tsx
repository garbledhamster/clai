import { useEffect, useImperativeHandle, useState, type Ref } from 'react';
import type { VariableMetadata } from '../../generated/bindings';
import { listVariables, saveVariable, variableWorkspaces } from '../../variables';
import form from './SettingsForm.module.css';
import styles from './VariablesSettings.module.css';

type Draft = {
  id: string | null;
  revision: number | null;
  name: string;
  value: string;
  workspaceIds: string[];
};
const empty = (): Draft => ({ id: null, revision: null, name: '', value: '', workspaceIds: [] });
const fromField = (field: VariableMetadata): Draft => ({
  id: field.id,
  revision: field.revision,
  name: field.name,
  value: field.value,
  workspaceIds: field.workspaceIds,
});

export type VariablesHandle = { requestLeave: (next: () => void) => void };
const VariablesSettings = ({ ref }: { ref?: Ref<VariablesHandle> }) => {
  const [fields, setFields] = useState<VariableMetadata[]>([]);
  const [workspaces, setWorkspaces] = useState<{ id: string; title: string }[]>([]);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [query, setQuery] = useState('');
  const [error, setError] = useState('');
  const [status, setStatus] = useState('');
  const [saving, setSaving] = useState(false);
  const [selectedOnly, setSelectedOnly] = useState(false);
  const [pending, setPending] = useState<(() => void) | null>(null);
  const [section, setSection] = useState<'details' | 'workspaces'>('details');
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
    if (dirty) setPending(() => next);
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
    setSaving(true);
    setError('');
    setStatus('');
    try {
      const saved = await saveVariable({
        id: draft.id,
        expectedRevision: draft.revision,
        name: draft.name,
        value: draft.value,
        workspaceIds: draft.workspaceIds,
      });
      setFields((values) => [...values.filter((f) => f.id !== saved.id), saved]);
      setDraft(fromField(saved));
      setStatus('Saved');
      return true;
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
                  <select className={form.select} value="text" onChange={() => {}}>
                    <option value="text">Text</option>
                  </select>
                </label>
                <label className={form.field}>
                  Value
                  <textarea
                    className={form.textarea}
                    value={draft.value}
                    onChange={(e) => setDraft({ ...draft, value: e.target.value })}
                  />
                </label>
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
              <span role="status">{status}</span>
              {pending ? (
                <>
                  <span>Save changes before leaving?</span>
                  <button
                    type="button"
                    className={form.secondaryButton}
                    autoFocus
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
