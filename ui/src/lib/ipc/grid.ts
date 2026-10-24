// Typed wrappers over src-tauri/src/commands/grid.rs (top-N view + editable layer).

import { invoke } from "@tauri-apps/api/core";

export type CriteriaOp = "=" | "<>" | "<" | "<=" | ">" | ">=" | "LIKE" | "ILIKE" | "NOT LIKE" | "IS NULL" | "IS NOT NULL";

export interface Criterion {
  column: string;
  op: CriteriaOp;
  value?: string | null;
}

export interface SortKey {
  column: string;
  dir: "asc" | "desc";
}

export interface GridColumn {
  name: string;
  type: string;
  /** part of the edit key (auto-included even if not asked for) */
  key: boolean;
  writable: boolean;
}

export interface GridView {
  result_id: string;
  columns: GridColumn[];
  row_count: number;
  /** the generated SELECT, with literals inlined for display only */
  sql: string;
  editable: boolean;
  read_only_reason?: string | null;
  n: number;
}

export interface PendingInfo {
  pending: number;
  /** total rows incl. inserted (virtual) rows */
  row_count: number;
}

export interface CommitResult {
  updated: number;
  inserted: number;
  deleted: number;
}

export const gridOpen = (args: {
  gridId: string;
  connectionId: string;
  database: string;
  schema: string;
  name: string;
  n?: number | null;
  criteria: Criterion[];
  sort: SortKey[];
}) => invoke<GridView>("grid_open", args);

export const gridRequery = (gridId: string, criteria: Criterion[], sort: SortKey[], n?: number | null) =>
  invoke<GridView>("grid_requery", { gridId, criteria, sort, n: n ?? null });

export const gridEditCell = (gridId: string, row: number, col: number, value: string | null) =>
  invoke<PendingInfo>("grid_edit_cell", { gridId, row, col, value });

export const gridInsertRow = (gridId: string) => invoke<PendingInfo>("grid_insert_row", { gridId });

export const gridDeleteRows = (gridId: string, rows: number[], undelete = false) =>
  invoke<PendingInfo>("grid_delete_rows", { gridId, rows, undelete });

export const gridPreview = (gridId: string) => invoke<string[]>("grid_preview", { gridId });

export const gridCommit = (gridId: string) => invoke<CommitResult>("grid_commit", { gridId });

export const gridDiscard = (gridId: string) => invoke<PendingInfo>("grid_discard", { gridId });

export const gridClose = (gridId: string) => invoke<void>("grid_close", { gridId });
