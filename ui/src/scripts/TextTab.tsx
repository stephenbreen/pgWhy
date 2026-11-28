// A text tab for plain-text and Markdown notes (.txt / .md) kept with the scripts: no
// connection and no Run, just edit and save. A file opened from disk can be added to the
// script library, after which the tab edits the library copy. Like editor tabs, a clean
// buffer follows the file on disk and a dirty one asks.

import { useEffect, useRef, useState } from "react";
import { EditorState } from "@codemirror/state";
import { EditorView, keymap, lineNumbers, highlightActiveLine, highlightActiveLineGutter, drawSelection } from "@codemirror/view";
import { defaultKeymap, history, historyKeymap } from "@codemirror/commands";
import { searchKeymap, highlightSelectionMatches } from "@codemirror/search";
import { setCloseGuard, useWorkspace, type Tab } from "../app/workspace";
import { ErrorNote } from "../connection/ErrorNote";
import { editorTheme } from "../editor/SqlEditor";
import { toAppError, type AppError } from "../lib/ipc/errors";
import * as ipc from "../lib/ipc/scripts";

export interface TextTabData {
  text: string;
  filePath?: string;
  /** library script id once the note is in the library */
  scriptId?: string;
  fingerprint?: ipc.FileFingerprint;
}

export function openTextTab(opts: TextTabData & { title: string }) {
  const { title, ...data } = opts;
  const ws = useWorkspace.getState();
  // one tab per file: reopening focuses it
  const open = data.filePath && ws.tabs.find((t) => t.kind === "text" && (t.data as TextTabData).filePath === data.filePath);
  if (open) return ws.activateTab(open.id);
  ws.openTab({ id: `text:${crypto.randomUUID()}`, kind: "text", title, data: data satisfies TextTabData });
}

/** Library path for "Add to library": keep the file's own extension so a note stays a note. */
function withExt(path: string, ext: string): string {
  return /\.(sql|txt|md)$/i.test(path) ? path : `${path}${ext}`;
}

