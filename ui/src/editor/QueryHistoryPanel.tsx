// Searchable Query Execution History panel shown inside the editor results pane.

import { useMemo, useState } from "react";
import { useQueryHistory, type QueryHistoryEntry } from "./history";

function ago(ms: number): string {
  const s = Math.max(0, (Date.now() - ms) / 1000);
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}

function kindLabel(kind: QueryHistoryEntry["kind"]): string | null {
  if (kind === "explain") return "EXPLAIN";
  if (kind === "explain_analyze") return "ANALYZE";
  return null;
}

export function QueryHistoryPanel({
  connectionId,
  database,
  onLoadSql,
  onOpenNewTab,
}: {
  connectionId: string;
  database: string;
  onLoadSql: (sql: string) => void;
  onOpenNewTab: (entry: QueryHistoryEntry) => void;
}) {
  const entries = useQueryHistory((s) => s.entries);
  const remove = useQueryHistory((s) => s.remove);
  const clear = useQueryHistory((s) => s.clear);

  const [search, setSearch] = useState("");
  const [thisDbOnly, setThisDbOnly] = useState(false);

  const filtered = useMemo(() => {
    const q = search.trim().toLowerCase();
    return entries.filter((e) => {
      if (thisDbOnly && (e.connectionId !== connectionId || e.database !== database)) return false;
      if (!q) return true;
      return (
        e.sql.toLowerCase().includes(q) ||
        e.connectionName.toLowerCase().includes(q) ||
        e.database.toLowerCase().includes(q)
      );
    });
  }, [entries, search, thisDbOnly, connectionId, database]);

  return (
    <div className="qhistory-pane" aria-label="Query history">
      <div className="qhistory-toolbar">
        <input
          type="search"
          aria-label="Search query history"
          placeholder="Filter SQL, connection, or database…"
          value={search}
          onChange={(e) => setSearch(e.target.value)}
        />
        <label className="check">
          <input
            type="checkbox"
            checked={thisDbOnly}
            onChange={(e) => setThisDbOnly(e.target.checked)}
          />
          This database only
        </label>
        <span className="spacer" />
        {entries.length > 0 && (
          <button type="button" onClick={clear}>
            Clear history
          </button>
        )}
      </div>
      {filtered.length === 0 ? (
        <p className="empty qhistory-empty">
          {entries.length === 0 ? "No queries executed yet." : "No matching history entries."}
        </p>
      ) : (
        <ul className="qhistory-list">
          {filtered.map((item) => {
            const badge = kindLabel(item.kind);
            const status = item.cancelled ? "cancelled" : item.errors > 0 ? "error" : "ok";
            return (
              <li key={item.id} className="qhistory-row" data-status={status}>
                <div className="qhistory-meta">
                  <span className="qhistory-icon" aria-label={status}>
                    {status === "ok" ? "✓" : status === "cancelled" ? "■" : "✕"}
                  </span>
                  {badge && <span className="plan-chip">{badge}</span>}
                  <span className="muted">
                    {item.connectionName}/{item.database} · {Math.round(item.durationMs)} ms · {ago(item.timestamp)}
                  </span>
                  <span className="spacer" />
                  <div className="qhistory-actions">
                    <button type="button" onClick={() => onLoadSql(item.sql)}>
                      Load
                    </button>
                    <button type="button" onClick={() => onOpenNewTab(item)}>
                      New tab
                    </button>
                    <button
                      type="button"
                      onClick={() => void navigator.clipboard?.writeText(item.sql).catch(() => {})}
                    >
                      Copy
                    </button>
                    <button
                      type="button"
                      className="icon"
                      aria-label="Remove history entry"
                      title="Remove"
                      onClick={() => remove(item.id)}
                    >
                      ✕
                    </button>
                  </div>
                </div>
                <pre className="mono qhistory-sql">{item.sql}</pre>
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}
