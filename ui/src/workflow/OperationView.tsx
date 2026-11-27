// One-off operation report (create/rename/drop/restore): steps, warnings and the error, in the
// same shape the dialogs show right after the action.

import { useEffect, useState } from "react";
import { useWorkspace, type Tab } from "../app/workspace";
import { ErrorNote } from "../connection/ErrorNote";
import { useTrees } from "../explorer/tree/model";
import type { OperationRecord } from "../lib/ipc/databases";
import { toAppError, type AppError } from "../lib/ipc/errors";
import * as r from "../lib/ipc/runs";

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
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    void r
      .operationGet(id)
      .then(setRec)
      .catch((e) => setError(toAppError(e)));
  }, [id]);

  const act = async (action: r.ActionKind) => {
    setBusy(true);
    setError(null);
    try {
      const next = await r.operationAction(id, action);
      setRec(next);
      const target = next.detail?.target;
      if (target) useTrees.getState().invalidate(target.connection_id, target.database);
    } catch (e) {
      setError(toAppError(e));
    } finally {
      setBusy(false);
    }
  };

  const swap = rec?.detail?.kind === "restore" && rec.detail.mode === "replace" ? rec.detail.swap : null;
  const inSwapWindow = !!swap && swap.renames_completed === 1 && rec?.status !== "running";
  const canDropOld = !!swap && swap.renames_completed !== 1 && !!swap.old_db && rec?.status === "succeeded_cleanup_required";
  const canDropStaging = !!swap && swap.renames_completed === 0 && !!swap.staging_db && rec?.status !== "running" && rec?.status !== "succeeded";

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
          {(inSwapWindow || canDropOld || canDropStaging) && (
            <div className="run-actions" role="group" aria-label="Recovery actions">
              {inSwapWindow && (
                <>
                  <button type="button" className="primary" disabled={busy} onClick={() => void act("complete_swap")}>
                    Complete swap
                  </button>
                  <button type="button" disabled={busy} onClick={() => void act("rollback_swap")}>
                    Roll back
                  </button>
                </>
              )}
              {canDropOld && (
                <button type="button" disabled={busy} onClick={() => void act("drop_old")}>
                  Drop {swap?.old_db}
                </button>
              )}
              {canDropStaging && (
                <button type="button" disabled={busy} onClick={() => void act("drop_staging")}>
                  Drop {swap?.staging_db}
                </button>
              )}
            </div>
          )}
          <p className="note">Record id {rec.id}</p>
        </>
      )}
    </div>
  );
}
