// Standalone backup run view: a docked, non-blocking tab with live progress.

import { useState } from "react";
import { useWorkspace, type Tab } from "../app/workspace";
import { useConnections } from "../connection/store";
import { ErrorNote } from "../connection/ErrorNote";
import * as b from "../lib/ipc/backup";
import { toAppError, type AppError } from "../lib/ipc/errors";
import { fmtBytes, fmtElapsed, ProgressLine } from "./progress";

export interface BackupTabData {
  connectionId: string;
  database: string;
  scope: b.BackupScope;
}

function scopeLabel(scope: b.BackupScope): string {
  switch (scope.kind) {
    case "database":
      return "the whole database";
    case "schema":
      return `schema ${scope.schema}`;
    case "tables":
      return scope.tables.map((t) => `${t.schema}.${t.name}`).join(", ");
  }
}

export function openBackup(connectionId: string, database: string, scope: b.BackupScope = { kind: "database" }) {
  const entry = useConnections.getState().entries.find((e) => e.id === connectionId);
  const what = scope.kind === "database" ? database : scopeLabel(scope);
  useWorkspace.getState().openTab({
    id: `backup:${crypto.randomUUID()}`,
    kind: "backup",
    title: `Backup ${what}`,
    connectionId,
    tag: entry?.env_tag,
    data: { connectionId, database, scope } satisfies BackupTabData,
  });
}

export function BackupTab({ tab }: { tab: Tab }) {
  const data = tab.data as BackupTabData;
  const entry = useConnections((s) => s.entries.find((e) => e.id === data.connectionId));
  const [request, setRequest] = useState<b.BackupRequest>(() => b.defaultBackupRequest(data.scope));
  const [started, setStarted] = useState<Extract<b.BackupEvent, { type: "started" }> | null>(null);
  const [progress, setProgress] = useState<b.ToolProgress | null>(null);
  const [done, setDone] = useState<Extract<b.BackupEvent, { type: "finished" }> | null>(null);
  const [error, setError] = useState<AppError | null>(null);
  const [running, setRunning] = useState(false);
  const [startedOnce, setStartedOnce] = useState(false);

  const start = () => {
    setStartedOnce(true);
    setRunning(true);
    setError(null);
    b.backupDatabase(tab.id, data.connectionId, data.database, request, (ev) => {
      if (ev.type === "started") setStarted(ev);
      else if (ev.type === "progress") setProgress(ev.progress);
      else if (ev.type === "finished") setDone(ev);
      else if (ev.type === "failed") setError(ev.error);
    })
      .catch((e) => setError((cur) => cur ?? toAppError(e)))
      .finally(() => setRunning(false));
  };

  return (
    <div className="run-view" aria-label="Backup">
      <header className="run-header">
        <h2>
          Backup of {entry?.name}/{data.database}
        </h2>
        {running && (
          <button type="button" onClick={() => void b.backupCancel(tab.id)}>
            Cancel
          </button>
        )}
      </header>
      {!startedOnce && (
        <form
          className="backup-form"
          onSubmit={(e) => {
            e.preventDefault();
            start();
          }}
        >
          <p className="muted">
            Backing up <b>{scopeLabel(data.scope)}</b> of {entry?.name}/{data.database}.
          </p>
          <label>
            Format
            <select
              aria-label="Backup format"
              value={request.format}
              onChange={(e) => setRequest({ ...request, format: e.target.value as b.BackupFormat })}
            >
              <option value="custom">Custom (single file, restorable, compressed)</option>
              <option value="directory">Directory (parallel, larger)</option>
            </select>
          </label>
          {request.format === "directory" && (
            <label className="narrow">
              Parallel jobs
              <input
                type="number"
                min={1}
                max={32}
                value={request.jobs ?? ""}
                placeholder="default"
                onChange={(e) =>
                  setRequest({ ...request, jobs: e.target.value === "" ? null : Number(e.target.value) })
                }
              />
            </label>
          )}
          <label>
            Contents
            <select
              aria-label="Backup contents"
              value={request.data}
              onChange={(e) => setRequest({ ...request, data: e.target.value as b.BackupData })}
            >
              <option value="all">Schema and data</option>
              <option value="schema_only">Schema only (no rows)</option>
              <option value="data_only">Data only (no DDL)</option>
            </select>
          </label>
          {error && <ErrorNote error={error} />}
          <div className="modal-actions">
            <button type="submit">Back up</button>
          </div>
        </form>
      )}
      {started && (
        <p className="muted">
          {started.pg_dump} · server {started.server_version} · <span className="mono">{started.path}</span>
        </p>
      )}
      {running && !progress && <p>Starting…</p>}
      {progress && !done && <ProgressLine p={progress} />}
      {done && (
        <div className="ok-note" role="status">
          <p>
            Backed up {fmtBytes(done.size_bytes)} in {fmtElapsed(done.duration_ms)} to <span className="mono">{done.path}</span>
          </p>
          {done.removed_by_retention.length > 0 && <p className="muted">Removed {done.removed_by_retention.length} older backup(s) per retention.</p>}
          <button type="button" onClick={() => void b.revealPath(done.path)}>
            Open backup folder
          </button>
        </div>
      )}
      {error && <ErrorNote error={error} />}
    </div>
  );
}
