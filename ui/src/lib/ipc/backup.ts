import { Channel, invoke } from "@tauri-apps/api/core";
import type { AppError } from "./errors";

export interface ToolProgress {
  tool: "pg_dump" | "pg_restore" | string;
  phase: string;
  current_object?: string | null;
  objects_done: number;
  objects_total?: number | null;
  total_is_estimate: boolean;
  bytes_written?: number | null;
  elapsed_ms: number;
  last_line?: string | null;
  warnings: number;
  errors: number;
}

export type BackupEvent =
  | { type: "started"; path: string; pg_dump: string; server_version: string }
  | { type: "progress"; progress: ToolProgress }
  | { type: "finished"; path: string; size_bytes: number; duration_ms: number; removed_by_retention: string[] }
  | { type: "failed"; error: AppError };

/** What to back up: the whole database, one schema, or named tables. */
export type BackupScope =
  | { kind: "database" }
  | { kind: "schema"; schema: string }
  | { kind: "tables"; tables: { schema: string; name: string }[] };

export type BackupFormat = "custom" | "directory";
export type BackupData = "all" | "schema_only" | "data_only";

export interface BackupRequest {
  format: BackupFormat;
  /** Parallel workers; directory format only. */
  jobs: number | null;
  data: BackupData;
  scope: BackupScope;
}

export const defaultBackupRequest = (scope: BackupScope): BackupRequest => ({
  format: "custom",
  jobs: null,
  data: "all",
  scope,
});

export function backupDatabase(
  backupId: string,
  connectionId: string,
  database: string,
  request: BackupRequest,
  onEvent: (e: BackupEvent) => void,
) {
  const events = new Channel<BackupEvent>();
  events.onmessage = onEvent;
  return invoke<void>("backup_database", { backupId, connectionId, database, request, events });
}

export const backupCancel = (backupId: string) => invoke<void>("backup_cancel", { backupId });
export const revealPath = (path: string) => invoke<void>("reveal_path", { path });
