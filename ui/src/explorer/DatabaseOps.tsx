// Database lifecycle dialogs (v1.1 Epic 4): create, rename, drop.
//
// One small modal, a mode at a time, opened from the database node's context menu. The rules
// that matter are enforced in the core (env-tag gate, exact typed name, per-server run lock) —
// this file only collects input, shows the plan's warnings, and renders the operation report.

import { useEffect, useState } from "react";
import { create } from "zustand";
import { ErrorNote } from "../connection/ErrorNote";
import { toAppError, type AppError } from "../lib/ipc/errors";
import * as db from "../lib/ipc/databases";
import { useTrees } from "./tree/model";
import { useConnections } from "../connection/store";
import { registerNodeAction, type NodeTarget } from "./actions";

type Mode = "create" | "rename" | "drop";

interface OpenState {
  mode: Mode;
  connectionId: string;
  database?: string;
  envTag?: string;
}

interface DbOpsState extends OpenState {
  open: (s: OpenState) => void;
  close: () => void;
}

export const useDbOps = create<DbOpsState>((set) => ({
  mode: "create",
  connectionId: "",
  open: (s) => set(s),
  close: () => set({ connectionId: "", database: undefined }),
}));

/** Databases already loaded for the connection, for the "copy locale from" picker. */
function databaseNames(connectionId: string): string[] {
  const tree = useTrees.getState().trees[connectionId];
  const loaded = tree?.databases;
  return loaded?.state === "loaded" ? loaded.value.map((d) => d.name) : [];
}

function StatusLine({ rec }: { rec: db.OperationRecord }) {
  const failed = rec.status !== "succeeded";
  return (
    <div className="op-report" role="status">
      <p data-kind={failed ? "failed" : "ok"}>
        {failed ? "Not completed" : "Done"}
        {rec.error ? `: ${rec.error.message}` : ""}
      </p>
      {rec.error?.hint && <p className="note">{rec.error.hint}</p>}
      {rec.warnings.map((w) => (
        <p key={w} className="note warn">
          {w}
        </p>
      ))}
      <ol className="op-steps">
        {rec.steps.map((s) => (
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
      <p className="note">Recorded in run history as {rec.id.slice(0, 8)}.</p>
    </div>
  );
}

export function DatabaseOps() {
  const { mode, connectionId, database, envTag, close } = useDbOps();
  const [caps, setCaps] = useState<db.DbCapabilities | null>(null);
  const [capsError, setCapsError] = useState<AppError | null>(null);
  const [name, setName] = useState("");
  const [localeFrom, setLocaleFrom] = useState("");
  const [comment, setComment] = useState("");
  const [typed, setTyped] = useState("");
  const [backupFirst, setBackupFirst] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<AppError | null>(null);
  const [report, setReport] = useState<db.OperationRecord | null>(null);

  const open = connectionId !== "";
  useEffect(() => {
    if (!open) return;
    setName(mode === "rename" ? `${database}_dev` : "new_database");
    setLocaleFrom(database ?? "");
    setComment("");
    setTyped("");
    setBackupFirst(true);
    setError(null);
    setReport(null);
    setCaps(null);
    setCapsError(null);
    let live = true;
    void db
      .dbCapabilities(connectionId)
      .then((c) => live && setCaps(c))
      .catch((e) => live && setCapsError(toAppError(e)));
    return () => {
      live = false;
    };
  }, [open, connectionId, database, mode]);

  if (!open) return null;

  // The core refuses these too; showing the reason up front is only about not wasting a click.
  const tagBlocked = envTag !== undefined && envTag !== "local" && envTag !== "dev";
  const createBlocked = caps !== null && !caps.can_create_database;
  const names = databaseNames(connectionId);

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      const rec =
        mode === "create"
          ? await db.dbCreate(connectionId, name, localeFrom || null, comment || null)
          : mode === "rename"
            ? await db.dbRename(connectionId, database ?? "", name)
            : await db.dbDrop(connectionId, database ?? "", typed, backupFirst);
      setReport(rec);
      // the database list changed (or the tree is stale for the renamed one)
      void useTrees.getState().loadDatabases(connectionId);
      useTrees.getState().invalidate(connectionId);
    } catch (e) {
      setError(toAppError(e));
    } finally {
      setBusy(false);
    }
  };

  const title =
    mode === "create" ? "New database" : mode === "rename" ? `Rename ${database}` : `Drop ${database}`;
  const blockedReason =
    tagBlocked
      ? `This connection is tagged ${envTag}; destructive actions only run against Local or Dev.`
      : mode === "create" && createBlocked
        ? `${caps?.current_user} cannot create databases on ${caps?.server_version}.`
        : null;

  return (
    <div className="modal-backdrop" role="presentation" onMouseDown={(e) => e.target === e.currentTarget && !busy && close()}>
      <div className="modal" role="dialog" aria-modal="true" aria-label={title}>
        <h2>{title}</h2>

        {report ? (
          <>
            <StatusLine rec={report} />
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
              void submit();
            }}
          >
            {mode !== "drop" && (
              <label>
                Name
                <input
                  aria-label="Database name"
                  value={name}
                  onChange={(e) => setName(e.target.value)}
                  autoFocus
                  spellCheck={false}
                />
              </label>
            )}

            {mode === "create" && (
              <>
                <label>
                  Copy encoding and locale from
                  <input
                    aria-label="Locale source database"
                    value={localeFrom}
                    onChange={(e) => setLocaleFrom(e.target.value)}
                    list="db-locale-sources"
                    placeholder="(server defaults)"
                    spellCheck={false}
                  />
                  <datalist id="db-locale-sources">
                    {names.map((n) => (
                      <option key={n} value={n} />
                    ))}
                  </datalist>
                </label>
                <label>
                  Comment (optional)
                  <input aria-label="Comment" value={comment} onChange={(e) => setComment(e.target.value)} />
                </label>
              </>
            )}

            {mode === "drop" && (
              <>
                <p className="note">
                  This deletes <b>{database}</b> and everything in it. Other sessions on it are terminated
                  first. Type the database name to confirm.
                </p>
                <label>
                  Database name
                  <input
                    aria-label="Type the database name to confirm"
                    value={typed}
                    onChange={(e) => setTyped(e.target.value)}
                    autoFocus
                    spellCheck={false}
                  />
                </label>
                <label className="check">
                  <input type="checkbox" checked={backupFirst} onChange={(e) => setBackupFirst(e.target.checked)} />
                  Back up first (kept by retention, recorded in history)
                </label>
              </>
            )}

            {mode === "rename" && (
              <p className="note">
                Sessions on <b>{database}</b> are terminated so the rename can proceed. Its contents are kept.
              </p>
            )}

            {blockedReason && <p className="note warn">{blockedReason}</p>}
            {capsError && <ErrorNote error={capsError} compact />}
            {error && <ErrorNote error={error} />}

            <div className="modal-actions">
              <button type="button" onClick={close} disabled={busy}>
                Cancel
              </button>
              <button
                type="submit"
                className={mode === "drop" ? "danger" : undefined}
                disabled={
                  busy ||
                  !!blockedReason ||
                  (mode === "drop" && typed !== database) ||
                  (mode !== "drop" && name.trim() === "")
                }
              >
                {busy ? "Working…" : mode === "drop" ? "Drop database" : mode === "rename" ? "Rename" : "Create"}
              </button>
            </div>
            {mode === "drop" && backupFirst && (
              <p className="note">The backup is taken first; if it fails, nothing is dropped.</p>
            )}
          </form>
        )}
      </div>
    </div>
  );
}

