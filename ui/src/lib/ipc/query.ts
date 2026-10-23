// Typed wrappers over src-tauri/src/commands/{editor,results}.rs. Statement-level events
// stream as JSON over a Channel; result rows never cross as JSON — the grid pulls binary
// windows per result set (see ui/src/grid/protocol.ts).

import { Channel, invoke } from "@tauri-apps/api/core";
import type { ConnInfo } from "./connections";

export type TxnState = "idle" | "running" | "in_transaction" | "failed_transaction" | "disconnected";

/** Core transaction status (crates/pgwhy-core/src/query/txn.rs). */
export type TxnStatus = "idle" | "in_transaction" | "failed";

export interface ColumnInfo {
  name: string;
  type_oid?: number | null;
  type_name?: string | null;
}

export interface SqlError {
  kind: string;
  class: string;
  message: string;
  severity?: string | null;
  sqlstate?: string | null;
  detail?: string | null;
  hint?: string | null;
  context?: string | null;
  /** byte offset into the submitted text */
  position?: number | null;
}

export interface Notice {
  severity: string;
  code: string;
  message: string;
  detail?: string | null;
  hint?: string | null;
}

/** Statement events of a run (BufferEvent in crates/pgwhy-core/src/query/results.rs). */
export type QueryEvent =
  | { type: "statement_started"; index: number; from: number; to: number }
  | { type: "result"; index: number; result_id: string; columns: ColumnInfo[] }
  | { type: "rows"; index: number; result_id: string; row_count: number; truncated: boolean }
  | {
      type: "statement_done";
      index: number;
      command: string;
      rows_affected: number;
      duration_ms: number;
      result_id?: string | null;
      row_count?: number | null;
      truncated: boolean;
    }
  | { type: "error"; index: number; error: SqlError; result_id?: string | null; row_count?: number | null }
  | { type: "notice"; index?: number | null; notice: Notice }
  | { type: "cancelled"; index: number; result_id?: string | null; row_count?: number | null }
  | { type: "txn"; status: TxnStatus }
  | { type: "evicted"; report: { evicted: { result_id: string; rows: number }[] } }
  | {
      type: "finished";
      statements: number;
      succeeded: number;
      errors: number;
      skipped: number;
      cancelled: boolean;
      connection_lost: boolean;
      transaction_lost: boolean;
      txn: TxnStatus;
      duration_ms: number;
    };

export type ErrorPolicy = "stop" | "continue";

export const editorOpen = (tabId: string, connectionId: string, database: string) =>
  invoke<ConnInfo>("editor_open", { tabId, connectionId, database });

export function editorRun(
  tabId: string,
  sql: string,
  range: { from: number; to: number },
  policy: ErrorPolicy,
  onEvent: (ev: QueryEvent) => void,
): Promise<void> {
  const channel = new Channel<QueryEvent>();
  channel.onmessage = onEvent;
  return invoke<void>("editor_run", { tabId, sql, from: range.from, to: range.to, policy, events: channel });
}

export const editorCancel = (tabId: string) => invoke<void>("editor_cancel", { tabId });

export const editorTxnState = (tabId: string) => invoke<TxnState>("editor_txn_state", { tabId });

/** Close a tab's connection; an open transaction is committed or rolled back as asked. */
export const editorClose = (tabId: string, txn: "commit" | "rollback" | null) =>
  invoke<void>("editor_close", { tabId, txn });

export function resultSubscribe(resultId: string, onChunk: (buf: ArrayBuffer) => void): Promise<void> {
  const channel = new Channel<ArrayBuffer>();
  channel.onmessage = onChunk;
  return invoke<void>("result_subscribe", { resultId, channel });
}

export const resultWindow = (resultId: string, start: number, count: number) =>
  invoke<void>("result_window", { resultId, start, count });

export const resultRelease = (resultId: string) => invoke<void>("result_release", { resultId });

export const resultCopy = (resultId: string, r0: number, r1: number, c0: number, c1: number) =>
  invoke<string>("result_copy", { resultId, r0, r1, c0, c1 });

export type ExportFormat = "csv" | "json";

export interface ExportOptions {
  format: ExportFormat;
  path: string;
  /** CSV only: write NULL as an empty field (default) or as the literal text NULL */
  csv_null_literal?: boolean;
  header?: boolean;
  /**
   * CSV only: prefix cells a spreadsheet would evaluate (`=`, `+`, `-`, `@`, TAB) with `'`.
   * Defaults to on in the core; turn it off for machine-to-machine exports.
   */
  escape_formulas?: boolean;
}

export interface ExportResult {
  rows: number;
  bytes: number;
  pending_edits_included: number;
  partial: boolean;
}

export const resultExport = (resultId: string, options: ExportOptions) =>
  invoke<ExportResult>("result_export", { resultId, options });

export const pickSavePath = (defaultName: string, format: ExportFormat) =>
  invoke<string | null>("pick_save_path", { defaultName, format });
