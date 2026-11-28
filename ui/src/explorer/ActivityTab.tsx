// Live Activity & Locks Monitor tab: pg_stat_activity sessions, pg_blocking_pids blocking
// tree, pg_locks table, auto-refresh, and one-click Cancel / Terminate backend PID.

import { useCallback, useEffect, useMemo, useState } from "react";
import { useWorkspace, type Tab } from "../app/workspace";
import { TagPill } from "../connection/EnvTag";
import { ErrorNote } from "../connection/ErrorNote";
import { useConnections } from "../connection/store";
import { openEditorTab } from "../editor/EditorTab";
import * as act from "../lib/ipc/activity";
import { toAppError, type AppError } from "../lib/ipc/errors";
import { registerNodeAction } from "./actions";

export interface ActivityTabData {
  connectionId: string;
  database?: string;
}

export function openActivityTab(connectionId: string, database?: string) {
  const entry = useConnections.getState().entries.find((e) => e.id === connectionId);
  const label = entry?.name ?? connectionId;
  useWorkspace.getState().openTab({
    id: database ? `activity:${connectionId}:${database}` : `activity:${connectionId}`,
    kind: "activity",
    title: database ? `Activity: ${label}/${database}` : `Activity: ${label}`,
    connectionId,
    tag: entry?.env_tag,
    data: { connectionId, database } satisfies ActivityTabData,
  });
}

export function registerActivityActions() {
  registerNodeAction({
    id: "activity",
    label: "Activity & locks",
    group: "monitor",
    applies: (t) => t.type === "database",
    run: (t) => {
      if (t.type === "database") openActivityTab(t.connectionId, t.db.name);
    },
  });
}

function fmtSecs(secs?: number | null): string {
  if (secs == null || !Number.isFinite(secs) || secs < 0) return "—";
  if (secs < 1) return `${Math.round(secs * 1000)} ms`;
  if (secs < 60) return `${secs.toFixed(1)} s`;
  if (secs < 3600) {
    const m = Math.floor(secs / 60);
    const s = Math.round(secs % 60);
    return `${m}m ${s}s`;
  }
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  return `${h}h ${m}m`;
}

interface BlockingNode {
  session: act.SessionInfo;
  waiters: BlockingNode[];
}

function buildBlockingForest(sessions: act.SessionInfo[]): BlockingNode[] {
  const byPid = new Map<number, act.SessionInfo>();
  for (const s of sessions) byPid.set(s.pid, s);

  const childrenMap = new Map<number, act.SessionInfo[]>();
  const blockedPids = new Set<number>();
  const blockerPids = new Set<number>();

  for (const s of sessions) {
    if (s.blocking_pids.length === 0) continue;
    blockedPids.add(s.pid);
    for (const bp of s.blocking_pids) {
      blockerPids.add(bp);
      const list = childrenMap.get(bp) ?? [];
      if (!list.some((x) => x.pid === s.pid)) list.push(s);
      childrenMap.set(bp, list);
    }
  }

  // Root blockers: PIDs blocking others that are not themselves blocked (plus cycle fallback)
  let roots = Array.from(blockerPids).filter((pid) => !blockedPids.has(pid));
  if (roots.length === 0 && blockerPids.size > 0) {
    roots = [Array.from(blockerPids).sort((a, b) => a - b)[0]];
  }

  const build = (pid: number, visited: Set<number>): BlockingNode | null => {
    const session = byPid.get(pid);
    if (!session || visited.has(pid)) return null;
    const nextVisited = new Set(visited);
    nextVisited.add(pid);
    const waiters = (childrenMap.get(pid) ?? [])
      .map((child) => build(child.pid, nextVisited))
      .filter((n): n is BlockingNode => n !== null);
    return { session, waiters };
  };

  return roots
    .sort((a, b) => a - b)
    .map((pid) => build(pid, new Set()))
    .filter((n): n is BlockingNode => n !== null);
}

