// Workflows v2 editor: entry modes (remote DB, backup file, in-place), target picker
// (all environments; Staging/Production require typed DB-name confirmation at run time),
// typed parameters, WYSIWYG step builder with live SQL preview & Convert to SQL,
// starter templates, and shareable TOML import/export.

import { useCallback, useEffect, useMemo, useState, type FormEvent } from "react";
import { useWorkspace, type Tab } from "../app/workspace";
import { useConnections } from "../connection/store";
import { TagPill } from "../connection/EnvTag";
import { ErrorNote } from "../connection/ErrorNote";
import * as w from "../lib/ipc/workflow";
import { pickRestoreArchive } from "../lib/ipc/restore";
import { libraryList, type ScriptInfo } from "../lib/ipc/scripts";
import { toAppError, type AppError } from "../lib/ipc/errors";
import { useWorkflows } from "./store";
import { useCatalogHints } from "./useCatalogHints";
import { ParamsEditor } from "./ParamsEditor";
import { StepCard } from "./StepCard";

export interface WorkflowEditorData {
  workflowId?: string;
}

export function openWorkflowEditor(workflowId?: string, name?: string) {
  useWorkspace.getState().openTab({
    id: workflowId ? `workflow-editor:${workflowId}` : `workflow-editor:new:${crypto.randomUUID()}`,
    kind: "workflow-editor",
    title: name ? `Edit ${name}` : "New workflow",
    data: { workflowId } satisfies WorkflowEditorData,
  });
}

function blank(): w.RefreshWorkflow {
  return {
    schema_version: w.WORKFLOW_SCHEMA_VERSION,
    id: crypto.randomUUID(),
    name: "Refresh local from remote",
    source: { connection: "", database: "" },
    source_mode: { mode: "from_remote_db", source: { connection: "", database: "" } },
    target: { connection: "", database: "" },
    post_restore_script: null,
    parameters: [],
    steps: [],
    options: w.defaultOptions(),
    has_succeeded: false,
  };
}

function normalizeForEdit(wf: w.RefreshWorkflow): w.RefreshWorkflow {
  return {
    ...wf,
    source_mode: w.effectiveSource(wf),
    parameters: wf.parameters ?? [],
    steps: wf.steps ?? [],
    options: { ...w.defaultOptions(), ...wf.options },
  };
}

