// The Refresh & Workflows v2 run view: optional archive/parameter inputs, preflight checks
// and ONE confirmation up front (naming exact target, server, strategy, and requiring exact
// typed DB-name confirmation on Staging/Production), then a docked, non-blocking live view
// with per-step progress, pipeline step results, Pause-before-swap inspection, Dry Run, and
// state-aware recovery actions offered by the engine.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { setCloseGuard, useWorkspace, type Tab } from "../app/workspace";
import { TagPill } from "../connection/EnvTag";
import { ErrorNote } from "../connection/ErrorNote";
import * as r from "../lib/ipc/runs";
import { toAppError, type AppError } from "../lib/ipc/errors";
import { revealPath } from "../lib/ipc/backup";
import { pickRestoreArchive } from "../lib/ipc/restore";
import { effectiveSource, type RefreshWorkflow, type WorkflowParam, type WorkflowSource } from "../lib/ipc/workflow";
import { openEditorTab } from "../editor/EditorTab";
import { useTrees } from "../explorer/tree/model";
import { useWorkflows } from "./store";
import { openWorkflowEditor } from "./WorkflowEditor";
import { fmtBytes, fmtElapsed } from "./progress";

export interface RunTabData {
  workflowId: string;
  workflowName: string;
  /** open an existing run (history) instead of starting one */
  runId?: string;
}

export function openRunTab(data: RunTabData) {
  useWorkspace.getState().openTab({
    id: data.runId ? `run:${data.runId}` : `run:new:${data.workflowId}:${crypto.randomUUID()}`,
    kind: "workflow-run",
    title: data.runId ? `Run of ${data.workflowName}` : data.workflowName,
    data,
  });
}

const STATUS_ICON: Record<string, string> = {
  ok: "✓", warn: "!", fail: "✕", skipped: "–",
  pending: "○", running: "◐", succeeded: "✓", failed: "✕", cancelled: "■", reused: "↺", interrupted: "⚠",
};

const RUN_STATUS: Record<r.RunStatus, string> = {
  running: "Running",
  succeeded: "Succeeded",
  succeeded_cleanup_required: "Succeeded — cleanup needed",
  paused_for_inspection: "Paused — inspect staging",
  failed_at_step: "Failed",
  failed_at_swap: "Failed during the swap",
  cancelled: "Cancelled",
  interrupted: "Interrupted",
};

const SECRET_LABEL: Record<r.SecretNeed, string> = {
  db_password: "Database password",
  ssh_key_passphrase: "SSH key passphrase",
  ssh_password: "SSH password",
};

function SecretPrompt({ need, onSaved }: { need: r.MissingSecret; onSaved: () => void }) {
  const [value, setValue] = useState("");
  const [persist, setPersist] = useState(true);
  const [error, setError] = useState<AppError | null>(null);
  return (
    <form
      className="inline-prompt secret-prompt"
      onSubmit={(e) => {
        e.preventDefault();
        void r.provideSecret(need.connection_id, need.kind, value, persist).then(onSaved).catch((err) => setError(toAppError(err)));
      }}
    >
      <span>
        {SECRET_LABEL[need.kind]} for <b>{need.connection_name}</b>
      </span>
      <input type="password" aria-label={`${SECRET_LABEL[need.kind]} for ${need.connection_name}`} value={value} onChange={(e) => setValue(e.target.value)} autoFocus />
      <label className="check">
        <input type="checkbox" checked={persist} onChange={(e) => setPersist(e.target.checked)} /> save
      </label>
      <button type="submit">Use</button>
      {error && <ErrorNote error={error} compact />}
    </form>
  );
}

function ArchivePromptBar({
  archivePath,
  onChangePath,
  onRecheck,
  busy,
}: {
  archivePath: string;
  onChangePath: (path: string) => void;
  onRecheck: (path: string) => void;
  busy: boolean;
}) {
  const browse = async () => {
    try {
      const picked = await pickRestoreArchive();
      if (picked) {
        onChangePath(picked);
        onRecheck(picked);
      }
    } catch {
      // ignore cancelled file dialog
    }
  };

  return (
    <section className="wf-archive-prompt" aria-label="Backup archive selection">
      <label className="grow">
        Backup archive file (.dump, directory, or .sql)
        <span className="row">
          <input
            aria-label="Backup archive path"
            className="mono grow"
            value={archivePath}
            placeholder="/path/to/backup.dump"
            onChange={(e) => onChangePath(e.target.value)}
          />
          <button type="button" onClick={() => void browse()} disabled={busy}>
            Browse…
          </button>
          <button type="button" className="primary" onClick={() => onRecheck(archivePath)} disabled={busy || !archivePath.trim()}>
            Check archive
          </button>
        </span>
      </label>
    </section>
  );
}

