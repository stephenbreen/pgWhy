// An editor tab: bound to one connection + database from creation, runs the
// selection or the whole buffer on a dedicated connection, shows results/messages in one
// pane, decorates errors where Postgres reported them, and never loses an open transaction
// silently — closing or disconnecting asks to commit or roll back.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { setCloseGuard, useWorkspace, type Tab } from "../app/workspace";
import { useConnections } from "../connection/store";
import { TagPill } from "../connection/EnvTag";
import { ErrorNote } from "../connection/ErrorNote";
import * as q from "../lib/ipc/query";
import { toAppError, type AppError } from "../lib/ipc/errors";
import * as scriptsIpc from "../lib/ipc/scripts";
import { byteToCharOffset, charToByteOffset, SqlEditor, type RunRequest, type SqlEditorHandle } from "./SqlEditor";
import { ResultsPane } from "./ResultsPane";
import { useEditors } from "./store";
import { useCompletionSchema } from "./completion";

export interface EditorTabData {
  connectionId: string;
  database: string;
  text?: string;
  filePath?: string;
  /** library script id when opened from the library */
  scriptId?: string;
  /** what was on disk when the buffer was loaded/saved (change detection) */
  fingerprint?: scriptsIpc.FileFingerprint;
}

const TXN_LABEL: Record<q.TxnState, string> = {
  idle: "autocommit",
  running: "running…",
  in_transaction: "in transaction",
  failed_transaction: "transaction failed — roll back",
  disconnected: "disconnected",
};

let untitled = 0;
export function openEditorTab(
  connectionId: string,
  database: string,
  opts: { text?: string; title?: string; filePath?: string; scriptId?: string; fingerprint?: scriptsIpc.FileFingerprint } = {},
) {
  const entry = useConnections.getState().entries.find((e) => e.id === connectionId);
  const id = `editor:${crypto.randomUUID()}`;
  useWorkspace.getState().openTab({
    id,
    kind: "editor",
    title: opts.title ?? `Query ${++untitled}`,
    connectionId,
    tag: entry?.env_tag,
    data: {
      connectionId,
      database,
      text: opts.text ?? "",
      filePath: opts.filePath,
      scriptId: opts.scriptId,
      fingerprint: opts.fingerprint,
    } satisfies EditorTabData,
  });
  return id;
}

type Ask = { resolve: (ok: boolean) => void; reason: "close" | "rebind" };
type DiskState = "changed" | "deleted" | null;

