// The Refresh run view (R13, R14, AE2, AE6): preflight checks and the ONE confirmation up
// front (naming the exact target, server and strategy), then a docked, non-blocking live
// view with per-step progress, then a state-aware report. Retry and recovery actions come
// from the engine's offers — the UI never invents one.

import { useCallback, useEffect, useRef, useState } from "react";
import { setCloseGuard, useWorkspace, type Tab } from "../app/workspace";
import { TagPill } from "../connection/EnvTag";
import { ErrorNote } from "../connection/ErrorNote";
import * as r from "../lib/ipc/runs";
import { toAppError, type AppError } from "../lib/ipc/errors";
import { revealPath } from "../lib/ipc/backup";
import { fmtBytes, fmtElapsed } from "./progress";
import { useTrees } from "../explorer/tree/model";

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

function Preflight({
  report,
  workflowId,
  onRecheck,
  onStart,
  busy,
}: {
  report: r.PreflightReport;
  workflowId: string;
  onRecheck: () => void;
  onStart: (remember: boolean) => void;
  busy: boolean;
}) {
  const [remember, setRemember] = useState(false);
  const c = report.confirmation;
  const failing = report.checks.some((x) => x.status === "fail");
  const blocked = !!report.refused || failing || report.missing_secrets.length > 0 || !c || !report.ticket;
  return (
    <>
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
                  <button key={`${f.kind}:${f.database}:${f.gid ?? ""}`} type="button" onClick={() => void r.applyFix(workflowId, f).then(onRecheck)}>
                    {f.kind === "drop_leftover" ? `Drop leftover ${f.database}` : `Roll back prepared transaction ${f.gid}`}
                  </button>
                ))}
              </div>
            </li>
          ))}
        </ul>
      </section>
      {report.missing_secrets.map((m) => (
        <SecretPrompt key={`${m.connection_id}:${m.kind}`} need={m} onSaved={onRecheck} />
      ))}
      {report.refused && <ErrorNote error={report.refused} />}
      {c && (
        <section className="confirm-box" data-tag={c.target_tag} aria-label="Confirmation">
          <h3>
            {c.create_mode ? "Create" : "Replace"} <span className="mono">{c.target_db}</span> on {c.target_server_label} <TagPill tag={c.target_tag} />
          </h3>
          <p>
            with a fresh copy of <span className="mono">{c.source_db}</span> from {c.source_label}.
          </p>
          <p>
            {c.strategy === "staging_swap"
              ? `pgWhy restores into ${c.staging_db ?? "a staging database"} first and only swaps it in after everything succeeds — a failure leaves ${c.target_db} untouched.`
              : `Drop-first: ${c.target_db} is dropped before restoring. A failed run will leave no local database.`}
            {report.retry && ` This retry begins at "${report.retry.begins_at}"${report.retry.reuses_backup ? " and reuses the existing backup" : ""}.`}
          </p>
          {c.warnings.length > 0 && (
            <ul className="confirm-warnings">
              {c.warnings.map((w) => (
                <li key={w}>{w}</li>
              ))}
            </ul>
          )}
          <div className="actions">
            {c.can_remember && (
              <label className="check">
                <input type="checkbox" checked={remember} onChange={(e) => setRemember(e.target.checked)} /> Don't ask again for this workflow
              </label>
            )}
            <span className="spacer" />
            <button type="button" onClick={onRecheck} disabled={busy}>Re-check</button>
            <button type="button" className="danger" disabled={blocked || busy} onClick={() => onStart(remember)}>
              {c.create_mode ? `Create ${c.target_db}` : `Replace ${c.target_db}`}
            </button>
          </div>
        </section>
      )}
    </>
  );
}

function Steps({ record, live }: { record: r.RunRecord; live: Record<string, r.StepProgress> }) {
  return (
    <ol className="steps" aria-label="Steps">
      {record.steps.map((s) => {
        const p = live[s.step];
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
              {(s.status === "failed" || s.status === "running") && s.output_tail.length > 0 && (
                <details open={s.status === "failed"}>
                  <summary>Output</summary>
                  <pre className="mono output-tail">{s.output_tail.slice(-20).join("\n")}</pre>
                </details>
              )}
            </div>
          </li>
        );
      })}
    </ol>
  );
}