function Preflight({
  report,
  workflowId,
  workflow,
  archivePath,
  onArchivePathChange,
  onRecheck,
  onStart,
  busy,
}: {
  report: r.PreflightReport;
  workflowId: string;
  workflow?: RefreshWorkflow;
  archivePath: string;
  onArchivePathChange: (p: string) => void;
  onRecheck: (overrideArchivePath?: string) => void;
  onStart: (remember: boolean, opts?: r.StartRunOptions) => void;
  busy: boolean;
}) {
  const c = report.confirmation;
  const sourceMode: WorkflowSource | undefined = c?.source_mode ?? (workflow ? effectiveSource(workflow) : undefined);
  const isInPlace = sourceMode?.mode === "in_place_target";
  const needsArchivePick =
    sourceMode?.mode === "from_backup_file" && sourceMode.archive.kind === "prompt_at_run";

  const parameters: WorkflowParam[] = useMemo(
    () => (c?.parameters && c.parameters.length > 0 ? c.parameters : (workflow?.parameters ?? [])),
    [c?.parameters, workflow?.parameters],
  );

  const [remember, setRemember] = useState(false);
  const [typedName, setTypedName] = useState("");
  const [pauseBeforeSwap, setPauseBeforeSwap] = useState<boolean>(
    () => c?.pause_before_swap ?? workflow?.options.pause_before_swap ?? false,
  );
  const [pauseTouched, setPauseTouched] = useState(false);
  const [dryRun, setDryRun] = useState<boolean>(() => !!c?.dry_run);
  const [paramValues, setParamValues] = useState<Record<string, string>>(() => {
    const init: Record<string, string> = {};
    for (const p of parameters) {
      if (p.kind !== "secret" && p.default) init[p.name] = p.default;
    }
    return init;
  });
  const [secretValues, setSecretValues] = useState<Record<string, string>>({});

  useEffect(() => {
    setParamValues((prev) => {
      let changed = false;
      const next = { ...prev };
      for (const p of parameters) {
        if (p.kind !== "secret" && next[p.name] === undefined && p.default) {
          next[p.name] = p.default;
          changed = true;
        }
      }
      return changed ? next : prev;
    });
  }, [parameters]);

  useEffect(() => {
    if (!pauseTouched && c?.pause_before_swap !== undefined) {
      setPauseBeforeSwap(c.pause_before_swap);
    }
  }, [c?.pause_before_swap, pauseTouched]);

  const missingRequiredParams = useMemo(
    () =>
      parameters.filter((p) => {
        if (!p.required) return false;
        if (p.kind === "secret") return !(secretValues[p.name] ?? "").length;
        const val = (paramValues[p.name] ?? p.default ?? "").trim();
        return val.length === 0;
      }),
    [parameters, paramValues, secretValues],
  );

  const failing = report.checks.some((x) => x.status === "fail");
  const typedNameMissing = !!c?.requires_typed_name && typedName.trim() !== c.target_db;
  const blocked =
    !!report.refused ||
    failing ||
    report.missing_secrets.length > 0 ||
    !c ||
    !report.ticket ||
    typedNameMissing ||
    missingRequiredParams.length > 0;

  const handleStart = () => {
    if (!c) return;
    const startOpts: r.StartRunOptions = {};
    if (c.requires_typed_name) startOpts.typedName = typedName.trim();
    if (parameters.length > 0) {
      const nonSecrets: Record<string, string> = {};
      const secrets: Record<string, string> = {};
      for (const p of parameters) {
        if (p.kind === "secret") {
          if (secretValues[p.name]) secrets[p.name] = secretValues[p.name];
        } else {
          const val = paramValues[p.name] ?? p.default ?? "";
          if (val !== "") nonSecrets[p.name] = val;
        }
      }
      if (Object.keys(nonSecrets).length > 0) startOpts.params = nonSecrets;
      if (Object.keys(secrets).length > 0) startOpts.secretParams = secrets;
    }
    if (!isInPlace && c.strategy === "staging_swap" && (pauseTouched || pauseBeforeSwap)) {
      startOpts.pauseBeforeSwap = pauseBeforeSwap;
    }
    if (isInPlace && dryRun) {
      startOpts.dryRun = true;
    }
    const hasOpts = Object.keys(startOpts).length > 0;
    onStart(c.can_remember ? remember : false, hasOpts ? startOpts : undefined);
  };

  const actionLabel = () => {
    if (!c) return "Start";
    if (isInPlace) return dryRun ? `Dry run on ${c.target_db}` : `Run on ${c.target_db}`;
    return c.create_mode ? `Create ${c.target_db}` : `Replace ${c.target_db}`;
  };

  return (
    <>
      {needsArchivePick && (
        <ArchivePromptBar
          archivePath={archivePath}
          onChangePath={onArchivePathChange}
          onRecheck={(p) => onRecheck(p)}
          busy={busy}
        />
      )}
      <section aria-label="Checks">
        <h3>Checks</h3>
        <ul className="checks-list">
          {report.checks.map((ch) => (
            <li key={ch.id} data-status={ch.status}>
              <span className="check-icon" aria-label={ch.status}>{STATUS_ICON[ch.status]}</span>
              <div>
                <b>{ch.label}</b> <span>{ch.message}</span>
                {ch.hint && <div className="muted">{ch.hint}</div>}
                {ch.fixes?.map((f) => (
                  <button key={`${f.kind}:${f.database}:${f.gid ?? ""}`} type="button" onClick={() => void r.applyFix(workflowId, f).then(() => onRecheck())}>
                    {f.kind === "drop_leftover" ? `Drop leftover ${f.database}` : `Roll back prepared transaction ${f.gid}`}
                  </button>
                ))}
              </div>
            </li>
          ))}
        </ul>
      </section>
      {report.missing_secrets.map((m) => (
        <SecretPrompt key={`${m.connection_id}:${m.kind}`} need={m} onSaved={() => onRecheck()} />
      ))}
      {report.refused && <ErrorNote error={report.refused} />}
      {c && (
        <section className="confirm-box" data-tag={c.target_tag} aria-label="Confirmation">
          <h3>
            {isInPlace ? (dryRun ? "Dry run on" : "Run in-place on") : c.create_mode ? "Create" : "Replace"}{" "}
            <span className="mono">{c.target_db}</span> on {c.target_server_label} <TagPill tag={c.target_tag} />
          </h3>
          {isInPlace ? (
            <p>
              {dryRun
                ? `Dry run: steps execute inside a transaction on ${c.target_db} and are rolled back at the end — no changes are saved.`
                : `Steps execute directly against ${c.target_db} inside a transaction.`}
            </p>
          ) : (
            <>
              <p>
                with a fresh copy of <span className="mono">{c.source_db}</span> from {c.source_label}.
              </p>
              <p>
                {c.strategy === "staging_swap"
                  ? `pgWhy restores into ${c.staging_db ?? "a staging database"} first and only swaps it in after everything succeeds — a failure leaves ${c.target_db} untouched.`
                  : `Drop-first: ${c.target_db} is dropped before restoring. A failed run will leave no local database.`}
                {report.retry && ` This retry begins at "${report.retry.begins_at}"${report.retry.reuses_backup ? " and reuses the existing backup" : ""}.`}
              </p>
            </>
          )}

          {parameters.length > 0 && (
            <section className="wf-run-params" aria-label="Parameters">
              <h4>Parameters</h4>
              <div className="wf-run-param-grid">
                {parameters.map((p) => {
                  const display = p.label || p.name;
                  if (p.kind === "secret") {
                    return (
                      <label key={p.name}>
                        <span>
                          {display} <span className="muted">(secret — memory only, never saved)</span>
                          {p.required && " *"}
                        </span>
                        <input
                          type="password"
                          autoComplete="off"
                          aria-label={display}
                          value={secretValues[p.name] ?? ""}
                          placeholder="Enter secret value"
                          onChange={(e) => setSecretValues((s) => ({ ...s, [p.name]: e.target.value }))}
                        />
                      </label>
                    );
                  }
                  if (p.kind === "file_path") {
                    return (
                      <label key={p.name}>
                        <span>
                          {display}
                          {p.required && " *"}
                        </span>
                        <span className="row">
                          <input
                            aria-label={display}
                            className="mono grow"
                            value={paramValues[p.name] ?? ""}
                            placeholder="/path/to/file"
                            onChange={(e) => setParamValues((v) => ({ ...v, [p.name]: e.target.value }))}
                          />
                          <button
                            type="button"
                            onClick={() =>
                              void pickRestoreArchive()
                                .then((picked) => {
                                  if (picked) setParamValues((v) => ({ ...v, [p.name]: picked }));
                                })
                                .catch(() => {})
                            }
                          >
                            Browse…
                          </button>
                        </span>
                      </label>
                    );
                  }
                  return (
                    <label key={p.name}>
                      <span>
                        {display}
                        {p.required && " *"}
                      </span>
                      <input
                        aria-label={display}
                        className={p.kind === "number" ? "mono" : undefined}
                        inputMode={p.kind === "number" ? "decimal" : undefined}
                        value={paramValues[p.name] ?? ""}
                        placeholder={p.default ?? (p.kind === "number" ? "0" : "")}
                        onChange={(e) => setParamValues((v) => ({ ...v, [p.name]: e.target.value }))}
                      />
                    </label>
                  );
                })}
              </div>
              {missingRequiredParams.length > 0 && (
                <p className="muted">
                  Enter required {missingRequiredParams.length === 1 ? "parameter" : "parameters"}:{" "}
                  {missingRequiredParams.map((p) => p.label || p.name).join(", ")}.
                </p>
              )}
            </section>
          )}

          <div className="wf-run-toggles">
            {!isInPlace && c.strategy === "staging_swap" && (
              <label className="check">
                <input
                  type="checkbox"
                  checked={pauseBeforeSwap}
                  onChange={(e) => {
                    setPauseTouched(true);
                    setPauseBeforeSwap(e.target.checked);
                  }}
                />
                Pause before swap (inspect staging database before replacing {c.target_db})
              </label>
            )}
            {isInPlace && (
              <label className="check">
                <input
                  type="checkbox"
                  checked={dryRun}
                  onChange={(e) => setDryRun(e.target.checked)}
                />
                Dry run (execute inside a transaction and roll back all changes)
              </label>
            )}
          </div>

          {c.warnings.length > 0 && (
            <ul className="confirm-warnings">
              {c.warnings.map((w) => (
                <li key={w}>{w}</li>
              ))}
            </ul>
          )}

          {c.requires_typed_name && (
            <div className="wf-typed-confirm">
              <label>
                <span>
                  Type <code className="mono">{c.target_db}</code> to confirm running against this{" "}
                  <b>{c.target_tag}</b> database:
                </span>
                <input
                  aria-label="Confirm target database name"
                  className="mono"
                  value={typedName}
                  placeholder={c.target_db}
                  onChange={(e) => setTypedName(e.target.value)}
                />
              </label>
            </div>
          )}

          <div className="actions">
            {c.can_remember && !c.requires_typed_name && (
              <label className="check">
                <input type="checkbox" checked={remember} onChange={(e) => setRemember(e.target.checked)} /> Don't ask again for this workflow
              </label>
            )}
            <span className="spacer" />
            <button type="button" onClick={() => onRecheck()} disabled={busy}>Re-check</button>
            <button
              type="button"
              className={dryRun ? "primary" : "danger"}
              disabled={blocked || busy}
              onClick={handleStart}
            >
              {actionLabel()}
            </button>
          </div>
        </section>
      )}
    </>
  );
}