export function EditorTab({ tab }: { tab: Tab }) {
  const data = tab.data as EditorTabData;
  const entry = useConnections((s) => s.entries.find((e) => e.id === data.connectionId));
  const init = useEditors((s) => s.init);
  const patch = useEditors((s) => s.patch);
  const state = useEditors((s) => s.tabs[tab.id]);
  const textRef = useRef(data.text ?? "");
  const [openError, setOpenError] = useState<AppError | null>(null);
  const [policy, setPolicy] = useState<q.ErrorPolicy>("stop");
  const [ask, setAsk] = useState<Ask | null>(null);
  const [disk, setDisk] = useState<DiskState>(null);
  const [saveAs, setSaveAs] = useState<string | null>(null);
  const [saveError, setSaveError] = useState<AppError | null>(null);
  const editorHandle = useRef<SqlEditorHandle | null>(null);
  /** a run request is on its way to the core (blocks a double submit before `running` is set) */
  const starting = useRef(false);
  const entries = useConnections((s) => s.entries);
  const updateTab = useWorkspace((s) => s.updateTab);
  const schema = useCompletionSchema(data.connectionId, data.database);

  const open = useCallback(async () => {
    setOpenError(null);
    try {
      const info = await q.editorOpen(tab.id, data.connectionId, data.database);
      patch(tab.id, { connected: true, txn: "idle", handle: info.handle });
    } catch (e) {
      setOpenError(toAppError(e));
      patch(tab.id, { connected: false });
    }
  }, [tab.id, data.connectionId, data.database, patch]);

  useEffect(() => {
    init(tab.id, data.connectionId, data.database, data.filePath);
    void open();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  /** Stop a run in flight and wait for the tab to settle, so what follows sees its real state. */
  const settle = useCallback(async () => {
    if (!useEditors.getState().tabs[tab.id]?.running) return;
    await q.editorCancel(tab.id).catch(() => {});
    for (let i = 0; i < 100 && useEditors.getState().tabs[tab.id]?.running; i++) {
      await new Promise((r) => setTimeout(r, 50));
    }
    // the cancel leaves a transaction open or failed; ask the core rather than guess
    const txn = await q.editorTxnState(tab.id).catch(() => null);
    if (txn) patch(tab.id, { txn });
  }, [tab.id, patch]);

  // closing with an open transaction asks first
  useEffect(() => {
    setCloseGuard(tab.id, async () => {
      await settle();
      const s = useEditors.getState().tabs[tab.id];
      if (s && (s.txn === "in_transaction" || s.txn === "failed_transaction")) {
        const ok = await new Promise<boolean>((resolve) => setAsk({ resolve, reason: "close" }));
        if (!ok) return false;
      } else {
        await q.editorClose(tab.id, null).catch(() => {});
      }
      useEditors.getState().remove(tab.id);
      return true;
    });
    return () => setCloseGuard(tab.id, null);
  }, [tab.id, settle]);

  const finishTxn = async (action: "commit" | "rollback") => {
    const a = ask;
    setAsk(null);
    try {
      await q.editorClose(tab.id, action);
      patch(tab.id, { txn: "idle", connected: false });
      a?.resolve(true);
    } catch (e) {
      setOpenError(toAppError(e));
      a?.resolve(false);
    }
  };

  const run = useCallback(
    async (req: RunRequest) => {
      const s = useEditors.getState().tabs[tab.id];
      if (!s || s.running || starting.current) return;
      starting.current = true;
      if (!s.connected) await open();
      const text = req.text;
      // The previous results go only once the core has accepted the run: a refusal (busy,
      // disconnected) must leave what the user was looking at.
      let started = false;
      const begin = () => {
        if (started) return;
        started = true;
        useEditors.getState().startRun(tab.id);
      };
      if (data.filePath) {
        void scriptsIpc.recordRecent({ path: data.filePath, library_id: data.scriptId, action: "run", connection_id: data.connectionId, database: data.database });
      }
      try {
        await q.editorRun(
          tab.id,
          text,
          { from: charToByteOffset(text, req.from), to: charToByteOffset(text, req.to) },
          policy,
          (ev) => {
            begin();
            useEditors.getState().apply(tab.id, ev, text);
          },
        );
      } catch (e) {
        const err = toAppError(e);
        if (!started && err.kind === "refused") {
          // not started: keep the current results and say why
          setOpenError(err);
          return;
        }
        begin();
        useEditors.getState().apply(tab.id, { type: "error", index: 0, error: { kind: err.kind, class: err.class, message: err.message, detail: err.detail } }, text);
        useEditors.getState().apply(
          tab.id,
          { type: "finished", cancelled: false, errors: 1, statements: 0, succeeded: 0, skipped: 0, connection_lost: err.kind === "disconnected", transaction_lost: false, txn: "idle", duration_ms: 0 },
          text,
        );
        if (err.kind === "disconnected") patch(tab.id, { connected: false, txn: "disconnected" });
      } finally {
        starting.current = false;
      }
    },
    [tab.id, open, policy, patch, data.filePath, data.scriptId, data.connectionId, data.database],
  );

  const setData = (patchData: Partial<EditorTabData>) => updateTab(tab.id, { data: { ...data, ...patchData } });

  const save = async (asPath?: string) => {
    setSaveError(null);
    const text = textRef.current;
    try {
      if (asPath) {
        const opened = await scriptsIpc.scriptSave(asPath, text);
        updateTab(tab.id, { title: opened.info.name, data: { ...data, scriptId: opened.info.id, filePath: opened.path, fingerprint: opened.file.fingerprint } });
        void scriptsIpc.recordRecent({ path: opened.path, library_id: opened.info.id, action: "save" });
        setSaveAs(null);
      } else if (data.scriptId) {
        const info = await scriptsIpc.scriptWrite(data.scriptId, text);
        setData({ fingerprint: info.fingerprint });
        if (data.filePath) void scriptsIpc.recordRecent({ path: data.filePath, library_id: data.scriptId, action: "save" });
      } else if (data.filePath) {
        const fp = await scriptsIpc.fileWrite(data.filePath, text);
        setData({ fingerprint: fp });
        void scriptsIpc.recordRecent({ path: data.filePath, action: "save" });
      } else {
        setSaveAs(tab.title.replace(/\s+/g, "_").toLowerCase());
        return;
      }
      patch(tab.id, { dirty: false });
      setDisk(null);
    } catch (e) {
      setSaveError(toAppError(e));
    }
  };

  // a clean buffer follows the file on disk; a dirty one asks; a deleted file is marked
  useEffect(() => {
    const check = async () => {
      if (!data.filePath || !data.fingerprint) return;
      try {
        const res = await scriptsIpc.fileCheck(data.filePath, data.fingerprint);
        if (res.state === "unchanged") return setDisk(null);
        if (res.state === "deleted_on_disk") return setDisk("deleted");
        if (!useEditors.getState().tabs[tab.id]?.dirty) await reloadFromDisk();
        else setDisk("changed");
      } catch {
        /* unreadable: leave the buffer alone */
      }
    };
    window.addEventListener("focus", check);
    return () => window.removeEventListener("focus", check);
  }); // re-bind each render so the closure sees current data

  const reloadFromDisk = async () => {
    if (!data.filePath) return;
    const file = await scriptsIpc.fileRead(data.filePath);
    const view = editorHandle.current?.view;
    if (view) view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: file.content } });
    textRef.current = file.content;
    setData({ fingerprint: file.fingerprint });
    patch(tab.id, { dirty: false });
    setDisk(null);
  };

  // switch the receiving connection explicitly; an open transaction is asked about first
  const rebind = async (connectionId: string, database: string) => {
    await settle(); // switching under a running query would orphan it
    const s = useEditors.getState().tabs[tab.id];
    if (s && (s.txn === "in_transaction" || s.txn === "failed_transaction")) {
      const ok = await new Promise<boolean>((resolve) => setAsk({ resolve, reason: "rebind" }));
      if (!ok) return;
    } else {
      await q.editorClose(tab.id, null).catch(() => {});
    }
    const target = entries.find((e) => e.id === connectionId);
    updateTab(tab.id, { connectionId, tag: target?.env_tag, data: { ...data, connectionId, database } });
    patch(tab.id, { connectionId, database, connected: false, txn: "idle" });
    try {
      const info = await q.editorOpen(tab.id, connectionId, database);
      patch(tab.id, { connected: true, handle: info.handle });
      setOpenError(null);
    } catch (e) {
      setOpenError(toAppError(e));
    }
  };

  const errorDecoration = useMemo(() => {
    const e = state?.errorAt;
    if (!e) return null;
    const from = byteToCharOffset(e.text, e.from);
    return { from, to: byteToCharOffset(e.text, e.to), message: e.message };
  }, [state?.errorAt]);

  if (!state) return null;

  return (
    <div className="editor-tab" data-tag={entry?.env_tag}>
      <div className="editor-toolbar" role="toolbar" aria-label="Editor">
        <button type="button" className="primary" disabled={state.running} onClick={() => run({ text: textRef.current, from: 0, to: textRef.current.length, selection: false })} title="Run (F5 / Ctrl+Enter) — runs the selection when there is one">
          ▶ Run
        </button>
        <button type="button" disabled={!state.running} onClick={() => void q.editorCancel(tab.id)} title="Cancel (Esc)">
          ■ Cancel
        </button>
        <label className="check">
          <input type="checkbox" checked={policy === "continue"} onChange={(e) => setPolicy(e.target.checked ? "continue" : "stop")} /> continue on error
        </label>
        <span className="spacer" />
        <span className="txn-badge" data-state={state.txn} role="status" aria-label="Transaction state">
          {TXN_LABEL[state.txn]}
        </span>
        {!data.scriptId && data.filePath && (
          <button type="button" onClick={() => setSaveAs(tab.title)} title="Copy this file into the script library and edit the library copy">
            Add to library
          </button>
        )}
        <button type="button" onClick={() => void save()} title="Save (Ctrl+S)">
          Save{state.dirty ? " •" : ""}
        </button>
        <span className="binding" title="This tab runs against">
          {entry && <TagPill tag={entry.env_tag} />}
          <select
            aria-label="Run against connection"
            value={data.connectionId}
            disabled={state.running}
            onChange={(e) => {
              const next = entries.find((x) => x.id === e.target.value);
              if (next) void rebind(next.id, next.database);
            }}
          >
            {!entry && <option value={data.connectionId}>missing connection</option>}
            {entries.map((e) => (
              <option key={e.id} value={e.id}>
                {e.name}
              </option>
            ))}
          </select>
          <input
            className="binding-db"
            aria-label="Database"
            defaultValue={data.database}
            key={`${data.connectionId}/${data.database}`}
            disabled={state.running}
            onKeyDown={(e) => {
              if (e.key === "Enter") void rebind(data.connectionId, (e.target as HTMLInputElement).value.trim() || data.database);
            }}
            onBlur={(e) => {
              const db = e.target.value.trim();
              if (db && db !== data.database) void rebind(data.connectionId, db);
            }}
          />
        </span>
      </div>
      {saveAs !== null && (
        <form
          className="editor-banner"
          onSubmit={(e) => {
            e.preventDefault();
            if (saveAs.trim()) void save(saveAs.trim());
          }}
        >
          <span>Save to the script library as</span>
          <input aria-label="Script path" value={saveAs} autoFocus onChange={(e) => setSaveAs(e.target.value)} placeholder="folder/name.sql" />
          <button type="submit" className="primary">Save</button>
          <button type="button" onClick={() => setSaveAs(null)}>Cancel</button>
        </form>
      )}
      {saveError && <div className="editor-banner"><ErrorNote error={saveError} compact /></div>}
      {disk === "changed" && (
        <div className="editor-banner" role="alert">
          <span>This file changed on disk.</span>
          <button type="button" onClick={() => void reloadFromDisk()}>Reload</button>
          <button type="button" onClick={() => setDisk(null)}>Keep mine</button>
        </div>
      )}
      {disk === "deleted" && (
        <div className="editor-banner" role="alert">
          <span>This file was deleted on disk. Save to recreate it.</span>
        </div>
      )}
      {ask && (
        <div className="txn-ask" role="alert">
          <span>
            This tab has an open transaction on {entry?.name}/{data.database}. {ask.reason === "close" ? "Closing" : "Switching connections"} ends it.
          </span>
          <button type="button" onClick={() => void finishTxn("commit")} disabled={state.txn === "failed_transaction"}>
            Commit
          </button>
          <button type="button" onClick={() => void finishTxn("rollback")}>Roll back</button>
          <button type="button" onClick={() => { ask.resolve(false); setAsk(null); }}>Keep open</button>
        </div>
      )}
      {openError && (
        <div className="editor-banner">
          <ErrorNote error={openError} compact />
          <button type="button" onClick={() => void open()}>Reconnect</button>
        </div>
      )}
      {!openError && !state.connected && state.txn === "disconnected" && (
        <div className="editor-banner" role="alert">
          <span>Disconnected — any open transaction was rolled back by the server. Your text is kept.</span>
          <button type="button" onClick={() => void open()}>Reconnect</button>
        </div>
      )}
      <div className="editor-split">
        <div className="editor-host">
          <SqlEditor
            handleRef={editorHandle}
            onSave={() => void save()}
            initialText={textRef.current}
            schema={schema}
            error={errorDecoration}
            onChange={(t) => {
              textRef.current = t;
              if (!state.dirty) patch(tab.id, { dirty: true });
            }}
            onRun={run}
            onCancel={() => void q.editorCancel(tab.id)}
          />
        </div>
        <ResultsPane state={state} onSelectResult={(i) => patch(tab.id, { activeResult: i })} />
      </div>
      <div className="editor-status">
        {state.lastRun && (
          <span>
            {state.lastRun.statements} statement{state.lastRun.statements === 1 ? "" : "s"}, {state.lastRun.errors} error{state.lastRun.errors === 1 ? "" : "s"} · {state.lastRun.durationMs} ms
          </span>
        )}
        <span className="spacer" />
        <span>{entry ? `${entry.user}@${entry.host}:${entry.port}/${data.database}` : ""}</span>
      </div>
    </div>
  );
}
