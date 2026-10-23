// One timeline for everything that can act on a database: Refresh runs and the one-off
// operations (create/rename/drop/restore). Rows that own a backup can hand it straight to the
// restore dialog — the recovery path when a run failed but its dump is already there.

import { useEffect, useState } from "react";
import { useWorkspace } from "../app/workspace";
import { useConnections } from "../connection/store";
import { ErrorNote } from "../connection/ErrorNote";
import { useRestore } from "../explorer/RestoreDialog";
import { toAppError, type AppError } from "../lib/ipc/errors";
import * as r from "../lib/ipc/runs";
import { openOperationTab } from "./OperationView";
import { openRunTab } from "./RunTab";

export function openHistoryView() {
  useWorkspace.getState().openTab({ id: "history", kind: "history", title: "History" });
}

const KIND_LABEL: Record<string, string> = {
  refresh: "Refresh",
  create: "Create DB",
  rename: "Rename DB",
  drop: "Drop DB",
  backup: "Backup",
  restore: "Restore",
};

export function HistoryView() {
  const [entries, setEntries] = useState<r.HistoryEntry[] | null>(null);
  const [error, setError] = useState<AppError | null>(null);
  const load = () => {
    setError(null);
    void r
      .historyList()
      .then(setEntries)
      .catch((e) => setError(toAppError(e)));
  };
  useEffect(load, []);
  // Tabs stay mounted, so switching back to History must refresh it (a backup or a restore may
  // have happened since it was opened).
  const activeTabId = useWorkspace((s) => s.activeTabId);
  useEffect(() => {
    if (activeTabId === "history") load();
  }, [activeTabId]); // eslint-disable-line react-hooks/exhaustive-deps

  const restore = (entry: r.HistoryEntry) => {
    if (!entry.backup) return;
    const connectionId = entry.backup.connection.id;
    useRestore.getState().open({
      connectionId,
      database: entry.backup.database,
      envTag: useConnections.getState().entries.find((c) => c.id === connectionId)?.env_tag,
      archive: entry.backup.path,
    });
  };

  return (
    <div className="run-view" aria-label="History">
      <header className="run-header">
        <h2>History</h2>
        <button type="button" onClick={load}>
          Refresh
        </button>
      </header>
      {error && <ErrorNote error={error} />}
      {entries && entries.length === 0 && <p className="muted">Nothing has run yet.</p>}
      {entries && entries.length > 0 && (
        <table className="details-table history-table">
          <thead>
            <tr>
              <th>Started</th>
              <th>What</th>
              <th>Target</th>
              <th>Status</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {entries.map((e) => (
              <tr key={e.id} data-kind={e.kind} data-id={e.id} data-unreadable={e.unreadable || undefined}>
                <td>{e.started_at ? new Date(e.started_at).toLocaleString() : "—"}</td>
                <td>
                  <span className="history-kind" data-kind={e.unreadable ? "unreadable" : e.kind}>
                    {e.unreadable ? "Unreadable" : (KIND_LABEL[e.kind] ?? e.kind)}
                  </span>{" "}
                  {e.name}
                  {e.headline && <div className="muted">{e.headline}</div>}
                </td>
                <td className="muted">{e.target ?? "—"}</td>
                <td>
                  <span className="run-status" data-status={e.status}>
                    {e.status.replace(/_/g, " ")}
                  </span>
                </td>
                <td>
                  <div className="history-actions">
                    {e.unreadable ? null : e.kind === "refresh" && e.workflow_id ? (
                      <button
                        type="button"
                        onClick={() =>
                          openRunTab({
                            workflowId: e.workflow_id!,
                            workflowName: e.workflow_name ?? e.name,
                            runId: e.id,
                          })
                        }
                      >
                        Open
                      </button>
                    ) : (
                      <button type="button" onClick={() => openOperationTab(e.id, e.name)}>
                        Open
                      </button>
                    )}
                    {e.backup && (
                      <button
                        type="button"
                        title={`Restore ${e.backup.path}`}
                        onClick={() => restore(e)}
                      >
                        Restore backup…
                      </button>
                    )}
                  </div>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}
