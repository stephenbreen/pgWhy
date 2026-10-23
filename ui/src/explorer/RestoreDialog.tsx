// Standalone restore dialog (v1.1 Epic 2): pick an archive, choose where it goes, watch it run.
//
// The core re-inspects the archive and re-checks every gate before it starts; this dialog only
// collects the choice and shows what happened.

import { useEffect, useRef, useState } from "react";
import { create } from "zustand";
import { ErrorNote } from "../connection/ErrorNote";
import { toAppError, type AppError } from "../lib/ipc/errors";
import type { OperationRecord, StepStatus } from "../lib/ipc/databases";
import * as rs from "../lib/ipc/restore";
import { useConnections } from "../connection/store";
import { registerNodeAction, type NodeTarget } from "./actions";
import { useTrees } from "./tree/model";

interface OpenState {
  connectionId: string;
  database?: string;
  envTag?: string;
  /** Preselected archive (from a history row's backup). */
  archive?: string;
}

interface RestoreState extends OpenState {
  open: (s: OpenState) => void;
  close: () => void;
}

export const useRestore = create<RestoreState>((set) => ({
  connectionId: "",
  open: (s) => set(s),
  close: () => set({ connectionId: "", database: undefined, archive: undefined }),
}));

type Mode = rs.RestoreMode;

interface StepView {
  id: string;
  label: string;
  status: StepStatus;
}