function Steps({
  record,
  live,
  liveOutput,
}: {
  record: r.RunRecord;
  live: Record<string, r.StepProgress>;
  liveOutput: Record<string, string[]>;
}) {
  return (
    <ol className="steps" aria-label="Steps">
      {record.steps.map((s) => {
        const p = live[s.step];
        const tail = s.output_tail.length > 0 ? s.output_tail : (liveOutput[s.step] ?? []);
        return (
          <li key={s.step} data-status={s.status}>
            <span className="check-icon">{STATUS_ICON[s.status]}</span>
            <div className="step-body">
              <div>
                <b>{s.label}</b>
                {s.duration_ms != null && <span className="muted"> · {fmtElapsed(s.duration_ms)}</span>}
                {s.note && <span className="muted"> · {s.note}</span>}
              </div>
              {s.status === "running" && p && (
                <div className="progress-line">
                  {p.current_object && <span className="mono">{p.current_object}</span>}
                  <span>
                    {p.total_is_estimate ? "~" : ""}
                    {p.objects_done.toLocaleString()}
                    {p.objects_total != null && ` of ${p.total_is_estimate ? "~" : ""}${p.objects_total.toLocaleString()}`} objects
                  </span>
                  {p.bytes_written != null && <span>{fmtBytes(p.bytes_written)}</span>}
                  <span>{fmtElapsed(p.elapsed_ms)}</span>
                </div>
              )}
              {s.status === "running" && s.step === "swap" && record.swap_phase && <div className="muted">{record.swap_phase.replace(/_/g, " ")}</div>}
              {s.error && <ErrorNote error={s.error} compact />}
              {(s.status === "failed" || s.status === "running") && tail.length > 0 && (
                <details open={s.status === "failed"}>
                  <summary>Output</summary>
                  <pre className="mono output-tail">{tail.slice(-20).join("\n")}</pre>
                </details>
              )}
            </div>
          </li>
        );
      })}
    </ol>
  );
}