function BlockingTreeNode({
  node,
  isRoot,
  busyPid,
  onSelect,
  onCancel,
  onAskTerminate,
}: {
  node: BlockingNode;
  isRoot: boolean;
  busyPid: number | null;
  onSelect: (s: act.SessionInfo) => void;
  onCancel: (s: act.SessionInfo) => void;
  onAskTerminate: (s: act.SessionInfo) => void;
}) {
  const s = node.session;
  return (
    <li className="block-node">
      <div className="block-card" data-root={isRoot || undefined} onClick={() => onSelect(s)}>
        <div className="block-card-head">
          <span className="plan-badge" data-analyze={isRoot || undefined}>
            {isRoot ? "ROOT BLOCKER" : "WAITING"}
          </span>
          <b className="mono">PID {s.pid}</b>
          <span>
            {s.username || "—"}@{s.database || "—"}
          </span>
          <span className="act-state-pill" data-state={s.state}>
            {s.state || "unknown"}
          </span>
          {s.wait_event && (
            <span className="muted mono">
              wait: {s.wait_event_type}:{s.wait_event}
            </span>
          )}
          <span className="muted mono">{fmtSecs(s.query_duration_secs ?? s.xact_duration_secs)}</span>
          <span className="spacer" />
          {!s.is_self && (
            <div className="act-row-btns" onClick={(e) => e.stopPropagation()}>
              <button type="button" disabled={busyPid === s.pid} onClick={() => onCancel(s)}>
                Cancel query
              </button>
              <button type="button" className="danger" disabled={busyPid === s.pid} onClick={() => onAskTerminate(s)}>
                Terminate
              </button>
            </div>
          )}
        </div>
        {s.query && <pre className="mono block-sql">{s.query}</pre>}
      </div>
      {node.waiters.length > 0 && (
        <ul className="plan-children">
          {node.waiters.map((w) => (
            <BlockingTreeNode
              key={w.session.pid}
              node={w}
              isRoot={false}
              busyPid={busyPid}
              onSelect={onSelect}
              onCancel={onCancel}
              onAskTerminate={onAskTerminate}
            />
          ))}
        </ul>
      )}
    </li>
  );
}