export function RestoreDialog() {
  const { connectionId, database, envTag, archive: preselected, close } = useRestore();
  const [targets, setTargets] = useState<rs.RestoreTargets | null>(null);
  const [archive, setArchive] = useState<string | null>(null);
  const [inspection, setInspection] = useState<rs.RestoreInspection | null>(null);
  const [mode, setMode] = useState<Mode>("new_database");
  const [name, setName] = useState("");
  const [localeFrom, setLocaleFrom] = useState("");
  const [typed, setTyped] = useState("");
  const [backupFirst, setBackupFirst] = useState(true);
  const [noOwner, setNoOwner] = useState(true);
  const [noPrivileges, setNoPrivileges] = useState(true);
  const [stopOnFirst, setStopOnFirst] = useState(true);
  const [jobs, setJobs] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<AppError | null>(null);
  const [steps, setSteps] = useState<StepView[]>([]);
  const [progress, setProgress] = useState<{ done: number; total?: number; current?: string } | null>(null);
  const [log, setLog] = useState<string[]>([]);
  const [report, setReport] = useState<OperationRecord | null>(null);
  const [runId, setRunId] = useState<string | null>(null);
  const logRef = useRef<HTMLPreElement>(null);

  const open = connectionId !== "";

  useEffect(() => {
    if (!open) return;
    setArchive(preselected ?? null);
    setInspection(null);
    setMode("new_database");
    setName("");
    setLocaleFrom(database ?? "");
    setTyped("");
    setBackupFirst(true);
    setBusy(false);
    setError(null);
    setSteps([]);
    setProgress(null);
    setLog([]);
    setReport(null);
    setRunId(null);
    let live = true;
    if (preselected) {
      void rs
        .restoreInspect(preselected)
        .then((view) => {
          if (!live) return;
          setInspection(view);
          if (view.dbname) setName(`${view.dbname}_restore`);
        })
        .catch((e) => live && setError(toAppError(e)));
    }
    void rs
      .restoreTargets(connectionId)
      .then((t) => {
        if (!live) return;
        setTargets(t);
        if (!t.can_create_database) setMode("on_top");
      })
      .catch((e) => live && setError(toAppError(e)));
    return () => {
      live = false;
    };
  }, [open, connectionId, database, preselected]);

  useEffect(() => {
    logRef.current?.scrollTo({ top: logRef.current.scrollHeight });
  }, [log]);

  if (!open) return null;

  const tagBlocked = envTag !== undefined && envTag !== "local" && envTag !== "dev";
  const canCreate = targets?.can_create_database ?? false;
  const blockers = inspection?.blockers ?? [];
  const databaseName = mode === "new_database" ? name.trim() : (database ?? "");
  const startBlocked =
    busy ||
    tagBlocked ||
    !inspection ||
    blockers.length > 0 ||
    databaseName === "" ||
    (mode === "new_database" && !canCreate) ||
    (mode !== "new_database" && typed !== database);

  const pick = async () => {
    setError(null);
    try {
      const path = await rs.pickRestoreArchive();
      if (!path) return;
      setArchive(path);
      setInspection(null);
      const view = await rs.restoreInspect(path);
      setInspection(view);
      if (view.dbname) {
        setName(view.dbname === database ? `${view.dbname}_restore` : `${view.dbname}_restore`);
        if (!localeFrom) setLocaleFrom(view.dbname);
      }
    } catch (e) {
      setError(toAppError(e));
    }
  };

  const start = async () => {
    if (!inspection || !archive) return;
    setBusy(true);
    setError(null);
    setSteps([]);
    setProgress(null);
    setLog([]);
    setReport(null);
    const settings: rs.RestoreSettings = {
      jobs: jobs === "" ? null : Number(jobs),
      no_owner: noOwner,
      no_privileges: noPrivileges,
      stop_on_first_error: stopOnFirst,
    };
    try {
      const rec = await rs.restoreStart(
        connectionId,
        {
          path: archive,
          database: databaseName,
          mode,
          locale_from: mode === "new_database" ? localeFrom || null : null,
          settings,
          typed_name: mode === "new_database" ? null : typed,
          backup_first: mode !== "new_database" && backupFirst,
        },
        (e) => {
          if (e.type === "started") {
            setRunId(e.id);
            return;
          }
          if (e.type === "step") {
            setSteps((s) => {
              const next = s.filter((x) => x.id !== e.id);
              return [...next, { id: e.id, label: e.label, status: e.status }];
            });
            return;
          }
          if (e.type === "progress") {
            setProgress({
              done: e.progress.objects_done,
              total: e.progress.objects_total,
              current: e.progress.current_object,
            });
            return;
          }
          setLog((l) => (l.length > 400 ? [...l.slice(-200), e.line] : [...l, e.line]));
        },
      );
      setReport(rec);
      void useTrees.getState().loadDatabases(connectionId);
      useTrees.getState().invalidate(connectionId);
    } catch (e) {
      setError(toAppError(e));
    } finally {
      setBusy(false);
      setRunId(null);
    }
  };

  return (
    <div
      className="modal-backdrop"
      role="presentation"
      onMouseDown={(e) => e.target === e.currentTarget && !busy && close()}
    >
      <div className="modal" role="dialog" aria-modal="true" aria-label="Restore a backup">
        <h2>Restore a backup</h2>

        {report ? (
          <>
            <div className="op-report" role="status">
              <p
                data-kind={
                  report.status === "succeeded" || report.status === "succeeded_cleanup_required"
                    ? "ok"
                    : "failed"
                }
              >
                {report.status === "succeeded" || report.status === "succeeded_cleanup_required"
                  ? "Restored"
                  : "Not completed"}
                {report.error ? `: ${report.error.message}` : ""}
              </p>
              {report.error?.hint && <p className="note">{report.error.hint}</p>}
              {report.warnings.map((w) => (
                <p key={w} className="note warn">
                  {w}
                </p>
              ))}
              <ol className="op-steps">
                {report.steps.map((s) => (
                  <li key={s.id} data-state={s.status}>
                    <span>{s.label}</span>
                    <span className="note">
                      {s.status}
                      {s.note ? ` — ${s.note}` : ""}
                      {s.error?.message ? ` — ${s.error.message}` : ""}
                    </span>
                  </li>
                ))}
              </ol>
              <p className="note">Recorded in run history as {report.id.slice(0, 8)}.</p>
            </div>
            <div className="modal-actions">
              <button type="button" onClick={close}>
                Close
              </button>
            </div>
          </>
        ) : (
          <form
            onSubmit={(e) => {
              e.preventDefault();
              void start();
            }}
          >
            <label>
              Archive
              <span className="row">
                <input aria-label="Archive" value={archive ?? ""} readOnly placeholder="choose a .dump file" />
                <button type="button" onClick={() => void pick()} disabled={busy}>
                  Choose…
                </button>
              </span>
            </label>
            {inspection && (
              <p className="note">
                {inspection.dbname ? `database ${inspection.dbname}` : "unknown database"} · dumped by
                pg_dump {inspection.dumped_by ?? "?"} ({inspection.dumped_from ?? "?"}) ·{" "}
                {inspection.toc_entries ?? "?"} items · restores with {inspection.tool}
              </p>
            )}
            {blockers.map((b) => (
              <p key={b} className="note warn">
                {b}
              </p>
            ))}

            <fieldset className="mode-choice">
              <legend>Where</legend>
              <label className="check">
                <input
                  type="radio"
                  name="mode"
                  aria-label="New database"
                  checked={mode === "new_database"}
                  disabled={busy || !canCreate}
                  onChange={() => setMode("new_database")}
                />
                New database {!canCreate && <span className="note">(needs CREATEDB)</span>}
              </label>
              <label className="check">
                <input
                  type="radio"
                  name="mode"
                  aria-label="Replace database"
                  checked={mode === "replace"}
                  disabled={busy || !database || !canCreate}
                  onChange={() => setMode("replace")}
                />
                Replace <b>{database}</b> (restore into a copy, then swap it in)
              </label>
              <label className="check">
                <input
                  type="radio"
                  name="mode"
                  aria-label="Restore on top"
                  checked={mode === "on_top"}
                  disabled={busy || !database}
                  onChange={() => setMode("on_top")}
                />
                Restore over <b>{database}</b> in place
              </label>
            </fieldset>

            {mode === "new_database" ? (
              <>
                <label>
                  New database name
                  <input
                    aria-label="New database name"
                    value={name}
                    onChange={(e) => setName(e.target.value)}
                    spellCheck={false}
                  />
                </label>
                <label>
                  Copy encoding and locale from
                  <input
                    aria-label="Locale source database"
                    value={localeFrom}
                    onChange={(e) => setLocaleFrom(e.target.value)}
                    list="restore-locale-sources"
                    placeholder="(server defaults)"
                    spellCheck={false}
                  />
                  <datalist id="restore-locale-sources">
                    {(targets?.databases ?? []).map((d) => (
                      <option key={d} value={d} />
                    ))}
                  </datalist>
                </label>
              </>
            ) : (
              <>
                <p className="note">
                  {mode === "replace" ? (
                    <>
                      The archive is restored into a staging copy, verified, then swapped over{" "}
                      <b>{database}</b> — which keeps its name. A failed restore leaves{" "}
                      <b>{database}</b> untouched. Sessions on it are ended at the swap.
                    </>
                  ) : (
                    <>
                      Objects in the archive are applied over what is already there; objects it does
                      not contain stay. A table that already exists makes pg_restore fail unless you
                      allow continuing past errors.
                    </>
                  )}{" "}
                  Type the database name to confirm.
                </p>
                <label>
                  Database name
                  <input
                    aria-label="Type the database name to confirm"
                    value={typed}
                    onChange={(e) => setTyped(e.target.value)}
                    spellCheck={false}
                  />
                </label>
                <label className="check">
                  <input
                    type="checkbox"
                    checked={backupFirst}
                    onChange={(e) => setBackupFirst(e.target.checked)}
                  />
                  Back up first (kept by retention, recorded in history)
                </label>
              </>
            )}

            <fieldset>
              <legend>Options</legend>
              <label className="narrow">
                Parallel jobs
                <input
                  type="number"
                  min={1}
                  max={32}
                  value={jobs}
                  placeholder="default"
                  onChange={(e) => setJobs(e.target.value)}
                />
              </label>
              <label className="check">
                <input type="checkbox" checked={noOwner} onChange={(e) => setNoOwner(e.target.checked)} />
                Do not restore ownership
              </label>
              <label className="check">
                <input
                  type="checkbox"
                  checked={noPrivileges}
                  onChange={(e) => setNoPrivileges(e.target.checked)}
                />
                Do not restore privileges
              </label>
              <label className="check">
                <input
                  type="checkbox"
                  checked={stopOnFirst}
                  onChange={(e) => setStopOnFirst(e.target.checked)}
                />
                Stop at the first error
              </label>
            </fieldset>

            {tagBlocked && (
              <p className="note warn">
                This connection is tagged {envTag}; restores only run against Local or Dev.
              </p>
            )}
            {targets?.capabilities_error && <p className="note warn">{targets.capabilities_error}</p>}
            {error && <ErrorNote error={error} />}

            {(steps.length > 0 || progress) && (
              <div className="op-report" role="status">
                <p>
                  {progress
                    ? `${progress.done}${progress.total ? ` of ${progress.total}` : ""} objects${
                        progress.current ? ` — ${progress.current}` : ""
                      }`
                    : "starting…"}
                </p>
                <ol className="op-steps">
                  {steps.map((s) => (
                    <li key={s.id} data-state={s.status}>
                      <span>{s.label}</span>
                      <span className="note">{s.status}</span>
                    </li>
                  ))}
                </ol>
                {log.length > 0 && (
                  <pre className="restore-log" ref={logRef}>
                    {log.join("\n")}
                  </pre>
                )}
              </div>
            )}

            <div className="modal-actions">
              <button type="button" onClick={close} disabled={busy}>
                Cancel
              </button>
              {busy && runId && (
                <button type="button" onClick={() => void rs.restoreCancel(runId)}>
                  Stop the restore
                </button>
              )}
              <button type="submit" disabled={startBlocked}>
                {busy ? "Restoring…" : "Restore"}
              </button>
            </div>
          </form>
        )}
      </div>
    </div>
  );
}

/** Context-menu action: restore an archive into a new database (or over this one). */
export function registerRestoreActions() {
  const tagOf = (connectionId: string) =>
    useConnections.getState().entries.find((e) => e.id === connectionId)?.env_tag;
  registerNodeAction({
    id: "db-restore",
    label: "Restore a backup…",
    applies: (t) => t.type === "database",
    disabledReason: (t) => {
      const tag = t.type === "database" ? tagOf(t.connectionId) : undefined;
      return tag === undefined || tag === "local" || tag === "dev"
        ? null
        : `tagged ${tag} — restores need a Local or Dev target`;
    },
    run: (t) => {
      const node = t as NodeTarget & { type: "database" };
      useRestore.getState().open({
        connectionId: node.connectionId,
        database: node.db.name,
        envTag: tagOf(node.connectionId),
      });
    },
  });
}