function PlainEditor({ initialText, onChange, onSave, viewRef }: { initialText: string; onChange: () => void; onSave: () => void; viewRef: { current: EditorView | null } }) {
  const host = useRef<HTMLDivElement>(null);
  const callbacks = useRef({ onChange, onSave });
  callbacks.current = { onChange, onSave };

  useEffect(() => {
    const view = new EditorView({
      parent: host.current!,
      state: EditorState.create({
        doc: initialText,
        extensions: [
          lineNumbers(),
          highlightActiveLineGutter(),
          history(),
          drawSelection(),
          highlightActiveLine(),
          highlightSelectionMatches(),
          EditorView.lineWrapping,
          keymap.of([
            { key: "Mod-s", run: () => (callbacks.current.onSave(), true), preventDefault: true },
            ...defaultKeymap,
            ...searchKeymap,
            ...historyKeymap,
          ]),
          EditorView.updateListener.of((u) => {
            if (u.docChanged) callbacks.current.onChange();
          }),
          editorTheme,
        ],
      }),
    });
    viewRef.current = view;
    return () => {
      view.destroy();
      viewRef.current = null;
    };
    // the editor owns its document after mount; initialText only seeds it
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return <div className="sql-editor" ref={host} />;
}

export function TextTab({ tab }: { tab: Tab }) {
  const data = tab.data as TextTabData;
  const updateTab = useWorkspace((s) => s.updateTab);
  const textRef = useRef(data.text);
  const viewRef = useRef<EditorView | null>(null);
  const [dirty, setDirtyState] = useState(false);
  // the close guard runs outside render, so it reads the flag from here
  const dirtyRef = useRef(false);
  const setDirty = (d: boolean) => {
    dirtyRef.current = d;
    setDirtyState(d);
  };
  const [unsaved, setUnsaved] = useState<{ resolve: (answer: "save" | "discard" | "cancel") => void } | null>(null);
  const [saveAs, setSaveAs] = useState<string | null>(null);
  const [error, setError] = useState<AppError | null>(null);
  const [disk, setDisk] = useState<"changed" | "deleted" | null>(null);
  const ext = data.filePath?.match(/\.(txt|md)$/i)?.[0] ?? ".md";

  const diskCurrent = useRef<ipc.FileFingerprint | null>(null);
  // "Keep mine" accepts the file as it is on disk now as the one the next save replaces
  const keepMine = () => {
    if (diskCurrent.current) setData({ fingerprint: diskCurrent.current });
    diskCurrent.current = null;
    setDisk(null);
  };
  const setData = (patch: Partial<TextTabData>) => updateTab(tab.id, { data: { ...data, ...patch } });

  const save = async (asPath?: string): Promise<boolean> => {
    setError(null);
    // read from the editor at save time rather than copying the document on every keystroke
    const text = viewRef.current?.state.doc.toString() ?? textRef.current;
    try {
      if (asPath) {
        const opened = await ipc.scriptSave(withExt(asPath, ext), text);
        updateTab(tab.id, { title: opened.info.name, data: { ...data, scriptId: opened.info.id, filePath: opened.path, fingerprint: opened.file.fingerprint } });
        void ipc.recordRecent({ path: opened.path, library_id: opened.info.id, action: "save" });
        setSaveAs(null);
      } else if (data.scriptId) {
        const info = await ipc.scriptWrite(data.scriptId, text, data.fingerprint);
        setData({ fingerprint: info.fingerprint });
      } else if (data.filePath) {
        setData({ fingerprint: await ipc.fileWrite(data.filePath, text, data.fingerprint) });
        void ipc.recordRecent({ path: data.filePath, action: "save" });
      } else {
        setSaveAs(tab.title.replace(/\s+/g, "_").toLowerCase());
        return false;
      }
      // keystrokes typed while the save was in flight are not on disk: only a buffer still equal
      // to what was written is clean
      if ((viewRef.current?.state.doc.toString() ?? textRef.current) === text) setDirty(false);
      setDisk(null);
      return true;
    } catch (e) {
      const current = ipc.changedOnDisk(e);
      if (current) {
        // someone else wrote the file since this tab read it: ask, rather than replace their work
        diskCurrent.current = current;
        setDisk("changed");
      } else setError(toAppError(e));
      return false;
    }
  };
  const asking = useRef(false);
  const saveRef = useRef(save);
  saveRef.current = save;

  // Closing with unsaved text asks first. A note that was never saved and is empty has nothing to lose.
  useEffect(() => {
    setCloseGuard(tab.id, async () => {
      if (!dirtyRef.current) return true;
      const text = viewRef.current?.state.doc.toString() ?? textRef.current;
      if (!text.trim() && !data.filePath && !data.scriptId) return true;
      if (asking.current) return false; // a second close request while the prompt is up
      asking.current = true;
      const answer = await new Promise<"save" | "discard" | "cancel">((resolve) => setUnsaved({ resolve }));
      asking.current = false;
      setUnsaved(null);
      if (answer === "discard") return true;
      if (answer === "save") return saveRef.current();
      return false;
    });
    return () => setCloseGuard(tab.id, null);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tab.id, data.filePath, data.scriptId]);

  const reloadFromDisk = async () => {
    if (!data.filePath) return;
    const file = await ipc.fileRead(data.filePath);
    const view = viewRef.current;
    if (view) view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: file.content } });
    textRef.current = file.content;
    setData({ fingerprint: file.fingerprint });
    setDirty(false);
    setDisk(null);
  };

  useEffect(() => {
    const check = async () => {
      if (!data.filePath || !data.fingerprint) return;
      try {
        const res = await ipc.fileCheck(data.filePath, data.fingerprint);
        if (res.state === "unchanged") return setDisk(null);
        if (res.state === "deleted_on_disk") return setDisk("deleted");
        if (!dirty) await reloadFromDisk();
        else {
          diskCurrent.current = res.current ?? null;
          setDisk("changed");
        }
      } catch {
        /* unreadable: leave the buffer alone */
      }
    };
    window.addEventListener("focus", check);
    return () => window.removeEventListener("focus", check);
  }); // re-bind each render so the closure sees current data

  return (
    <div className="editor-tab text-tab">
      <div className="editor-toolbar" role="toolbar" aria-label="Text editor">
        <span className="text-path" title={data.filePath}>
          {data.scriptId ? "Library" : data.filePath ? "File" : "Unsaved"}
          {data.filePath && ` · ${data.filePath}`}
        </span>
        <span className="spacer" />
        {!data.scriptId && data.filePath && (
          <button type="button" onClick={() => setSaveAs(tab.title)} title="Copy this file into the script library and edit the library copy">
            Add to library
          </button>
        )}
        <button type="button" onClick={() => void save()} title="Save (Ctrl+S)">
          Save{dirty ? " •" : ""}
        </button>
      </div>
      {unsaved && (
        <div className="txn-ask" role="alert">
          <span>{tab.title} has unsaved changes.</span>
          <button type="button" onClick={() => unsaved.resolve("save")}>Save</button>
          <button type="button" onClick={() => unsaved.resolve("discard")}>Discard</button>
          <button type="button" onClick={() => unsaved.resolve("cancel")}>Cancel</button>
        </div>
      )}
      {saveAs !== null && (
        <form
          className="editor-banner"
          onSubmit={(e) => {
            e.preventDefault();
            if (saveAs.trim()) void save(saveAs.trim());
          }}
        >
          <span>Save to the script library as</span>
          <input aria-label="Script path" value={saveAs} autoFocus onChange={(e) => setSaveAs(e.target.value)} placeholder={`folder/name${ext}`} />
          <button type="submit" className="primary">Save</button>
          <button type="button" onClick={() => setSaveAs(null)}>Cancel</button>
        </form>
      )}
      {error && <div className="editor-banner"><ErrorNote error={error} compact /></div>}
      {disk === "changed" && (
        <div className="editor-banner" role="alert">
          <span>This file changed on disk.</span>
          <button type="button" onClick={() => void reloadFromDisk()}>Reload</button>
          <button type="button" onClick={keepMine}>Keep mine</button>
        </div>
      )}
      {disk === "deleted" && (
        <div className="editor-banner" role="alert">
          <span>This file was deleted on disk. Save to recreate it.</span>
        </div>
      )}
      <div className="editor-host">
        <PlainEditor
          viewRef={viewRef}
          initialText={textRef.current}
          onSave={() => void save()}
          onChange={() => setDirty(true)}
        />
      </div>
    </div>
  );
}
