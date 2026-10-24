// Refresh workflow editor: source and target pickers with env tags, the post-restore
// script from the library, and the options. Saving enforces the target environment-tag gate
// and the post-restore script checks; both are re-checked at run time.

import { useEffect, useState, type FormEvent } from "react";
import { useWorkspace, type Tab } from "../app/workspace";
import { useConnections } from "../connection/store";
import { TagPill } from "../connection/EnvTag";
import { ErrorNote } from "../connection/ErrorNote";
import * as w from "../lib/ipc/workflow";
import { libraryList, type ScriptInfo } from "../lib/ipc/scripts";
import { toAppError, type AppError } from "../lib/ipc/errors";
import { useWorkflows } from "./store";

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
    id: crypto.randomUUID(),
    name: "Refresh local from remote",
    source: { connection: "", database: "" },
    target: { connection: "", database: "" },
    post_restore_script: null,
    options: w.defaultOptions(),
    has_succeeded: false,
  };
}

export function WorkflowEditor({ tab }: { tab: Tab }) {
  const data = (tab.data ?? {}) as WorkflowEditorData;
  const existing = useWorkflows((s) => s.workflows.find((x) => x.id === data.workflowId));
  const entries = useConnections((s) => s.entries);
  const upsert = useWorkflows((s) => s.upsert);
  const closeTab = useWorkspace((s) => s.closeTab);
  const [wf, setWf] = useState<w.RefreshWorkflow>(() => existing ?? blank());
  const [scripts, setScripts] = useState<ScriptInfo[]>([]);
  const [check, setCheck] = useState<w.ScriptCheck | null>(null);
  const [error, setError] = useState<AppError | null>(null);
  const [excludeText, setExcludeText] = useState(() => (existing?.options.exclude_table_data ?? []).join("\n"));
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    void libraryList().then((l) => setScripts(l.scripts)).catch(() => {});
  }, []);

  useEffect(() => {
    if (!wf.post_restore_script) return setCheck(null);
    void w.workflowCheckScript(wf.post_restore_script, wf.options.script_mode).then(setCheck).catch((e) => setError(toAppError(e)));
  }, [wf.post_restore_script, wf.options.script_mode]);

  const opt = <K extends keyof w.RefreshOptions>(k: K, v: w.RefreshOptions[K]) => setWf((x) => ({ ...x, options: { ...x.options, [k]: v } }));
  const target = entries.find((e) => e.id === wf.target.connection);
  const source = entries.find((e) => e.id === wf.source.connection);

  const pickConn = (side: "source" | "target", id: string) => {
    const e = entries.find((x) => x.id === id);
    setWf((x) => ({ ...x, [side]: { connection: id, database: e?.database ?? "" } }));
  };

  const onSubmit = async (ev: FormEvent) => {
    ev.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const saved = await w.workflowSave({
        ...wf,
        options: { ...wf.options, exclude_table_data: excludeText.split(/\n|,/).map((s) => s.trim()).filter(Boolean) },
      });
      upsert(saved);
      closeTab(tab.id);
    } catch (e) {
      setError(toAppError(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <form className="form-page" aria-label="Workflow" onSubmit={onSubmit}>
      <h2>{existing ? `Edit ${existing.name}` : "New Refresh workflow"}</h2>
      <label>
        Name <input value={wf.name} onChange={(e) => setWf({ ...wf, name: e.target.value })} required />
      </label>
      <fieldset>
        <legend>Copy from (source)</legend>
        <div className="row">
          <label className="grow">
            Connection
            <select aria-label="Source connection" value={wf.source.connection} onChange={(e) => pickConn("source", e.target.value)} required>
              <option value="">choose…</option>
              {entries.map((e) => (
                <option key={e.id} value={e.id}>
                  {e.name} ({e.env_tag})
                </option>
              ))}
            </select>
          </label>
          <label className="grow">
            Database <input aria-label="Source database" value={wf.source.database} onChange={(e) => setWf({ ...wf, source: { ...wf.source, database: e.target.value } })} required />
          </label>
        </div>
        {source && <p className="note">{source.user}@{source.host}:{source.port}{source.tunnel ? ` via ssh ${source.tunnel.host}` : ""}</p>}
      </fieldset>
      <fieldset>
        <legend>Replace (target)</legend>
        <div className="row">
          <label className="grow">
            Connection
            <select aria-label="Target connection" value={wf.target.connection} onChange={(e) => pickConn("target", e.target.value)} required>
              <option value="">choose…</option>
              {entries.map((e) => {
                const allowed = e.env_tag === "local" || e.env_tag === "dev";
                return (
                  <option key={e.id} value={e.id} disabled={!allowed}>
                    {e.name} ({e.env_tag}){allowed ? "" : " — only Local or Dev can be replaced"}
                  </option>
                );
              })}
            </select>
          </label>
          <label className="grow">
            Database <input aria-label="Target database" value={wf.target.database} onChange={(e) => setWf({ ...wf, target: { ...wf.target, database: e.target.value } })} required />
          </label>
        </div>
        {target && (
          <p className="note">
            <TagPill tag={target.env_tag} /> {target.host}:{target.port} · maintenance database {target.maintenance_db}
          </p>
        )}
      </fieldset>
      <fieldset>
        <legend>After restore</legend>
        <label>
          Post-restore script
          <select aria-label="Post-restore script" value={wf.post_restore_script ?? ""} onChange={(e) => setWf({ ...wf, post_restore_script: e.target.value || null })}>
            <option value="">none</option>
            {scripts.filter((s) => /\.sql$/i.test(s.path)).map((s) => (
              <option key={s.id} value={s.id} disabled={s.missing}>
                {s.path}
              </option>
            ))}
          </select>
        </label>
        <label>
          Script mode
          <select aria-label="Script mode" value={wf.options.script_mode} onChange={(e) => opt("script_mode", e.target.value as w.ScriptMode)}>
            <option value="transaction">one transaction (rolled back on any error)</option>
            <option value="no_transaction">no transaction (for CONCURRENTLY / VACUUM)</option>
          </select>
        </label>
        {check && !check.ok && (
          <div className="warn-note" role="alert">
            <p>This script can't be attached:</p>
            <ul>{check.problems.map((p) => <li key={p}>{p}</li>)}</ul>
          </div>
        )}
        <label>
          Sanity check query (optional)
          <input value={wf.options.sanity_query ?? ""} placeholder="default: at least one user table exists" onChange={(e) => opt("sanity_query", e.target.value || null)} />
        </label>
      </fieldset>
      <fieldset>
        <legend>Options</legend>
        <label>
          Replace strategy
          <select aria-label="Replace strategy" value={wf.options.strategy} onChange={(e) => opt("strategy", e.target.value as w.ReplaceStrategy)}>
            <option value="staging_swap">restore into staging, then swap (a failure never touches your database)</option>
            <option value="drop_first">drop first (needs less disk; a failure leaves no local database)</option>
          </select>
        </label>
        <label>
          Skip data for tables (one per line, schema.table)
          <textarea rows={3} value={excludeText} onChange={(e) => setExcludeText(e.target.value)} placeholder="public.audit_log" />
        </label>
        <div className="row">
          <label className="narrow">
            Restore jobs <input type="number" min={1} max={16} value={wf.options.jobs ?? ""} placeholder="auto" onChange={(e) => opt("jobs", e.target.value ? Number(e.target.value) : null)} />
          </label>
          <label className="grow">
            On restore error
            <select value={wf.options.error_policy} onChange={(e) => opt("error_policy", e.target.value as w.RestoreErrorPolicy)}>
              <option value="stop_on_first">stop at the first error</option>
              <option value="continue_then_fail">restore what it can, then fail naming every error</option>
            </select>
          </label>
        </div>
        <div className="checks">
          <label className="check">
            <input type="checkbox" checked={wf.options.no_owner} onChange={(e) => opt("no_owner", e.target.checked)} /> skip ownership (--no-owner)
          </label>
          <label className="check">
            <input type="checkbox" checked={wf.options.no_privileges} onChange={(e) => opt("no_privileges", e.target.checked)} /> skip grants (--no-privileges)
          </label>
        </div>
      </fieldset>
      {error && <ErrorNote error={error} />}
      <div className="actions">
        <span className="spacer" />
        <button type="button" onClick={() => closeTab(tab.id)}>Cancel</button>
        <button type="submit" className="primary" disabled={busy || (!!check && !check.ok)}>Save</button>
      </div>
    </form>
  );
}
