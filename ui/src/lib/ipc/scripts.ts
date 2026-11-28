// Typed wrappers over src-tauri/src/commands/scripts.rs.

import { invoke } from "@tauri-apps/api/core";

export interface FileFingerprint {
  sha256: string;
  size: number;
  modified_ms: number;
}

export interface ScriptInfo {
  id: string;
  path: string;
  name: string;
  folder: string;
  tags: string[];
  fingerprint: FileFingerprint;
  missing: boolean;
}

export interface LibrarySnapshot {
  root: string;
  scripts: ScriptInfo[];
  tags: string[];
}

export interface ScriptFile {
  content: string;
  fingerprint: FileFingerprint;
}

export interface OpenedScript {
  info: ScriptInfo;
  path: string;
  file: ScriptFile;
}

export type RecentAction = "open" | "save" | "run";

export interface RecentScript {
  path: string;
  library_id?: string;
  connection_id?: string;
  database?: string;
  last_used_ms: number;
  last_action: RecentAction;
}

export interface RecentStatus {
  entry: RecentScript;
  resolved_path: string;
  state: "available" | "not_found";
}

export interface MetaCommand {
  command: string;
  line: number;
  column: number;
  offset: number;
  text: string;
  position: "line_start" | "inline";
}

export type FileState = "unchanged" | "changed_on_disk" | "deleted_on_disk";

export const libraryList = (query?: string) => invoke<LibrarySnapshot>("library_list", { query: query ?? null });
export const libraryRescan = () => invoke<{ library: LibrarySnapshot }>("library_rescan");
export const scriptRead = (id: string) => invoke<OpenedScript>("script_read", { id });
export const scriptSave = (path: string, content: string) => invoke<OpenedScript>("script_save", { path, content });
/** `expected` is the file as the tab last saw it; a file changed since is refused (`isChangedOnDisk`). */
export const scriptWrite = (id: string, content: string, expected?: FileFingerprint) =>
  invoke<ScriptInfo>("script_write", { id, content, expected: expected ?? null });
export const scriptRename = (id: string, path: string) => invoke<ScriptInfo>("script_rename", { id, path });
export const scriptDelete = (id: string) => invoke<void>("script_delete", { id });
export const scriptSetTags = (id: string, tags: string[]) => invoke<ScriptInfo>("script_set_tags", { id, tags });
export const scriptRecents = () => invoke<RecentStatus[]>("script_recents");
export const recordRecent = (used: { path: string; library_id?: string | null; action: RecentAction; connection_id?: string | null; database?: string | null }) =>
  invoke<void>("script_recent_record", { used });
export const removeRecent = (path: string) => invoke<void>("script_recent_remove", { path });
/** Native open dialog for .sql / .txt / .md files; null when cancelled. */
export const pickOpenPath = () => invoke<string | null>("pick_open_path");
export const fileRead = (path: string) => invoke<ScriptFile>("file_read", { path });
export const fileWrite = (path: string, content: string, expected?: FileFingerprint) =>
  invoke<FileFingerprint>("file_write", { path, content, expected: expected ?? null });

/** A save refused because the file is no longer what the tab loaded: the fingerprint it has now. */
export function changedOnDisk(e: unknown): FileFingerprint | null {
  const data = (e as { data?: { code?: string; current?: FileFingerprint } } | null)?.data;
  return data?.code === "changed_on_disk" && data.current ? data.current : null;
}
export const fileCheck = (path: string, fingerprint: FileFingerprint) =>
  invoke<{ state: FileState; current?: FileFingerprint | null }>("file_check", { path, fingerprint });
export const metaScan = (text: string) => invoke<MetaCommand[]>("meta_scan", { text });