function PipelineStepResults({ results }: { results: r.PipelineStepResult[] }) {
  if (results.length === 0) return null;
  return (
    <section className="wf-pipeline-results" aria-label="Pipeline steps">
      <h3>Pipeline steps</h3>
      <ul className="checks-list">
        {results.map((ps) => (
          <li key={ps.step_id} data-status={ps.status}>
            <span className="check-icon">{STATUS_ICON[ps.status] ?? "·"}</span>
            <div className="step-body">
              <div>
                <b>{ps.step_name}</b>
                {ps.is_verify && <span className="wf-step-badge" data-kind="verify">Verify</span>}
                {ps.rows_affected != null && (
                  <span className="muted"> · {ps.rows_affected.toLocaleString()} {ps.rows_affected === 1 ? "row" : "rows"}</span>
                )}
                {ps.summary && <span className="muted"> · {ps.summary}</span>}
              </div>
              {ps.preview_sql && (
                <details>
                  <summary>SQL</summary>
                  <pre className="mono output-tail">{ps.preview_sql}</pre>
                </details>
              )}
            </div>
          </li>
        ))}
      </ul>
    </section>
  );
}

type Phase =
  | { kind: "checking" }
  | { kind: "preflight"; report: r.PreflightReport }
  | { kind: "run"; runId: string; record: r.RunRecord | null };

