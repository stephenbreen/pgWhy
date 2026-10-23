// Typed wrappers over src-tauri/src/commands/restore.rs (v1.1 Epic 2).
//
// The archive is inspected server-side before anything starts (the view is display only), and
// the pipeline re-inspects it again when the restore begins.

import { Channel, invoke } from "@tauri-apps/api/core";
import type { OperationRecord, StepStatus } from "./databases";

export type RestoreMode = "new_database" | "replace" | "on_top";

export interface TocHeaderView {
  dbname?: string | null;
  dumped_from?: string | null;
  dumped_by?: string | null;
  toc_entries?: number | null;
}

export interface RestoreInspection {
  archive: { path: string; size_bytes?: number | null; reusable: boolean; reason?: string | null };
  tool: string;
  dbname?: string | null;
  dumped_from?: string | null;
  dumped_by?: string | null;
  toc_entries?: number | null;
  /** Refusals that must disable Start (plain SQL, unreadable, version gate). */
  blockers: string[];
}

export interface RestoreTargets {
  databases: string[];
  can_create_database: boolean;
  capabilities_error?: string | null;
}

export interface RestoreSettings {
  jobs: number | null;
  no_owner: boolean;
  no_privileges: boolean;
  stop_on_first_error: boolean;
}

export interface RestoreStartRequest {
  path: string;
  database: string;
  mode: RestoreMode;
  locale_from?: string | null;
  settings: RestoreSettings;
  typed_name?: string | null;
  backup_first: boolean;
}

export type RestoreEvent =
  | { type: "started"; id: string; name: string; mode: RestoreMode }
  | { type: "step"; id: string; label: string; status: StepStatus }
  | {
      type: "progress";
      progress: {
        phase?: string;
        current_object?: string;
        objects_done: number;
        objects_total?: number;
        total_is_estimate: boolean;
        elapsed_ms: number;
      };
    }
  | { type: "output"; line: string };

export const pickRestoreArchive = () => invoke<string | null>("pick_restore_archive");

export const restoreInspect = (path: string) =>
  invoke<RestoreInspection>("restore_inspect", { path });

export const restoreTargets = (connectionId: string) =>
  invoke<RestoreTargets>("restore_targets", { connectionId });

export const restoreCancel = (runId: string) => invoke<boolean>("restore_cancel", { runId });

export function restoreStart(
  connectionId: string,
  request: RestoreStartRequest,
  onEvent: (e: RestoreEvent) => void,
): Promise<OperationRecord> {
  const channel = new Channel<RestoreEvent>();
  channel.onmessage = onEvent;
  return invoke<OperationRecord>("restore_start", { connectionId, request, events: channel });
}
