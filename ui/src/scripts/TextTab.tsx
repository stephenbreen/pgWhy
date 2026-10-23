// A text tab for plain-text and Markdown notes (.txt / .md) kept with the scripts: no
// connection and no Run, just edit and save. A file opened from disk can be added to the
// script library, after which the tab edits the library copy. Like editor tabs, a clean
// buffer follows the file on disk and a dirty one asks (EC-8).

import { useEffect, useRef, useState } from "react";
import { EditorState } from "@codemirror/state";
import { EditorView, keymap, lineNumbers, highlightActiveLine, highlightActiveLineGutter, drawSelection } from "@codemirror/view";
import { defaultKeymap, history, historyKeymap } from "@codemirror/commands";
import { searchKeymap, highlightSelectionMatches } from "@codemirror/search";
import { useWorkspace, type Tab } from "../app/workspace";
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

function PlainEditor({ initialText, onChange, onSave, viewRef }: { initialText: string; onChange: (t: string) => void; onSave: () => void; viewRef: { current: EditorView | null } }) {
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
            if (u.docChanged) callbacks.current.onChange(u.state.doc.toString());
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
  const [dirty, setDirty] = useState(false);
  const [saveAs, setSaveAs] = useState<string | null>(null);
  const [error, setError] = useState<AppError | null>(null);
  const [disk, setDisk] = useState<"changed" | "deleted" | null>(null);
  const ext = data.filePath?.match(/\.(txt|md)$/i)?.[0] ?? ".md";

  const setData = (patch: Partial<TextTabData>) => updateTab(tab.id, { data: { ...data, ...patch } });

  const save = async (asPath?: string) => {
    setError(null);
    const text = textRef.current;
    try {
      if (asPath) {
        const opened = await ipc.scriptSave(withExt(asPath, ext), text);
        updateTab(tab.id, { title: opened.info.name, data: { ...data, scriptId: opened.info.id, filePath: opened.path, fingerprint: opened.file.fingerprint } });
        void ipc.recordRecent({ path: opened.path, library_id: opened.info.id, action: "save" });
        setSaveAs(null);
      } else if (data.scriptId) {
        const info = await ipc.scriptWrite(data.scriptId, text);
        setData({ fingerprint: info.fingerprint });
      } else if (data.filePath) {
        setData({ fingerprint: await ipc.fileWrite(data.filePath, text) });
        void ipc.recordRecent({ path: data.filePath, action: "save" });
      } else {
        setSaveAs(tab.title.replace(/\s+/g, "_").toLowerCase());
        return;
      }
      setDirty(false);
      setDisk(null);
    } catch (e) {
      setError(toAppError(e));
    }
  };

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
        else setDisk("changed");
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
          <button type="button" onClick={() => setDisk(null)}>Keep mine</button>
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
          onChange={(t) => {
            textRef.current = t;
            setDirty(true);
          }}
        />
      </div>
    </div>
  );
}
