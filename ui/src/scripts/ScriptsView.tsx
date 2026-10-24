// Scripts sidebar: recents one click away, and the searchable on-disk library
// grouped by folder with tags. Rescans when the window regains focus so external edits,
// renames and git operations show up. "Open…" brings in a .sql / .txt / .md file from
// anywhere on disk; its tab offers "Add to library" to keep it for later.

import { useCallback, useEffect, useMemo, useState } from "react";
import * as ipc from "../lib/ipc/scripts";
import { toAppError, type AppError } from "../lib/ipc/errors";
import { ErrorNote } from "../connection/ErrorNote";
import { useConnections } from "../connection/store";
import { fileName, isNote, openFromDisk, openLibraryScript, openRecent } from "./open";

const kindGlyph = (path: string) => (isNote(path) ? "¶" : "⌗");

function ago(ms: number): string {
  const s = (Date.now() - ms) / 1000;
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.floor(s / 60)} min ago`;
  if (s < 86400) return `${Math.floor(s / 3600)} h ago`;
  return `${Math.floor(s / 86400)} d ago`;
}

export function ScriptsView() {
  const [query, setQuery] = useState("");
  const [lib, setLib] = useState<ipc.LibrarySnapshot | null>(null);
  const [recents, setRecents] = useState<ipc.RecentStatus[]>([]);
  const [error, setError] = useState<AppError | null>(null);
  const entries = useConnections((s) => s.entries);

  const refresh = useCallback(async (q = query) => {
    try {
      setLib(await ipc.libraryList(q));
      setRecents(await ipc.scriptRecents());
    } catch (e) {
      setError(toAppError(e));
    }
  }, [query]);

  useEffect(() => {
    void refresh(query);
  }, [query]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    const onFocus = () => void ipc.libraryRescan().then(() => refresh()).catch(() => {});
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  }, [refresh]);

  const folders = useMemo(() => {
    const m = new Map<string, ipc.ScriptInfo[]>();
    for (const s of lib?.scripts ?? []) m.set(s.folder, [...(m.get(s.folder) ?? []), s]);
    return [...m.entries()].sort(([a], [b]) => a.localeCompare(b));
  }, [lib]);

  const open = (f: () => Promise<unknown>) => {
    setError(null);
    void f().then(() => refresh()).catch((e) => setError(toAppError(e)));
  };
  const connName = (id?: string) => entries.find((e) => e.id === id)?.name;

  return (
    <div className="scripts-view">
      <div className="search-box">
        <input type="search" placeholder="Search scripts and tags" aria-label="Search scripts" value={query} onChange={(e) => setQuery(e.target.value)} />
        <button type="button" onClick={() => open(openFromDisk)} title="Open a .sql, .txt or .md file">
          Open…
        </button>
      </div>
      {error && <ErrorNote error={error} compact />}
      {!query && recents.length > 0 && (
        <section aria-label="Recent scripts">
          <header className="group-header">Recent</header>
          <ul className="script-list">
            {recents.slice(0, 10).map((r) => {
              const name = fileName(r.resolved_path);
              return (
                <li key={r.entry.path} data-state={r.state}>
                  {r.state === "available" ? (
                    <button type="button" className="script-item" onClick={() => open(() => openRecent(r))} title={r.resolved_path}>
                      <span className="script-kind" aria-hidden="true">{kindGlyph(name)}</span>
                      <span className="script-name">{name}</span>
                      <span className="script-meta">
                        {ago(r.entry.last_used_ms)}
                        {r.entry.connection_id && ` · ${connName(r.entry.connection_id) ?? "?"}${r.entry.database ? `/${r.entry.database}` : ""}`}
                      </span>
                    </button>
                  ) : (
                    <div className="script-item missing">
                      <span className="script-name">{name}</span>
                      <span className="script-meta">not found</span>
                      <button type="button" className="icon" aria-label={`Remove ${name} from recents`} onClick={() => open(() => ipc.removeRecent(r.entry.path))}>
                        ✕
                      </button>
                    </div>
                  )}
                </li>
              );
            })}
          </ul>
        </section>
      )}
      <section aria-label="Library">
        <header className="group-header" title={lib?.root}>
          Library
        </header>
        {lib && lib.scripts.length === 0 && (
          <p className="empty">{query ? "No scripts match." : "No saved scripts yet. Save an editor tab (Ctrl+S), or open a .sql, .txt or .md file and add it to the library."}</p>
        )}
        {folders.map(([folder, scripts]) => (
          <div key={folder || "/"} className="script-folder">
            {folder && <div className="folder-name">▸ {folder}</div>}
            <ul className="script-list">
              {scripts.map((s) => (
                <li key={s.id} data-missing={s.missing || undefined}>
                  <button type="button" className="script-item" disabled={s.missing} onClick={() => open(() => openLibraryScript(s.id))} title={s.path}>
                    <span className="script-kind" aria-hidden="true">{kindGlyph(s.path)}</span>
                    <span className="script-name">{s.name}</span>
                    {s.missing && <span className="script-meta">missing on disk</span>}
                    {s.tags.map((t) => (
                      <span key={t} className="script-tag">
                        {t}
                      </span>
                    ))}
                  </button>
                </li>
              ))}
            </ul>
          </div>
        ))}
      </section>
    </div>
  );
}