export function RunTab({ tab }: { tab: Tab }) {
  const data = tab.data as RunTabData;
  const workflow = useWorkflows((s) => s.workflows.find((w) => w.id === data.workflowId));
  const [phase, setPhase] = useState<Phase>(() => (data.runId ? { kind: "run", runId: data.runId, record: null } : { kind: "checking" }));
  const [archivePath, setArchivePath] = useState("");
  const [live, setLive] = useState<Record<string, r.StepProgress>>({});
  const [liveOutput, setLiveOutput] = useState<Record<string, string[]>>({});
  const [error, setError] = useState<AppError | null>(null);
  const [busy, setBusy] = useState(false);
  const ticket = useRef<string | null>(null);
  const runRef = useRef<string | null>(data.runId ?? null);
  const archivePathRef = useRef("");
  const updateTab = useWorkspace((s) => s.updateTab);

  const runPreflight = useCallback(async (retry?: { runId: string; from: r.StepId }, overrideArchivePath?: string) => {
    setError(null);
    setPhase({ kind: "checking" });
    if (ticket.current) void r.discardPreflight(ticket.current);
    const chosenArchive = (overrideArchivePath ?? archivePathRef.current).trim();
    const opts: r.PreflightOptions | undefined = chosenArchive ? { archive_path: chosenArchive } : undefined;
    try {
      const report = retry
        ? await r.retryPreflight(retry.runId, retry.from, opts)
        : await r.preflight(data.workflowId, opts);
      ticket.current = report.ticket ?? null;
      // a remembered confirmation (Local targets, after a first success) runs straight away
      // unless runtime parameters or typed-name confirmation are required
      const c = report.confirmation;
      const hasParams = (c?.parameters?.length ?? 0) > 0;
      if (
        c?.remembered &&
        !c.requires_typed_name &&
        !hasParams &&
        report.ticket &&
        !report.refused &&
        report.missing_secrets.length === 0 &&
        !report.checks.some((x) => x.status === "fail")
      ) {
        await start(report, false);
        return;
      }
      setPhase({ kind: "preflight", report });
    } catch (e) {
      setError(toAppError(e));
      setPhase({ kind: "preflight", report: { checks: [], missing_secrets: [] } });
    }
  }, [data.workflowId]); // eslint-disable-line react-hooks/exhaustive-deps

  const start = async (report: r.PreflightReport, remember: boolean, opts?: r.StartRunOptions) => {
    if (!report.ticket || !report.confirmation) return;
    setBusy(true);
    setError(null);
    try {
      const runId = await r.startRun(report.ticket, report.confirmation.fingerprint, remember, opts);
      ticket.current = null;
      runRef.current = runId;
      setLive({});
      setLiveOutput({});
      setPhase({ kind: "run", runId, record: null });
      updateTab(tab.id, { data: { ...data, runId } });
      setPhase({ kind: "run", runId, record: await r.getRun(runId) });
    } catch (e) {
      setError(toAppError(e));
    } finally {
      setBusy(false);
    }
  };

  useEffect(() => {
    if (data.runId) void r.getRun(data.runId).then((record) => setPhase({ kind: "run", runId: data.runId!, record })).catch((e) => setError(toAppError(e)));
    else void runPreflight();
    const unlisten = r.onRunEvent((ev) => {
      if (ev.run_id !== runRef.current) return;
      if (ev.event === "progress") {
        setLive((l) => ({ ...l, [ev.step]: ev.progress }));
      } else if (ev.event === "output") {
        // Buffer live output lines in memory rather than triggering a full disk-read getRun() per line.
        setLiveOutput((o) => {
          const prev = o[ev.step] ?? [];
          const next = prev.length >= 60 ? [...prev.slice(-59), ev.line] : [...prev, ev.line];
          return { ...o, [ev.step]: next };
        });
      } else if (ev.event === "run_started" || ev.event === "run_finished") {
        setPhase({ kind: "run", runId: ev.run_id, record: ev.record });
        if (
          ev.event === "run_finished" &&
          (ev.status === "succeeded" || ev.status === "succeeded_cleanup_required") &&
          !ev.record.dry_run
        ) {
          // the explorer re-reads the replaced database
          useTrees.getState().invalidate(ev.record.target.connection_id, ev.record.target.database);
        }
      } else {
        void r.getRun(ev.run_id).then((record) => setPhase((p) => (p.kind === "run" && p.runId === ev.run_id ? { ...p, record } : p)));
      }
    });
    return () => {
      // the listener may already be gone (window closing / test env)
      void unlisten.then((f) => Promise.resolve().then(f)).catch(() => {});
    };
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    setCloseGuard(tab.id, async () => {
      if (ticket.current) void r.discardPreflight(ticket.current);
      return true;
    });
    return () => setCloseGuard(tab.id, null);
  }, [tab.id]);

  const act = async (rec: r.RunRecord, a: r.ActionOffer) => {
    setError(null);
    if (a.kind === "retry" && a.retry) return runPreflight({ runId: rec.run_id, from: a.retry.from });
    setBusy(true);
    try {
      const next = await r.runAction(rec.run_id, a.kind);
      setPhase({ kind: "run", runId: rec.run_id, record: next });
      if (a.kind === "complete_swap" || a.kind === "rollback_swap") useTrees.getState().invalidate(next.target.connection_id, next.target.database);
    } catch (e) {
      setError(toAppError(e));
    } finally {
      setBusy(false);
    }
  };

  const inspectStaging = (rec: r.RunRecord) => {
    if (!rec.staging_db) return;
    openEditorTab(rec.target.connection_id, rec.staging_db, {
      title: `Inspect ${rec.staging_db}`,
      text: `-- Inspecting staging database "${rec.staging_db}" before swapping into "${rec.target.database}".\n-- Return to the workflow run tab to Complete swap, Edit & retry transforms, or Drop staging.\n`,
    });
  };

  const record = phase.kind === "run" ? phase.record : null;
  const running = record?.status === "running";
  const isPaused = record?.status === "paused_for_inspection";

  const describeRunSource = (rec: r.RunRecord): string => {
    if (rec.source_mode?.mode === "in_place_target") {
      return `In-place on ${rec.target.connection_name}/${rec.target.database}${rec.dry_run ? " · dry run (rolled back)" : ""}`;
    }
    if (rec.source_mode?.mode === "from_backup_file") {
      return `Backup archive → ${rec.target.connection_name}/${rec.target.database}${rec.strategy === "drop_first" ? " · drop-first" : " · staging then swap"}`;
    }
    return `${rec.source.connection_name}/${rec.source.database} → ${rec.target.connection_name}/${rec.target.database}${rec.strategy === "drop_first" ? " · drop-first" : " · staging then swap"}`;
  };

  return (
    <div className="run-view" aria-label="Refresh run">
      <header className="run-header">
        <h2>{data.workflowName}</h2>
        {record && <span className="run-status" data-status={record.status}>{RUN_STATUS[record.status]}</span>}
        {running && (
          <button type="button" onClick={() => void r.cancelRun(record!.run_id).catch((e) => setError(toAppError(e)))} disabled={record!.renames_completed === 1}>
            Cancel
          </button>
        )}
        {record && !running && (
          <button type="button" onClick={() => void r.reinventory(record.run_id).then((x) => setPhase({ kind: "run", runId: x.run_id, record: x })).catch((e) => setError(toAppError(e)))}>
            Check current state
          </button>
        )}
      </header>
      {phase.kind === "checking" && <p className="muted">Checking tools, servers, privileges and disk space…</p>}
      {phase.kind === "preflight" && (
        <Preflight
          report={phase.report}
          workflowId={data.workflowId}
          workflow={workflow}
          archivePath={archivePath}
          onArchivePathChange={(p) => {
            archivePathRef.current = p;
            setArchivePath(p);
          }}
          onRecheck={(overrideArchive) => void runPreflight(undefined, overrideArchive)}
          onStart={(rem, opts) => void start(phase.report, rem, opts)}
          busy={busy}
        />
      )}
      {record && (
        <>
          <p className="muted">
            {describeRunSource(record)} · started {new Date(record.started_at).toLocaleString()}
          </p>
          {(record.headline || record.status === "succeeded" || record.dry_run) && (
            <div className={record.status === "succeeded" ? "ok-note" : "warn-note"} role="status">
              <p>
                {record.headline ??
                  (record.dry_run
                    ? `Dry run succeeded on ${record.target.database} — all changes were rolled back.`
                    : `${record.target.database} on ${record.target.connection_name} is now a fresh copy of ${record.source.database}` +
                      (record.backup ? ` (backup ${fmtBytes(record.backup.size_bytes)})` : "") +
                      (record.finished_at ? `, finished in ${fmtElapsed(new Date(record.finished_at).getTime() - new Date(record.started_at).getTime())}` : "") +
                      ".")}
              </p>
            </div>
          )}
          <Steps record={record} live={live} liveOutput={liveOutput} />
          {record.pipeline_steps && record.pipeline_steps.length > 0 && (
            <PipelineStepResults results={record.pipeline_steps} />
          )}
          {record.error && <ErrorNote error={record.error} />}
          {record.failed_items.length > 0 && (
            <section aria-label="Failed objects">
              <h3>Failed objects</h3>
              <ul className="details-list">
                {record.failed_items.map((f, i) => (
                  <li key={i}>
                    {f.object && <b>{f.object}</b>} <span className="mono">{f.error}</span>
                  </li>
                ))}
              </ul>
            </section>
          )}
          {!running && (record.actions.length > 0 || (isPaused && record.staging_db)) && (
            <div className="run-actions" role="group" aria-label="Actions">
              {isPaused && record.staging_db && (
                <button type="button" className="primary" onClick={() => inspectStaging(record)}>
                  Inspect staging (SQL)
                </button>
              )}
              {isPaused && workflow && (
                <button type="button" onClick={() => openWorkflowEditor(workflow.id, workflow.name)}>
                  Edit workflow
                </button>
              )}
              {record.actions.map((a) => (
                <button
                  key={`${a.kind}:${a.database ?? ""}`}
                  type="button"
                  disabled={!a.enabled || busy}
                  title={a.disabled_reason ?? undefined}
                  onClick={() => void act(record, a)}
                >
                  {a.label}
                </button>
              ))}
              {record.backup && (
                <button type="button" onClick={() => void revealPath(record.backup!.path)}>
                  Open backup folder
                </button>
              )}
            </div>
          )}
        </>
      )}
      {error && <ErrorNote error={error} />}
    </div>
  );
}
