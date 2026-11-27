// Scripts sidebar: recents one click away, and the searchable on-disk library
// grouped by collapsible folders with tags and full CRUD (new script/folder, rename/move,
// tag editing, delete with confirmation). Rescans when the window regains focus so external
// edits, renames and git operations show up.

import { useCallback, useEffect, useMemo, useState, type FormEvent } from "react";
import * as ipc from "../lib/ipc/scripts";
import { toAppError, type AppError } from "../lib/ipc/errors";
import { ErrorNote } from "../connection/ErrorNote";
import { useConnections } from "../connection/store";
import { fileName, isNote, openFromDisk, openLibraryScript, openRecent } from "./open";
import { NewScriptModal } from "./ScriptModals";
import { useScriptLibrary } from "./store";

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
  const [activeTag, setActiveTag] = useState<string | null>(null);
  const [lib, setLib] = useState<ipc.LibrarySnapshot | null>(null);
  const [recents, setRecents] = useState<ipc.RecentStatus[]>([]);
  const [collapsed, setCollapsed] = useState<Record<string, boolean>>({});
  const [error, setError] = useState<AppError | null>(null);

  // CRUD modals
  const [newOpen, setNewOpen] = useState(false);
  const [renaming, setRenaming] = useState<ipc.ScriptInfo | null>(null);
  const [renamePath, setRenamePath] = useState("");
  const [tagging, setTagging] = useState<ipc.ScriptInfo | null>(null);
  const [tagsText, setTagsText] = useState("");
  const [deleting, setDeleting] = useState<ipc.ScriptInfo | null>(null);
  const [modalBusy, setModalBusy] = useState(false);
  const [modalError, setModalError] = useState<AppError | null>(null);

  const entries = useConnections((s) => s.entries);
  const libVersion = useScriptLibrary((s) => s.version);

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
  }, [query, libVersion]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    const onFocus = () => void ipc.libraryRescan().then(() => refresh()).catch(() => {});
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  }, [refresh]);

  const visibleScripts = useMemo(() => {
    const all = lib?.scripts ?? [];
    if (!activeTag) return all;
    return all.filter((s) => s.tags.includes(activeTag));
  }, [lib, activeTag]);

  const folders = useMemo(() => {
    const m = new Map<string, ipc.ScriptInfo[]>();
    for (const s of visibleScripts) m.set(s.folder, [...(m.get(s.folder) ?? []), s]);
    return [...m.entries()].sort(([a], [b]) => a.localeCompare(b));
  }, [visibleScripts]);

  const existingFolderNames = useMemo(
    () => Array.from(new Set((lib?.scripts ?? []).map((s) => s.folder).filter(Boolean))).sort(),
    [lib],
  );

  const open = (f: () => Promise<unknown>) => {
    setError(null);
    void f().then(() => refresh()).catch((e) => setError(toAppError(e)));
  };
  const connName = (id?: string) => entries.find((e) => e.id === id)?.name;

  const startRename = (s: ipc.ScriptInfo) => {
    setModalError(null);
    setRenaming(s);
    setRenamePath(s.path);
  };

  const submitRename = async (e: FormEvent) => {
    e.preventDefault();
    if (!renaming || !renamePath.trim()) return;
    setModalBusy(true);
    setModalError(null);
    try {
      await ipc.scriptRename(renaming.id, renamePath.trim());
      useScriptLibrary.getState().bump();
      setRenaming(null);
      await refresh();
    } catch (err) {
      setModalError(toAppError(err));
    } finally {
      setModalBusy(false);
    }
  };

  const startTags = (s: ipc.ScriptInfo) => {
    setModalError(null);
    setTagging(s);
    setTagsText(s.tags.join(", "));
  };

  const submitTags = async (e: FormEvent) => {
    e.preventDefault();
    if (!tagging) return;
    setModalBusy(true);
    setModalError(null);
    try {
      const parsed = Array.from(
        new Set(
          tagsText
            .split(/[,\n]/)
            .map((t) => t.trim())
            .filter(Boolean),
        ),
      );
      await ipc.scriptSetTags(tagging.id, parsed);
      useScriptLibrary.getState().bump();
      setTagging(null);
      await refresh();
    } catch (err) {
      setModalError(toAppError(err));
    } finally {
      setModalBusy(false);
    }
  };

  const confirmDelete = async () => {
    if (!deleting) return;
    setModalBusy(true);
    setModalError(null);
    try {
      await ipc.scriptDelete(deleting.id);
      useScriptLibrary.getState().bump();
      setDeleting(null);
      await refresh();
    } catch (err) {
      setModalError(toAppError(err));
    } finally {
      setModalBusy(false);
    }
  };

  const addTagSuggestion = (tag: string) => {
    const current = tagsText
      .split(/[,\n]/)
      .map((t) => t.trim())
      .filter(Boolean);
    if (!current.includes(tag)) {
      setTagsText([...current, tag].join(", "));
    }
  };

  return (
    <div className="scripts-view">
      <div className="search-box">
        <input
          type="search"
          placeholder="Search scripts and tags"
          aria-label="Search scripts"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
        />
        <button type="button" onClick={() => setNewOpen(true)} title="Create a new library script or folder">
          + New
        </button>
        <button type="button" onClick={() => open(openFromDisk)} title="Open a .sql, .txt or .md file">
          Open…
        </button>
      </div>
      {lib && lib.tags.length > 0 && (
        <div className="script-tag-filter" role="group" aria-label="Filter by tag">
          {lib.tags.map((t) => {
            const active = activeTag === t;
            return (
              <button
                key={t}
                type="button"
                className="script-tag-chip"
                aria-pressed={active}
                data-active={active || undefined}
                onClick={() => setActiveTag(active ? null : t)}
              >
                #{t}
              </button>
            );
          })}
        </div>
      )}
      {error && <ErrorNote error={error} compact />}
      {!query && !activeTag && recents.length > 0 && (
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
        {lib && visibleScripts.length === 0 && (
          <p className="empty">
            {query || activeTag
              ? "No scripts match."
              : "No saved scripts yet. Click + New, save an editor tab (Ctrl+S), or open a .sql, .txt or .md file."}
          </p>
        )}
        {folders.map(([folder, scripts]) => {
          const isCollapsed = !!folder && !!collapsed[folder];
          return (
            <div key={folder || "/"} className="script-folder">
              {folder && (
                <button
                  type="button"
                  className="folder-name folder-toggle"
                  aria-expanded={!isCollapsed}
                  onClick={() => setCollapsed((c) => ({ ...c, [folder]: !c[folder] }))}
                >
                  <span>{isCollapsed ? "▸" : "▾"} {folder}</span>
                  <span className="muted">{scripts.length}</span>
                </button>
              )}
              {!isCollapsed && (
                <ul className="script-list">
                  {scripts.map((s) => (
                    <li key={s.id} className="script-row" data-missing={s.missing || undefined}>
                      <button
                        type="button"
                        className="script-item"
                        disabled={s.missing}
                        onClick={() => open(() => openLibraryScript(s.id))}
                        title={s.path}
                      >
                        <span className="script-kind" aria-hidden="true">{kindGlyph(s.path)}</span>
                        <span className="script-name">{s.name}</span>
                        {s.missing && <span className="script-meta">missing on disk</span>}
                        {s.tags.map((t) => (
                          <span key={t} className="script-tag">
                            {t}
                          </span>
                        ))}
                      </button>
                      <span className="script-row-actions">
                        <button
                          type="button"
                          className="icon"
                          aria-label={`Edit tags for ${s.name}`}
                          title="Edit tags"
                          onClick={() => startTags(s)}
                        >
                          #
                        </button>
                        <button
                          type="button"
                          className="icon"
                          aria-label={`Rename ${s.name}`}
                          title="Rename or move"
                          disabled={s.missing}
                          onClick={() => startRename(s)}
                        >
                          ✎
                        </button>
                        <button
                          type="button"
                          className="icon"
                          aria-label={`Delete ${s.name}`}
                          title="Delete script"
                          onClick={() => {
                            setModalError(null);
                            setDeleting(s);
                          }}
                        >
                          ✕
                        </button>
                      </span>
                    </li>
                  ))}
                </ul>
              )}
            </div>
          );
        })}
      </section>

      {newOpen && (
        <NewScriptModal
          folders={existingFolderNames}
          onClose={() => setNewOpen(false)}
          onCreated={(opened) => {
            setNewOpen(false);
            void refresh();
            void openLibraryScript(opened.info.id).catch(() => {
              // If no connection is configured yet for a .sql file, it is still saved cleanly in the library.
            });
          }}
        />
      )}

      {renaming && (
        <div className="modal-backdrop" role="presentation" onClick={() => setRenaming(null)}>
          <form
            className="modal"
            role="dialog"
            aria-modal="true"
            aria-label="Rename script"
            onClick={(e) => e.stopPropagation()}
            onSubmit={submitRename}
          >
            <h2>Rename / move script</h2>
            <label>
              New library path (folders created automatically)
              <input
                aria-label="New path"
                className="mono"
                value={renamePath}
                onChange={(e) => setRenamePath(e.target.value)}
                autoFocus
                required
              />
            </label>
            {modalError && <ErrorNote error={modalError} compact />}
            <div className="modal-actions">
              <button type="button" onClick={() => setRenaming(null)} disabled={modalBusy}>
                Cancel
              </button>
              <button type="submit" className="primary" disabled={modalBusy || !renamePath.trim()}>
                Rename
              </button>
            </div>
          </form>
        </div>
      )}

      {tagging && (
        <div className="modal-backdrop" role="presentation" onClick={() => setTagging(null)}>
          <form
            className="modal"
            role="dialog"
            aria-modal="true"
            aria-label="Edit script tags"
            onClick={(e) => e.stopPropagation()}
            onSubmit={submitTags}
          >
            <h2>Tags for {tagging.path}</h2>
            <label>
              Tags (comma-separated)
              <input
                aria-label="Script tags"
                value={tagsText}
                placeholder="post-restore, seed, qa"
                onChange={(e) => setTagsText(e.target.value)}
                autoFocus
              />
            </label>
            {lib && lib.tags.length > 0 && (
              <div className="wf-param-chips">
                <span className="muted">Existing tags:</span>
                {lib.tags.map((t) => (
                  <button key={t} type="button" className="wf-chip" onClick={() => addTagSuggestion(t)}>
                    +{t}
                  </button>
                ))}
              </div>
            )}
            {modalError && <ErrorNote error={modalError} compact />}
            <div className="modal-actions">
              <button type="button" onClick={() => setTagging(null)} disabled={modalBusy}>
                Cancel
              </button>
              <button type="submit" className="primary" disabled={modalBusy}>
                Save tags
              </button>
            </div>
          </form>
        </div>
      )}

      {deleting && (
        <div className="modal-backdrop" role="presentation" onClick={() => setDeleting(null)}>
          <div
            className="modal"
            role="dialog"
            aria-modal="true"
            aria-label="Delete script"
            onClick={(e) => e.stopPropagation()}
          >
            <h2>Delete script</h2>
            <p>
              Delete <code className="mono">{deleting.path}</code> from the script library on disk? Workflows referencing this script will need to be updated.
            </p>
            {modalError && <ErrorNote error={modalError} compact />}
            <div className="modal-actions">
              <button type="button" onClick={() => setDeleting(null)} disabled={modalBusy}>
                Cancel
              </button>
              <button type="button" className="danger" disabled={modalBusy} onClick={() => void confirmDelete()}>
                Confirm delete
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