export function WorkflowEditor({ tab }: { tab: Tab }) {
  const data = (tab.data ?? {}) as WorkflowEditorData;
  const existing = useWorkflows((s) => s.workflows.find((x) => x.id === data.workflowId));
  const entries = useConnections((s) => s.entries);
  const upsert = useWorkflows((s) => s.upsert);
  const closeTab = useWorkspace((s) => s.closeTab);

  const [wf, setWf] = useState<w.RefreshWorkflow>(() => (existing ? normalizeForEdit(existing) : blank()));
  const [scripts, setScripts] = useState<ScriptInfo[]>([]);
  const [templates, setTemplates] = useState<w.WorkflowTemplate[]>([]);
  const [check, setCheck] = useState<w.ScriptCheck | null>(null);
  const [compiled, setCompiled] = useState<w.CompiledStep[]>([]);
  const [previewError, setPreviewError] = useState<string | null>(null);
  const [previewTick, setPreviewTick] = useState(0);
  const [error, setError] = useState<AppError | null>(null);
  const [excludeText, setExcludeText] = useState(() => (existing?.options.exclude_table_data ?? []).join("\n"));
  const [busy, setBusy] = useState(false);

  // Import / Export TOML modal state
  const [importOpen, setImportOpen] = useState(false);
  const [importText, setImportText] = useState("");
  const [importError, setImportError] = useState<AppError | null>(null);
  const [exportToml, setExportToml] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);

  const catalog = useCatalogHints(wf);
  const sourceMode = w.effectiveSource(wf);
  const steps = wf.steps ?? [];
  const params = wf.parameters ?? [];

  useEffect(() => {
    void libraryList().then((l) => setScripts(l?.scripts ?? [])).catch(() => {});
  }, []);

  useEffect(() => {
    if (existing) return;
    void w.workflowTemplates().then((t) => setTemplates(Array.isArray(t) ? t : [])).catch(() => {});
  }, [existing]);

  // Legacy v1 post-restore script static check (only when no v2 steps are defined)
  useEffect(() => {
    if (steps.length > 0 || !wf.post_restore_script) {
      setCheck(null);
      return;
    }
    void w.workflowCheckScript(wf.post_restore_script, wf.options.script_mode).then(setCheck).catch((e) => setError(toAppError(e)));
  }, [steps.length, wf.post_restore_script, wf.options.script_mode]);

  // Live debounced SQL preview compilation for v2 steps
  useEffect(() => {
    if (steps.length === 0) {
      setCompiled([]);
      setPreviewError(null);
      return;
    }
    let cancelled = false;
    const timer = setTimeout(() => {
      void w
        .workflowPreviewSql(wf)
        .then((res) => {
          if (!cancelled) {
            setCompiled(Array.isArray(res) ? res : []);
            setPreviewError(null);
          }
        })
        .catch((e) => {
          if (!cancelled) {
            setCompiled([]);
            setPreviewError(toAppError(e).message);
          }
        });
    }, 180);
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, [wf, steps.length, previewTick]);

  const compiledByStep = useMemo(() => {
    const map = new Map<string, w.CompiledStep>();
    for (const c of compiled) map.set(c.step_id, c);
    return map;
  }, [compiled]);

  const hasStepProblems = useMemo(() => compiled.some((c) => c.problems.length > 0), [compiled]);

  const opt = <K extends keyof w.RefreshOptions>(k: K, v: w.RefreshOptions[K]) =>
    setWf((x) => ({ ...x, options: { ...x.options, [k]: v } }));

  const target = entries.find((e) => e.id === wf.target.connection);
  const remoteSource = sourceMode.mode === "from_remote_db" ? entries.find((e) => e.id === sourceMode.source.connection) : undefined;
  const targetIsRestricted = target?.env_tag === "staging" || target?.env_tag === "production";

  const setSourceMode = (next: w.WorkflowSource) => {
    setWf((x) => ({
      ...x,
      source_mode: next,
      source: next.mode === "from_remote_db" ? next.source : x.source,
    }));
  };

  const switchEntryMode = (mode: w.WorkflowSource["mode"]) => {
    if (mode === sourceMode.mode) return;
    if (mode === "from_remote_db") {
      setSourceMode({ mode: "from_remote_db", source: wf.source });
    } else if (mode === "from_backup_file") {
      setSourceMode({ mode: "from_backup_file", archive: { kind: "prompt_at_run", param: null } });
    } else {
      setSourceMode({ mode: "in_place_target", pre_run_backup: true });
    }
  };

  const pickRemoteSource = (id: string) => {
    const e = entries.find((x) => x.id === id);
    const nextRef: w.DbRef = { connection: id, database: e?.database ?? "" };
    setSourceMode({ mode: "from_remote_db", source: nextRef });
  };

  const pickTarget = (id: string) => {
    const e = entries.find((x) => x.id === id);
    setWf((x) => ({ ...x, target: { connection: id, database: e?.database ?? "" } }));
  };

  const applyTemplate = (tpl: w.WorkflowTemplate) => {
    setError(null);
    const loaded = normalizeForEdit(tpl.workflow);
    const keepTarget = wf.target.connection ? wf.target : loaded.target;
    setWf({
      ...loaded,
      id: existing ? wf.id : crypto.randomUUID(),
      target: keepTarget,
    });
    setExcludeText((loaded.options.exclude_table_data ?? []).join("\n"));
  };

  const setSteps = (nextSteps: w.WorkflowStep[]) => {
    setWf((x) => ({
      ...x,
      schema_version: nextSteps.length > 0 ? w.WORKFLOW_SCHEMA_VERSION : x.schema_version,
      steps: nextSteps,
      // When switching to v2 pipeline steps, clear legacy single script field so both don't collide.
      post_restore_script: nextSteps.length > 0 ? null : x.post_restore_script,
    }));
  };

  const addStep = (kind: w.WorkflowStep["kind"]) => {
    const fresh = w.createBlankStep(kind);
    // If migrating from a legacy v1 post_restore_script, carry it in as a linked SQL step first.
    if (steps.length === 0 && wf.post_restore_script) {
      const migrated: w.SqlStep = {
        id: crypto.randomUUID(),
        name: "Post-restore script",
        enabled: true,
        no_transaction: wf.options.script_mode === "no_transaction",
        kind: "sql",
        source: "library",
        script_id: wf.post_restore_script,
      };
      setSteps([migrated, fresh]);
      return;
    }
    setSteps([...steps, fresh]);
  };

  const updateStep = (index: number, next: w.WorkflowStep) => {
    setSteps(steps.map((s, i) => (i === index ? next : s)));
  };

  const moveStep = (index: number, delta: -1 | 1) => {
    const targetIdx = index + delta;
    if (targetIdx < 0 || targetIdx >= steps.length) return;
    const copy = [...steps];
    const [item] = copy.splice(index, 1);
    copy.splice(targetIdx, 0, item);
    setSteps(copy);
  };

  const duplicateStep = (index: number) => {
    const src = steps[index];
    const clone: w.WorkflowStep = { ...structuredClone(src), id: crypto.randomUUID(), name: `${src.name} (copy)` };
    const copy = [...steps];
    copy.splice(index + 1, 0, clone);
    setSteps(copy);
  };

  const removeStep = (index: number) => {
    setSteps(steps.filter((_, i) => i !== index));
  };

  const convertStepToSql = useCallback(
    async (index: number, stepId: string) => {
      setError(null);
      try {
        const converted = await w.workflowConvertStepToSql(wf, stepId);
        setWf((x) => ({
          ...x,
          steps: (x.steps ?? []).map((s, i) => (i === index ? converted : s)),
        }));
      } catch (e) {
        setError(toAppError(e));
      }
    },
    [wf],
  );

  const handleBrowseArchive = async () => {
    try {
      const picked = await pickRestoreArchive();
      if (picked) {
        setSourceMode({ mode: "from_backup_file", archive: { kind: "fixed_path", path: picked } });
      }
    } catch (e) {
      setError(toAppError(e));
    }
  };

  const handleExportToml = async () => {
    if (!existing) return;
    setError(null);
    setCopied(false);
    try {
      const toml = await w.workflowExportToml(existing.id);
      setExportToml(toml);
    } catch (e) {
      setError(toAppError(e));
    }
  };

  const handleImportToml = async () => {
    setImportError(null);
    try {
      const imported = await w.workflowImportToml(importText);
      const loaded = normalizeForEdit(imported);
      setWf({ ...loaded, id: existing ? wf.id : loaded.id });
      setExcludeText((loaded.options.exclude_table_data ?? []).join("\n"));
      setImportOpen(false);
      setImportText("");
    } catch (e) {
      setImportError(toAppError(e));
    }
  };

  const onSubmit = async (ev: FormEvent) => {
    ev.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const sanitizedParams = params.map((p) => (p.kind === "secret" ? { ...p, default: null } : p));
      const isV2 =
        steps.length > 0 ||
        sanitizedParams.length > 0 ||
        sourceMode.mode !== "from_remote_db" ||
        !!wf.options.pause_before_swap;

      const payload: w.RefreshWorkflow = {
        ...wf,
        schema_version: isV2 ? w.WORKFLOW_SCHEMA_VERSION : 1,
        source: sourceMode.mode === "from_remote_db" ? sourceMode.source : wf.source,
        source_mode: sourceMode,
        parameters: sanitizedParams,
        steps,
        options: {
          ...wf.options,
          exclude_table_data: excludeText
            .split(/\n|,/)
            .map((s) => s.trim())
            .filter(Boolean),
        },
      };

      const saved = await w.workflowSave(payload);
      upsert(saved);
      closeTab(tab.id);
    } catch (e) {
      setError(toAppError(e));
    } finally {
      setBusy(false);
    }
  };

  const saveDisabled = busy || (steps.length === 0 && !!check && !check.ok) || hasStepProblems;

  return (
    <form className="form-page wf-editor" aria-label="Workflow" onSubmit={onSubmit}>
      <div className="wf-editor-header">
        <h2>{existing ? `Edit ${existing.name}` : "New workflow"}</h2>
        <div className="wf-editor-tools">
          <button type="button" onClick={() => { setImportError(null); setImportOpen(true); }}>
            Import TOML
          </button>
          {existing && (
            <button type="button" onClick={() => void handleExportToml()}>
              Export TOML
            </button>
          )}
        </div>
      </div>

      {!existing && templates.length > 0 && (
        <section className="wf-template-bar" aria-label="Starter templates">
          <div className="wf-template-label muted">Start from a template:</div>
          <div className="wf-template-cards">
            {templates.map((tpl) => (
              <button
                key={tpl.id}
                type="button"
                className="wf-template-card"
                onClick={() => applyTemplate(tpl)}
              >
                <span className="wf-template-title">{tpl.name}</span>
                <span className="muted wf-template-desc">{tpl.description}</span>
              </button>
            ))}
          </div>
        </section>
      )}

      <label>
        Name <input value={wf.name} onChange={(e) => setWf({ ...wf, name: e.target.value })} required />
      </label>

      <fieldset>
        <legend>Source / Entry mode</legend>
        <div className="wf-mode-tabs" role="radiogroup" aria-label="Entry mode">
          <label className="wf-mode-tab" data-active={sourceMode.mode === "from_remote_db"}>
            <input
              type="radio"
              name="entry-mode"
              checked={sourceMode.mode === "from_remote_db"}
              onChange={() => switchEntryMode("from_remote_db")}
            />
            Copy from remote DB
          </label>
          <label className="wf-mode-tab" data-active={sourceMode.mode === "from_backup_file"}>
            <input
              type="radio"
              name="entry-mode"
              checked={sourceMode.mode === "from_backup_file"}
              onChange={() => switchEntryMode("from_backup_file")}
            />
            Restore from backup file
          </label>
          <label className="wf-mode-tab" data-active={sourceMode.mode === "in_place_target"}>
            <input
              type="radio"
              name="entry-mode"
              checked={sourceMode.mode === "in_place_target"}
              onChange={() => switchEntryMode("in_place_target")}
            />
            Run in-place on target
          </label>
        </div>

        {sourceMode.mode === "from_remote_db" && (
          <>
            <div className="row">
              <label className="grow">
                Connection
                <select
                  aria-label="Source connection"
                  value={sourceMode.source.connection}
                  onChange={(e) => pickRemoteSource(e.target.value)}
                  required
                >
                  <option value="">choose…</option>
                  {entries.map((e) => (
                    <option key={e.id} value={e.id}>
                      {e.name} ({e.env_tag})
                    </option>
                  ))}
                </select>
              </label>
              <label className="grow">
                Database
                <input
                  aria-label="Source database"
                  value={sourceMode.source.database}
                  onChange={(e) =>
                    setSourceMode({
                      mode: "from_remote_db",
                      source: { ...sourceMode.source, database: e.target.value },
                    })
                  }
                  required
                />
              </label>
            </div>
            {remoteSource && (
              <p className="note">
                {remoteSource.user}@{remoteSource.host}:{remoteSource.port}
                {remoteSource.tunnel ? ` via ssh ${remoteSource.tunnel.host}` : ""}
              </p>
            )}
          </>
        )}

        {sourceMode.mode === "from_backup_file" && (
          <div className="wf-backup-source">
            <div className="wf-inline-radios" role="radiogroup" aria-label="Backup archive source">
              <label className="check">
                <input
                  type="radio"
                  name="backup-src"
                  checked={sourceMode.archive.kind === "prompt_at_run"}
                  onChange={() =>
                    setSourceMode({ mode: "from_backup_file", archive: { kind: "prompt_at_run", param: null } })
                  }
                />
                Choose file at run time
              </label>
              <label className="check">
                <input
                  type="radio"
                  name="backup-src"
                  checked={sourceMode.archive.kind === "fixed_path"}
                  onChange={() =>
                    setSourceMode({ mode: "from_backup_file", archive: { kind: "fixed_path", path: "" } })
                  }
                />
                Fixed file path
              </label>
              <label className="check">
                <input
                  type="radio"
                  name="backup-src"
                  checked={sourceMode.archive.kind === "latest_from_history"}
                  onChange={() =>
                    setSourceMode({
                      mode: "from_backup_file",
                      archive: {
                        kind: "latest_from_history",
                        connection: wf.source.connection || entries[0]?.id || "",
                        database: wf.source.database || entries[0]?.database || "",
                      },
                    })
                  }
                />
                Latest backup from History
              </label>
            </div>

            {sourceMode.archive.kind === "fixed_path" && (
              <label>
                Backup archive (.dump, directory, or .sql)
                <span className="row">
                  <input
                    aria-label="Backup archive path"
                    className="mono grow"
                    value={sourceMode.archive.path}
                    placeholder="/path/to/backup.dump"
                    onChange={(e) =>
                      setSourceMode({
                        mode: "from_backup_file",
                        archive: { kind: "fixed_path", path: e.target.value },
                      })
                    }
                    required
                  />
                  <button type="button" onClick={() => void handleBrowseArchive()}>
                    Browse…
                  </button>
                </span>
              </label>
            )}

            {sourceMode.archive.kind === "prompt_at_run" && (
              <p className="note">
                You will be prompted to pick a Custom (<code>.dump</code>), Directory, or Plain SQL (<code>.sql</code>) archive before preflight runs.
              </p>
            )}

            {sourceMode.archive.kind === "latest_from_history" && (
              <div className="row">
                <label className="grow">
                  Backup of connection
                  <select
                    aria-label="History backup connection"
                    value={sourceMode.archive.connection}
                    onChange={(e) => {
                      const picked = entries.find((x) => x.id === e.target.value);
                      setSourceMode({
                        mode: "from_backup_file",
                        archive: {
                          kind: "latest_from_history",
                          connection: e.target.value,
                          database: picked?.database ?? "",
                        },
                      });
                    }}
                    required
                  >
                    <option value="">choose…</option>
                    {entries.map((e) => (
                      <option key={e.id} value={e.id}>
                        {e.name} ({e.env_tag})
                      </option>
                    ))}
                  </select>
                </label>
                <label className="grow">
                  Backup of database
                  <input
                    aria-label="History backup database"
                    value={sourceMode.archive.database}
                    onChange={(e) =>
                      setSourceMode({
                        mode: "from_backup_file",
                        archive: {
                          kind: "latest_from_history",
                          connection:
                            sourceMode.archive.kind === "latest_from_history" ? sourceMode.archive.connection : "",
                          database: e.target.value,
                        },
                      })
                    }
                    required
                  />
                </label>
              </div>
            )}
          </div>
        )}

        {sourceMode.mode === "in_place_target" && (
          <div className="wf-inplace-note">
            <label className="check">
              <input
                type="checkbox"
                checked={sourceMode.pre_run_backup}
                onChange={(e) =>
                  setSourceMode({ mode: "in_place_target", pre_run_backup: e.target.checked })
                }
              />
              Take a safety backup of the target database before running steps
            </label>
            <p className="note">
              Runs transform and verify steps directly on the target inside a single transaction. You can also choose <b>Dry run (roll back)</b> at run time.
            </p>
          </div>
        )}
      </fieldset>

      <fieldset>
        <legend>{sourceMode.mode === "in_place_target" ? "Target database" : "Replace (target)"}</legend>
        <div className="row">
          <label className="grow">
            Connection
            <select
              aria-label="Target connection"
              value={wf.target.connection}
              onChange={(e) => pickTarget(e.target.value)}
              required
            >
              <option value="">choose…</option>
              {entries.map((e) => (
                <option key={e.id} value={e.id}>
                  {e.name} ({e.env_tag})
                </option>
              ))}
            </select>
          </label>
          <label className="grow">
            Database
            <input
              aria-label="Target database"
              value={wf.target.database}
              onChange={(e) => setWf({ ...wf, target: { ...wf.target, database: e.target.value } })}
              required
            />
          </label>
        </div>
        {target && (
          <p className="note">
            <TagPill tag={target.env_tag} /> {target.host}:{target.port} · maintenance database {target.maintenance_db}
          </p>
        )}
        {targetIsRestricted && (
          <div className="warn-note" role="status">
            <strong>{target.env_tag === "production" ? "Production" : "Staging"} target:</strong> every run requires typing the exact database name (<span className="mono">{wf.target.database || "database"}</span>) to confirm. "Don't ask again" is permanently disabled for {target.env_tag} targets.
          </div>
        )}
      </fieldset>

      <fieldset>
        <legend>Parameters ({params.length})</legend>
        <ParamsEditor
          params={params}
          onChange={(nextParams) =>
            setWf((x) => ({
              ...x,
              schema_version: nextParams.length > 0 ? w.WORKFLOW_SCHEMA_VERSION : x.schema_version,
              parameters: nextParams,
            }))
          }
        />
      </fieldset>

      <fieldset>
        <legend>
          Pipeline steps ({steps.length})
          {catalog.sourceLabel && (
            <span className="wf-catalog-badge">
              Schema hints: {catalog.sourceLabel} ({catalog.tables.length} tables)
            </span>
          )}
        </legend>

        <div className="wf-add-step-bar" role="toolbar" aria-label="Add pipeline step">
          <span className="muted">Add step:</span>
          <button type="button" onClick={() => addStep("delete_rows")}>
            + Delete rows
          </button>
          <button type="button" onClick={() => addStep("insert_rows")}>
            + Insert rows
          </button>
          <button type="button" onClick={() => addStep("update_mask")}>
            + Update / Mask
          </button>
          <button type="button" onClick={() => addStep("sql")}>
            + SQL
          </button>
          <button type="button" onClick={() => addStep("verify")}>
            + Verify
          </button>
        </div>

        {steps.length > 0 ? (
          <div className="wf-step-list">
            {steps.map((step, idx) => (
              <StepCard
                key={step.id}
                step={step}
                index={idx}
                total={steps.length}
                catalog={catalog}
                params={params}
                scripts={scripts}
                compiled={compiledByStep.get(step.id)}
                previewError={previewError}
                onChange={(next) => updateStep(idx, next)}
                onMove={(delta) => moveStep(idx, delta)}
                onDuplicate={() => duplicateStep(idx)}
                onRemove={() => removeStep(idx)}
                onConvertToSql={step.kind !== "sql" ? () => convertStepToSql(idx, step.id) : undefined}
                onRequestPreview={() => setPreviewTick((t) => t + 1)}
              />
            ))}
          </div>
        ) : (
          <div className="wf-legacy-after">
            <p className="muted wf-empty-hint">
              Add visual steps above (delete users, insert test users, mask PII, SQL, verify), or attach a single post-restore library script below.
            </p>
            <label>
              Post-restore script
              <select
                aria-label="Post-restore script"
                value={wf.post_restore_script ?? ""}
                onChange={(e) => setWf({ ...wf, post_restore_script: e.target.value || null })}
              >
                <option value="">none</option>
                {scripts
                  .filter((s) => /\.sql$/i.test(s.path))
                  .map((s) => (
                    <option key={s.id} value={s.id} disabled={s.missing}>
                      {s.path}
                    </option>
                  ))}
              </select>
            </label>
            <label>
              Script mode
              <select
                aria-label="Script mode"
                value={wf.options.script_mode}
                onChange={(e) => opt("script_mode", e.target.value as w.ScriptMode)}
              >
                <option value="transaction">one transaction (rolled back on any error)</option>
                <option value="no_transaction">no transaction (for CONCURRENTLY / VACUUM)</option>
              </select>
            </label>
            {check && !check.ok && (
              <div className="warn-note" role="alert">
                <p>This script can't be attached:</p>
                <ul>
                  {check.problems.map((p) => (
                    <li key={p}>{p}</li>
                  ))}
                </ul>
              </div>
            )}
            <label>
              Sanity check query (optional)
              <input
                value={wf.options.sanity_query ?? ""}
                placeholder="default: at least one user table exists"
                onChange={(e) => opt("sanity_query", e.target.value || null)}
              />
            </label>
          </div>
        )}
      </fieldset>

      {sourceMode.mode !== "in_place_target" && (
        <fieldset>
          <legend>Restore &amp; swap options</legend>
          <label>
            Replace strategy
            <select
              aria-label="Replace strategy"
              value={wf.options.strategy}
              onChange={(e) => opt("strategy", e.target.value as w.ReplaceStrategy)}
            >
              <option value="staging_swap">
                restore into staging, then swap (a failure never touches your database)
              </option>
              <option value="drop_first">
                drop first (needs less disk; a failure leaves no local database)
              </option>
            </select>
          </label>
          {wf.options.strategy === "staging_swap" && (
            <label className="check">
              <input
                type="checkbox"
                checked={!!wf.options.pause_before_swap}
                onChange={(e) => opt("pause_before_swap", e.target.checked)}
              />
              Pause before swap so I can inspect the staging database first
            </label>
          )}
          <label>
            Skip data for tables (one per line, schema.table)
            <textarea
              rows={3}
              value={excludeText}
              onChange={(e) => setExcludeText(e.target.value)}
              placeholder="public.audit_log"
            />
          </label>
          <div className="row">
            <label className="narrow">
              Restore jobs
              <input
                type="number"
                min={1}
                max={16}
                value={wf.options.jobs ?? ""}
                placeholder="auto"
                onChange={(e) => opt("jobs", e.target.value ? Number(e.target.value) : null)}
              />
            </label>
            <label className="grow">
              On restore error
              <select
                value={wf.options.error_policy}
                onChange={(e) => opt("error_policy", e.target.value as w.RestoreErrorPolicy)}
              >
                <option value="stop_on_first">stop at the first error</option>
                <option value="continue_then_fail">restore what it can, then fail naming every error</option>
              </select>
            </label>
          </div>
          <div className="checks">
            <label className="check">
              <input
                type="checkbox"
                checked={wf.options.no_owner}
                onChange={(e) => opt("no_owner", e.target.checked)}
              />
              skip ownership (--no-owner)
            </label>
            <label className="check">
              <input
                type="checkbox"
                checked={wf.options.no_privileges}
                onChange={(e) => opt("no_privileges", e.target.checked)}
              />
              skip grants (--no-privileges)
            </label>
          </div>
        </fieldset>
      )}

      {error && <ErrorNote error={error} />}

      <div className="actions">
        {existing && (
          <DeleteWorkflowButton
            busy={busy}
            onDelete={async () => {
              setBusy(true);
              setError(null);
              try {
                await w.workflowDelete(existing.id);
                useWorkflows.getState().remove(existing.id);
                closeTab(tab.id);
              } catch (e) {
                setError(toAppError(e));
              } finally {
                setBusy(false);
              }
            }}
          />
        )}
        <span className="spacer" />
        <button type="button" onClick={() => closeTab(tab.id)}>
          Cancel
        </button>
        <button type="submit" className="primary" disabled={saveDisabled}>
          Save
        </button>
      </div>

      {importOpen && (
        <div className="modal-backdrop" role="presentation" onClick={() => setImportOpen(false)}>
          <div
            className="modal"
            role="dialog"
            aria-modal="true"
            aria-label="Import workflow TOML"
            onClick={(e) => e.stopPropagation()}
          >
            <h2>Import workflow TOML</h2>
            <label>
              Paste exported workflow TOML
              <textarea
                aria-label="Workflow TOML input"
                className="mono"
                rows={10}
                value={importText}
                placeholder="schema_version = 2..."
                onChange={(e) => setImportText(e.target.value)}
              />
            </label>
            {importError && <ErrorNote error={importError} compact />}
            <div className="modal-actions">
              <button type="button" onClick={() => setImportOpen(false)}>
                Cancel
              </button>
              <button
                type="button"
                className="primary"
                disabled={!importText.trim()}
                onClick={() => void handleImportToml()}
              >
                Load workflow
              </button>
            </div>
          </div>
        </div>
      )}

      {exportToml !== null && (
        <div className="modal-backdrop" role="presentation" onClick={() => setExportToml(null)}>
          <div
            className="modal"
            role="dialog"
            aria-modal="true"
            aria-label="Export workflow TOML"
            onClick={(e) => e.stopPropagation()}
          >
            <h2>Export workflow TOML</h2>
            <p className="muted">Secrets and local confirmations have been stripped. Safe to commit or share.</p>
            <textarea
              aria-label="Exported workflow TOML"
              className="mono"
              rows={12}
              readOnly
              value={exportToml}
            />
            <div className="modal-actions">
              <button
                type="button"
                onClick={() => {
                  void navigator.clipboard?.writeText(exportToml).then(() => setCopied(true)).catch(() => {});
                }}
              >
                {copied ? "Copied!" : "Copy TOML"}
              </button>
              <button type="button" className="primary" onClick={() => setExportToml(null)}>
                Close
              </button>
            </div>
          </div>
        </div>
      )}
    </form>
  );
}

function DeleteWorkflowButton({ busy, onDelete }: { busy: boolean; onDelete: () => Promise<void> }) {
  const [confirm, setConfirm] = useState(false);
  if (confirm) {
    return (
      <>
        <button type="button" className="danger" disabled={busy} onClick={() => void onDelete()}>
          Confirm delete
        </button>
        <button type="button" disabled={busy} onClick={() => setConfirm(false)}>
          Keep
        </button>
      </>
    );
  }
  return (
    <button type="button" className="danger" disabled={busy} onClick={() => setConfirm(true)}>
      Delete
    </button>
  );
}
