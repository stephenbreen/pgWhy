// Run history for a workflow: status, timings, and one click into any run's report.

import { useEffect, useState } from "react";
import { useWorkspace, type Tab } from "../app/workspace";
import { ErrorNote } from "../connection/ErrorNote";
import * as r from "../lib/ipc/runs";
import { toAppError, type AppError } from "../lib/ipc/errors";
import { openRunTab } from "./RunTab";

export function openHistory(workflowId: string, workflowName: string) {
  useWorkspace.getState().openTab({ id: `history:${workflowId}`, kind: "workflow-history", title: `History: ${workflowName}`, data: { workflowId, workflowName } });
}

export function HistoryTab({ tab }: { tab: Tab }) {
  const { workflowId, workflowName } = tab.data as { workflowId: string; workflowName: string };
  const [runs, setRuns] = useState<r.RunRecord[] | null>(null);
  const [error, setError] = useState<AppError | null>(null);
  const load = () => void r.listRuns(workflowId).then(setRuns).catch((e) => setError(toAppError(e)));
  useEffect(load, [workflowId]); // eslint-disable-line react-hooks/exhaustive-deps
  return (
    <div className="run-view" aria-label="Run history">
      <header className="run-header">
        <h2>History: {workflowName}</h2>
        <button type="button" onClick={load}>Refresh</button>
      </header>
      {error && <ErrorNote error={error} />}
      {runs && runs.length === 0 && <p className="muted">No runs yet.</p>}
      {runs && runs.length > 0 && (
        <table className="details-table history-table">
          <thead>
            <tr><th>Started</th><th>Status</th><th>Duration</th><th>Backup</th><th /></tr>
          </thead>
          <tbody>
            {runs.map((run) => {
              const ms = run.finished_at ? new Date(run.finished_at).getTime() - new Date(run.started_at).getTime() : null;
              return (
                <tr key={run.run_id}>
                  <td>{new Date(run.started_at).toLocaleString()}</td>
                  <td><span className="run-status" data-status={run.status}>{run.status.replace(/_/g, " ")}</span>{run.retry_of && <span className="muted"> (retry)</span>}</td>
                  <td>{ms != null ? `${Math.round(ms / 1000)} s` : "—"}</td>
                  <td>{run.backup ? `${(run.backup.size_bytes / 1048576).toFixed(1)} MB${run.backup.reusable ? "" : " (not reusable)"}` : "—"}</td>
                  <td>
                    <button type="button" onClick={() => openRunTab({ workflowId, workflowName, runId: run.run_id })}>
                      Open{run.actions.some((a) => a.enabled) ? " · actions" : ""}
                    </button>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      )}
    </div>
  );
}