export function ActivityTab({ tab }: { tab: Tab }) {
  const data = tab.data as ActivityTabData;
  const entry = useConnections((s) => s.entries.find((e) => e.id === data.connectionId));

  const [snapshot, setSnapshot] = useState<act.ActivitySnapshot | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<AppError | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [view, setView] = useState<"sessions" | "blocking" | "locks">("sessions");
  const [allDbs, setAllDbs] = useState(!data.database);
  const [search, setSearch] = useState("");
  const [stateFilter, setStateFilter] = useState<"all" | "active" | "idle_in_txn" | "blocked">("all");
  const [hideIdle, setHideIdle] = useState(false);
  const [waitingLocksOnly, setWaitingLocksOnly] = useState(false);
  const [refreshSec, setRefreshSec] = useState<number>(0);
  const [selectedPid, setSelectedPid] = useState<number | null>(null);
  const [busyPid, setBusyPid] = useState<number | null>(null);
  const [confirmTerminate, setConfirmTerminate] = useState<act.SessionInfo | null>(null);

  const dbFilter = allDbs ? null : (data.database ?? null);

  const refresh = useCallback(async () => {
    setError(null);
    try {
      const snap = await act.activitySnapshot(data.connectionId, dbFilter);
      setSnapshot(snap);
    } catch (e) {
      setError(toAppError(e));
    } finally {
      setLoading(false);
    }
  }, [data.connectionId, dbFilter]);

  useEffect(() => {
    setLoading(true);
    void refresh();
  }, [refresh]);

  useEffect(() => {
    if (refreshSec <= 0) return;
    const id = setInterval(() => {
      void refresh();
    }, refreshSec * 1000);
    return () => clearInterval(id);
  }, [refreshSec, refresh]);

  const handleCancel = async (s: act.SessionInfo) => {
    setBusyPid(s.pid);
    setError(null);
    setNotice(null);
    try {
      const sent = await act.cancelBackend(data.connectionId, s.pid);
      setNotice(sent ? `Cancel signal sent to PID ${s.pid}.` : `Could not cancel PID ${s.pid} (already finished or insufficient privileges).`);
      await refresh();
    } catch (e) {
      setError(toAppError(e));
    } finally {
      setBusyPid(null);
    }
  };

  const handleTerminate = async (s: act.SessionInfo) => {
    setBusyPid(s.pid);
    setError(null);
    setNotice(null);
    try {
      const sent = await act.terminateBackend(data.connectionId, s.pid);
      setConfirmTerminate(null);
      setNotice(sent ? `Terminated backend PID ${s.pid}.` : `Could not terminate PID ${s.pid} (already exited or insufficient privileges).`);
      await refresh();
    } catch (e) {
      setError(toAppError(e));
    } finally {
      setBusyPid(null);
    }
  };

  const sessions = snapshot?.sessions ?? [];
  const locks = snapshot?.locks ?? [];

  const filteredSessions = useMemo(() => {
    const q = search.trim().toLowerCase();
    return sessions.filter((s) => {
      if (hideIdle && s.state === "idle") return false;
      if (stateFilter === "active" && s.state !== "active") return false;
      if (stateFilter === "idle_in_txn" && !s.state.startsWith("idle in transaction")) return false;
      if (stateFilter === "blocked" && s.blocking_pids.length === 0) return false;
      if (!q) return true;
      return (
        String(s.pid).includes(q) ||
        s.database.toLowerCase().includes(q) ||
        s.username.toLowerCase().includes(q) ||
        s.application_name.toLowerCase().includes(q) ||
        s.wait_event.toLowerCase().includes(q) ||
        s.query.toLowerCase().includes(q)
      );
    });
  }, [sessions, search, stateFilter, hideIdle]);

  const filteredLocks = useMemo(() => {
    const q = search.trim().toLowerCase();
    return locks.filter((l) => {
      if (waitingLocksOnly && l.granted) return false;
      if (!q) return true;
      return (
        String(l.pid).includes(q) ||
        l.database.toLowerCase().includes(q) ||
        l.relation.toLowerCase().includes(q) ||
        l.lock_type.toLowerCase().includes(q) ||
        l.mode.toLowerCase().includes(q)
      );
    });
  }, [locks, search, waitingLocksOnly]);

  const blockingForest = useMemo(() => buildBlockingForest(sessions), [sessions]);
  const waitingLockCount = useMemo(() => locks.filter((l) => !l.granted).length, [locks]);
  const selectedSession = useMemo(
    () => sessions.find((s) => s.pid === selectedPid) ?? null,
    [sessions, selectedPid],
  );

  const isRestricted = entry?.env_tag === "staging" || entry?.env_tag === "production";

  return (
    <div className="activity-tab" data-tag={entry?.env_tag} aria-label="Activity and locks monitor">
      <header className="activity-header">
        <div className="activity-title">
          <h2>
            Activity &amp; Locks — {entry?.name ?? data.connectionId}
            {!allDbs && data.database ? `/${data.database}` : " (all databases)"}
          </h2>
          {entry && <TagPill tag={entry.env_tag} />}
          {snapshot?.server_version && <span className="muted">PG {snapshot.server_version}</span>}
        </div>
        <div className="activity-controls">
          <button type="button" onClick={() => void refresh()} disabled={loading}>
            Refresh
          </button>
          <label className="act-interval">
            Auto-refresh
            <select
              aria-label="Auto-refresh interval"
              value={refreshSec}
              onChange={(e) => setRefreshSec(Number(e.target.value))}
            >
              <option value={0}>Off</option>
              <option value={2}>Every 2s</option>
              <option value={5}>Every 5s</option>
              <option value={10}>Every 10s</option>
            </select>
          </label>
        </div>
      </header>

      {snapshot && (
        <div className="activity-kpis" role="region" aria-label="Activity summary">
          <div className="act-kpi">
            <span className="muted">Sessions</span>
            <b>{snapshot.total_count}</b>
          </div>
          <div className="act-kpi" data-highlight={snapshot.active_count > 0 ? "ok" : undefined}>
            <span className="muted">Active</span>
            <b>{snapshot.active_count}</b>
          </div>
          <div className="act-kpi" data-highlight={snapshot.idle_in_txn_count > 0 ? "warn" : undefined}>
            <span className="muted">Idle in txn</span>
            <b>{snapshot.idle_in_txn_count}</b>
          </div>
          <div className="act-kpi" data-highlight={snapshot.blocked_count > 0 ? "danger" : undefined}>
            <span className="muted">Blocked</span>
            <b>{snapshot.blocked_count}</b>
          </div>
          <div className="act-kpi" data-highlight={waitingLockCount > 0 ? "danger" : undefined}>
            <span className="muted">Waiting locks</span>
            <b>{waitingLockCount}</b>
          </div>
        </div>
      )}

      <div className="activity-toolbar">
        <div className="results-tabs act-subtabs" role="tablist" aria-label="Activity views">
          <button type="button" role="tab" aria-selected={view === "sessions"} onClick={() => setView("sessions")}>
            Sessions ({filteredSessions.length})
          </button>
          <button type="button" role="tab" aria-selected={view === "blocking"} onClick={() => setView("blocking")}>
            Blocking tree{snapshot && snapshot.blocked_count > 0 ? ` (${snapshot.blocked_count})` : ""}
          </button>
          <button type="button" role="tab" aria-selected={view === "locks"} onClick={() => setView("locks")}>
            Locks ({filteredLocks.length})
          </button>
        </div>
        <input
          type="search"
          aria-label="Filter activity"
          placeholder="Filter PID, user, database, relation, SQL…"
          value={search}
          onChange={(e) => setSearch(e.target.value)}
        />
        {view === "sessions" && (
          <>
            <select
              aria-label="Filter by state"
              value={stateFilter}
              onChange={(e) => setStateFilter(e.target.value as typeof stateFilter)}
            >
              <option value="all">All states</option>
              <option value="active">Active</option>
              <option value="idle_in_txn">Idle in transaction</option>
              <option value="blocked">Blocked only</option>
            </select>
            <label className="check">
              <input type="checkbox" checked={hideIdle} onChange={(e) => setHideIdle(e.target.checked)} />
              Hide idle
            </label>
          </>
        )}
        {view === "locks" && (
          <label className="check">
            <input type="checkbox" checked={waitingLocksOnly} onChange={(e) => setWaitingLocksOnly(e.target.checked)} />
            Waiting only
          </label>
        )}
        {data.database && (
          <label className="check">
            <input type="checkbox" checked={allDbs} onChange={(e) => setAllDbs(e.target.checked)} />
            All databases
          </label>
        )}
      </div>

      {notice && (
        <div className="ok-note act-banner" role="status">
          {notice}
        </div>
      )}
      {error && <ErrorNote error={error} />}

      <div className="activity-body">
        {loading && !snapshot ? (
          <p className="empty">Loading server activity…</p>
        ) : view === "sessions" ? (
          <table className="details-table act-table" aria-label="Active sessions">
            <thead>
              <tr>
                <th>PID</th>
                <th>Database</th>
                <th>User</th>
                <th>State</th>
                <th>Wait</th>
                <th>Duration</th>
                <th>Blocked by</th>
                <th>Query</th>
                <th>Actions</th>
              </tr>
            </thead>
            <tbody>
              {filteredSessions.length === 0 ? (
                <tr>
                  <td colSpan={9} className="muted">
                    No matching sessions.
                  </td>
                </tr>
              ) : (
                filteredSessions.map((s) => (
                  <tr
                    key={s.pid}
                    className="act-row"
                    data-selected={selectedPid === s.pid || undefined}
                    data-blocked={s.blocking_pids.length > 0 || undefined}
                    onClick={() => setSelectedPid(s.pid)}
                  >
                    <td className="mono">
                      {s.pid}
                      {s.is_self && <span className="plan-chip">self</span>}
                    </td>
                    <td className="mono">{s.database || "—"}</td>
                    <td>{s.username || "—"}</td>
                    <td>
                      <span className="act-state-pill" data-state={s.state}>
                        {s.state || s.backend_type || "—"}
                      </span>
                    </td>
                    <td className="mono muted">{s.wait_event ? `${s.wait_event_type}:${s.wait_event}` : "—"}</td>
                    <td className="mono">{fmtSecs(s.query_duration_secs)}</td>
                    <td className="mono">
                      {s.blocking_pids.length > 0 ? (
                        <span className="error-text">{s.blocking_pids.join(", ")}</span>
                      ) : (
                        "—"
                      )}
                    </td>
                    <td className="mono act-query-cell" title={s.query}>
                      {s.query.replace(/\s+/g, " ").slice(0, 120) || "—"}
                    </td>
                    <td className="act-actions-cell" onClick={(e) => e.stopPropagation()}>
                      {!s.is_self && (
                        <div className="act-row-btns">
                          <button
                            type="button"
                            aria-label={`Cancel PID ${s.pid}`}
                            disabled={busyPid === s.pid || s.state === "idle"}
                            onClick={() => void handleCancel(s)}
                          >
                            Cancel
                          </button>
                          <button
                            type="button"
                            className="danger"
                            aria-label={`Terminate PID ${s.pid}`}
                            disabled={busyPid === s.pid}
                            onClick={() => setConfirmTerminate(s)}
                          >
                            Terminate
                          </button>
                        </div>
                      )}
                    </td>
                  </tr>
                ))
              )}
            </tbody>
          </table>
        ) : view === "blocking" ? (
          blockingForest.length === 0 ? (
            <div className="ok-note act-empty-block" role="status">
              No blocked sessions — no lock contention detected.
            </div>
          ) : (
            <ul className="plan-tree" aria-label="Blocking tree">
              {blockingForest.map((root) => (
                <BlockingTreeNode
                  key={root.session.pid}
                  node={root}
                  isRoot
                  busyPid={busyPid}
                  onSelect={(s) => setSelectedPid(s.pid)}
                  onCancel={(s) => void handleCancel(s)}
                  onAskTerminate={(s) => setConfirmTerminate(s)}
                />
              ))}
            </ul>
          )
        ) : (
          <table className="details-table act-table" aria-label="Database locks">
            <thead>
              <tr>
                <th>PID</th>
                <th>Database</th>
                <th>Relation</th>
                <th>Lock type</th>
                <th>Mode</th>
                <th>Status</th>
                <th>Virtual XID</th>
              </tr>
            </thead>
            <tbody>
              {filteredLocks.length === 0 ? (
                <tr>
                  <td colSpan={7} className="muted">
                    No matching locks.
                  </td>
                </tr>
              ) : (
                filteredLocks.map((l, idx) => (
                  <tr key={`${l.pid}-${l.lock_type}-${l.relation}-${idx}`} data-blocked={!l.granted || undefined}>
                    <td className="mono">{l.pid}</td>
                    <td className="mono">{l.database || "—"}</td>
                    <td className="mono">{l.relation || "—"}</td>
                    <td className="mono">{l.lock_type}</td>
                    <td className="mono">{l.mode}</td>
                    <td>
                      <span className={l.granted ? "act-state-pill" : "plan-badge"} data-analyze={!l.granted || undefined}>
                        {l.granted ? "granted" : "WAITING"}
                      </span>
                    </td>
                    <td className="mono muted">{l.virtual_xid || "—"}</td>
                  </tr>
                ))
              )}
            </tbody>
          </table>
        )}
      </div>

      {selectedSession && (
        <section className="activity-inspector" aria-label="Selected session details">
          <div className="activity-inspector-head">
            <b className="mono">PID {selectedSession.pid}</b>
            <span className="muted">
              {selectedSession.username || "—"}@{selectedSession.database || "—"}
              {selectedSession.application_name && ` · app: ${selectedSession.application_name}`}
              {selectedSession.client_addr && ` · client: ${selectedSession.client_addr}`}
              {selectedSession.xact_duration_secs != null && ` · xact: ${fmtSecs(selectedSession.xact_duration_secs)}`}
            </span>
            <span className="spacer" />
            {selectedSession.query && (
              <button
                type="button"
                onClick={() =>
                  openEditorTab(
                    data.connectionId,
                    selectedSession.database || data.database || entry?.database || "postgres",
                    { title: `PID ${selectedSession.pid}`, text: selectedSession.query },
                  )
                }
              >
                Open SQL in editor
              </button>
            )}
            <button type="button" className="icon" aria-label="Close session inspector" onClick={() => setSelectedPid(null)}>
              ✕
            </button>
          </div>
          <pre className="mono activity-inspector-sql">{selectedSession.query || "(no query text)"}</pre>
        </section>
      )}

      {confirmTerminate && (
        <div className="modal-backdrop" role="presentation" onClick={() => setConfirmTerminate(null)}>
          <div
            className="modal"
            role="dialog"
            aria-modal="true"
            aria-label="Confirm terminate backend"
            onClick={(e) => e.stopPropagation()}
          >
            <h2>Terminate backend PID {confirmTerminate.pid}?</h2>
            <p>
              This calls <code className="mono">pg_terminate_backend({confirmTerminate.pid})</code> on{" "}
              <b>{entry?.name ?? data.connectionId}</b> ({confirmTerminate.username}@{confirmTerminate.database || "server"}), forcefully disconnecting the session and rolling back its open transaction.
            </p>
            {isRestricted && (
              <div className="warn-note" role="alert">
                <strong>{entry?.env_tag === "production" ? "Production" : "Staging"} server:</strong> terminating a backend immediately aborts in-flight client work.
              </div>
            )}
            {confirmTerminate.query && (
              <pre className="mono restore-log">{confirmTerminate.query}</pre>
            )}
            <div className="modal-actions">
              <button type="button" onClick={() => setConfirmTerminate(null)} disabled={busyPid === confirmTerminate.pid}>
                Cancel
              </button>
              <button
                type="button"
                className="danger"
                disabled={busyPid === confirmTerminate.pid}
                onClick={() => void handleTerminate(confirmTerminate)}
              >
                Confirm terminate
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
