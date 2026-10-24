// Opening scripts into editor tabs with the right connection binding:
// recents restore the connection they last ran against; library scripts bind to the active
// connection. The editor shows the binding before anything runs and lets it be switched.
// Plain-text and Markdown notes (.txt / .md) open in a text tab, which needs no connection.

import { useConnections } from "../connection/store";
import { openEditorTab } from "../editor/EditorTab";
import * as ipc from "../lib/ipc/scripts";
import { openTextTab } from "./TextTab";

/** .sql files are scripts; everything else the library holds (.txt / .md) is a note. */
export function isNote(path: string): boolean {
  return /\.(txt|md)$/i.test(path);
}

export function fileName(path: string): string {
  return path.split(/[\\/]/).pop() ?? path;
}

function pickBinding(preferred?: { connectionId?: string; database?: string }): { connectionId: string; database: string } | null {
  const { entries, activeId } = useConnections.getState();
  const pref = preferred?.connectionId && entries.find((e) => e.id === preferred.connectionId);
  if (pref) return { connectionId: pref.id, database: preferred?.database ?? pref.database };
  const active = entries.find((e) => e.id === activeId) ?? entries[0];
  return active ? { connectionId: active.id, database: active.database } : null;
}

function requireBinding(preferred?: { connectionId?: string; database?: string }) {
  const binding = pickBinding(preferred);
  if (!binding) throw new Error("Add a connection first — scripts open bound to a connection.");
  return binding;
}

export async function openLibraryScript(id: string, preferred?: { connectionId?: string; database?: string }) {
  const opened = await ipc.scriptRead(id);
  const file = { text: opened.file.content, filePath: opened.path, scriptId: opened.info.id, fingerprint: opened.file.fingerprint };
  if (isNote(opened.path)) {
    openTextTab({ ...file, title: opened.info.name });
  } else {
    const binding = requireBinding(preferred);
    openEditorTab(binding.connectionId, binding.database, { ...file, title: opened.info.name });
  }
  void ipc.recordRecent({ path: opened.path, library_id: opened.info.id, action: "open" });
}

/** Open a file from disk by absolute path; files inside the library open as library scripts. */
export async function openPath(path: string, preferred?: { connectionId?: string; database?: string }) {
  const lib = await ipc.libraryList().catch(() => null);
  const inLibrary = lib?.scripts.find((s) => `${lib.root}/${s.path}` === path && !s.missing);
  if (inLibrary) return openLibraryScript(inLibrary.id, preferred);
  const file = await ipc.fileRead(path);
  const name = fileName(path);
  if (isNote(path)) {
    openTextTab({ text: file.content, title: name, filePath: path, fingerprint: file.fingerprint });
  } else {
    const binding = requireBinding(preferred);
    openEditorTab(binding.connectionId, binding.database, { text: file.content, title: name.replace(/\.sql$/i, ""), filePath: path, fingerprint: file.fingerprint });
  }
  void ipc.recordRecent({ path, action: "open" });
}

/** Native open dialog → tab. Resolves false when the dialog was cancelled. */
export async function openFromDisk(): Promise<boolean> {
  const path = await ipc.pickOpenPath();
  if (!path) return false;
  await openPath(path);
  return true;
}

export async function openRecent(r: ipc.RecentStatus) {
  const preferred = { connectionId: r.entry.connection_id, database: r.entry.database };
  if (r.entry.library_id) return openLibraryScript(r.entry.library_id, preferred);
  return openPath(r.resolved_path, preferred);
}
