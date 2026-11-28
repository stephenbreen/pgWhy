// One-off operation report (create/rename/drop/restore): steps, warnings and the error, in the
// same shape the dialogs show right after the action.

import { useEffect, useState } from "react";
import { useWorkspace, type Tab } from "../app/workspace";
import { ErrorNote } from "../connection/ErrorNote";
import type { OperationRecord } from "../lib/ipc/databases";
import { toAppError, type AppError } from "../lib/ipc/errors";
import * as r from "../lib/ipc/runs";
import { useTrees } from "../explorer/tree/model";

export function openOperationTab(id: string, name: string) {
  useWorkspace.getState().openTab({
    id: `operation:${id}`,
    kind: "operation",
    title: name.length > 40 ? `${name.slice(0, 39)}…` : name,
    data: { id },
  });
}

export function OperationView({ tab }: { tab: Tab }) {
  const { id } = tab.data as { id: string };
  const [rec, setRec] = useState<OperationRecord | null>(null);
  const [error, setError] = useState<AppError | null>(null);
  // what can still be done about a restore that stopped part-way, per the server as it is now
  const [actions, setActions] = useState<r.ActionOffer[]>([]);
  const [busy, setBusy] = useState(false);
  const loadActions = (rec: OperationRecord) => {
    if (rec.kind !== "restore") return;
    void r
      .operationActions(rec.id)
      .then(setActions)
      .catch(() => setActions([])); // still running, or the server is unreachable: nothing to offer
  };
  useEffect(() => {
    void r
      .operationGet(id)
      .then((rec) => {
        setRec(rec);
        loadActions(rec);
      })
      .catch((e) => setError(toAppError(e)));
  }, [id]);

  const act = async (a: r.ActionOffer) => {
    setError(null);
    setBusy(true);
    try {
      const next = await r.operationAction(id, a.kind);
      setRec(next);
      const conn = next.detail?.target?.connection_id;
      if (conn) useTrees.getState().invalidate(conn);
      loadActions(next);
    } catch (e) {
      setError(toAppError(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="run-view" aria-label="Operation report">
      <header className="run-header">
        <h2>{tab.title}</h2>
      </header>
      {error && <ErrorNote error={error} />}
      {rec && (
        <>
          <p>
            <span className="run-status" data-status={rec.status}>
              {rec.status.replace(/_/g, " ")}
            </span>{" "}
            <span className="muted">
              started {new Date(rec.started_at).toLocaleString()}
              {rec.finished_at ? ` · finished ${new Date(rec.finished_at).toLocaleString()}` : ""}
            </span>
          </p>
          {rec.error && <ErrorNote error={rec.error as unknown as AppError} />}
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
          {actions.length > 0 && (
            <div className="run-actions" role="group" aria-label="Actions">
              {actions.map((a) => (
                <button key={`${a.kind}:${a.database ?? ""}`} type="button" disabled={!a.enabled || busy} title={a.disabled_reason ?? undefined} onClick={() => void act(a)}>
                  {a.label}
                </button>
              ))}
            </div>
          )}
          <p className="note">Record id {rec.id}</p>
        </>
      )}
    </div>
  );
}