/** Context-menu actions on database nodes (v1.1 Epic 4). */
export function registerDatabaseActions() {
  const tagOf = (connectionId: string) =>
    useConnections.getState().entries.find((e) => e.id === connectionId)?.env_tag;
  const openFor = (mode: Mode, t: NodeTarget & { type: "database" }) =>
    useDbOps.getState().open({
      mode,
      connectionId: t.connectionId,
      database: t.db.name,
      envTag: tagOf(t.connectionId),
    });
  // The core refuses a non-Local/Dev target regardless; naming the reason here saves a click.
  const tagReason = (t: NodeTarget) => {
    const tag = t.type === "database" ? tagOf(t.connectionId) : undefined;
    return tag === undefined || tag === "local" || tag === "dev"
      ? null
      : `tagged ${tag} — destructive actions need Local or Dev`;
  };

  registerNodeAction({
    id: "db-create",
    label: "New database…",
    applies: (t) => t.type === "database",
    disabledReason: (t) => tagReason(t),
    run: (t) => openFor("create", t as NodeTarget & { type: "database" }),
  });
  registerNodeAction({
    id: "db-rename",
    label: "Rename database…",
    applies: (t) => t.type === "database",
    disabledReason: (t) => tagReason(t),
    run: (t) => openFor("rename", t as NodeTarget & { type: "database" }),
  });
  registerNodeAction({
    id: "db-drop",
    label: "Drop database…",
    applies: (t) => t.type === "database",
    disabledReason: (t) => tagReason(t),
    run: (t) => openFor("drop", t as NodeTarget & { type: "database" }),
  });
}
