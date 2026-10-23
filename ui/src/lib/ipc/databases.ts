// Typed wrappers over src-tauri/src/commands/databases.rs (v1.1 Epic 4).
//
// Create/rename/drop are user-initiated destructive operations: the core enforces the
// environment-tag gate, the typed-name confirmation and the per-server run lock, and records
// each attempt in the shared run history. These wrappers only move data.

import { invoke } from "@tauri-apps/api/core";

export interface DbCapabilities {
  can_create_database: boolean;
  is_superuser: boolean;
  server_version: string;
  current_user: string;
}

export type OperationKind = "create" | "rename" | "drop" | "backup" | "restore";
export type OperationStatus =
  | "running"
  | "succeeded"
  | "succeeded_cleanup_required"
  | "failed_at_step"
  | "failed_at_swap"
  | "cancelled"
  | "interrupted";
export type StepStatus =
  | "pending"
  | "running"
  | "succeeded"
  | "failed"
  | "cancelled"
  | "skipped"
  | "reused"
  | "interrupted";

export interface OperationStep {
  id: string;
  label: string;
  status: StepStatus;
  duration_ms?: number;
  error?: { message: string; hint?: string; detail?: string };
  note?: string;
}

/** The report the dialogs show after an attempt (a subset of the persisted record). */
export interface OperationRecord {
  id: string;
  kind: OperationKind;
  name: string;
  started_at: string;
  finished_at?: string;
  status: OperationStatus;
  steps: OperationStep[];
  warnings: string[];
  error?: { kind: string; class: string; message: string; hint?: string; detail?: string };
  failed_step?: string;
}

export const dbCapabilities = (connectionId: string) =>
  invoke<DbCapabilities>("db_capabilities", { connectionId });

export const dbCreate = (connectionId: string, name: string, localeFrom: string | null, comment: string | null) =>
  invoke<OperationRecord>("db_create", { connectionId, name, localeFrom, comment });

export const dbRename = (connectionId: string, database: string, to: string) =>
  invoke<OperationRecord>("db_rename", { connectionId, database, to });

export const dbDrop = (connectionId: string, database: string, typedName: string, backupFirst: boolean) =>
  invoke<OperationRecord>("db_drop", { connectionId, database, typedName, backupFirst });