type Phase =
  | { kind: "checking" }
  | { kind: "preflight"; report: r.PreflightReport }
  | { kind: "run"; runId: string; record: r.RunRecord | null };

export function RunTab({ tab }: { tab: Tab }) {
  const data = tab.data as RunTabData;
  const [phase, setPhase] = useState<Phase>(() => (data.runId ? { kind: "run", runId: data.runId, record: null } : { kind: "checking" }));
  const [live, setLive] = useState<Record<string, r.StepProgress>>({});
  const [error, setError] = useState<AppError | null>(null);
  const [busy, setBusy] = useState(false);
  const ticket = useRef<string | null>(null);
  const runRef = useRef<string | null>(data.runId ?? null);
  const updateTab = useWorkspace((s) => s.updateTab);

  const runPreflight = useCallback(async (retry?: { runId: string; from: r.StepId }) => {
    setError(null);
    setPhase({ kind: "checking" });
    if (ticket.current) void r.discardPreflight(ticket.current);
    try {
      const report = retry ? await r.retryPreflight(retry.runId, retry.from) : await r.preflight(data.workflowId);
      ticket.current = report.ticket ?? null;
      // a remembered confirmation (Local targets, after a first success) runs straight away
      const c = report.confirmation;
      if (c?.remembered && report.ticket && !report.refused && report.missing_secrets.length === 0 && !report.checks.some((x) => x.status === "fail")) {
        await start(report, false);
        return;
      }
      setPhase({ kind: "preflight", report });
    } catch (e) {
      setError(toAppError(e));
      setPhase({ kind: "preflight", report: { checks: [], missing_secrets: [] } });
    }
  }, [data.workflowId]); // eslint-disable-line react-hooks/exhaustive-deps

  const start = async (report: r.PreflightReport, remember: boolean) => {
    if (!report.ticket || !report.confirmation) return;
    setBusy(true);
    try {
      const runId = await r.startRun(report.ticket, report.confirmation.fingerprint, remember);
      ticket.current = null;
      runRef.current = runId;
      setLive({});
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
      if (ev.event === "progress") setLive((l) => ({ ...l, [ev.step]: ev.progress }));
      else if (ev.event === "run_started" || ev.event === "run_finished") {
        setPhase({ kind: "run", runId: ev.run_id, record: ev.record });
        if (ev.event === "run_finished" && (ev.status === "succeeded" || ev.status === "succeeded_cleanup_required")) {
          // EC-1: the explorer re-reads the replaced database
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

  const record = phase.kind === "run" ? phase.record : null;
  const running = record?.status === "running";

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
      {phase.kind === "preflight" && <Preflight report={phase.report} workflowId={data.workflowId} onRecheck={() => void runPreflight()} onStart={(rem) => void start(phase.report, rem)} busy={busy} />}
      {record && (
        <>
          <p className="muted">
            {record.source.connection_name}/{record.source.database} → {record.target.connection_name}/{record.target.database}
            {record.strategy === "drop_first" ? " · drop-first" : " · staging then swap"} · started {new Date(record.started_at).toLocaleString()}
          </p>
          {(record.headline || record.status === "succeeded") && (
            <div className={record.status === "succeeded" ? "ok-note" : "warn-note"} role="status">
              <p>
                {record.headline ??
                  `${record.target.database} on ${record.target.connection_name} is now a fresh copy of ${record.source.database}` +
                    (record.backup ? ` (backup ${fmtBytes(record.backup.size_bytes)})` : "") +
                    (record.finished_at ? `, finished in ${fmtElapsed(new Date(record.finished_at).getTime() - new Date(record.started_at).getTime())}` : "") +
                    "."}
              </p>
            </div>
          )}
          <Steps record={record} live={live} />
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
          {!running && record.actions.length > 0 && (
            <div className="run-actions" role="group" aria-label="Actions">
              {record.actions.map((a) => (
                <button key={`${a.kind}:${a.database ?? ""}`} type="button" disabled={!a.enabled || busy} title={a.disabled_reason ?? undefined} onClick={() => void act(record, a)}>
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
