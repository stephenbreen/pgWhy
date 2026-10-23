// Typed wrappers over src-tauri/src/commands/explorer.rs (introspection, U8/U9/U10).

import { invoke } from "@tauri-apps/api/core";

export type RelKind = "table" | "partitioned_table" | "view" | "matview" | "foreign_table" | "sequence";
export type RoutineKind = "function" | "procedure" | "aggregate" | "window";

export interface DatabaseNode {
  name: string;
  is_template: boolean;
  allow_connections: boolean;
  comment?: string | null;
}

export interface SchemaNode {
  name: string;
  system: boolean;
}

export interface ColumnNode {
  name: string;
  type: string;
  not_null: boolean;
  default?: string | null;
  in_primary_key: boolean;
  identity?: string | null;
  generated?: string | null;
}

export interface RelationNode {
  schema: string;
  name: string;
  kind: RelKind;
  columns: ColumnNode[];
  partition_of?: string | null;
  row_estimate?: number | null;
  comment?: string | null;
}

export interface RoutineNode {
  schema: string;
  name: string;
  kind: RoutineKind;
  identity_args: string;
  result?: string | null;
}

export interface SchemaContents {
  relations: RelationNode[];
  routines: RoutineNode[];
}

export interface SearchHit {
  schema: string;
  name: string;
  kind: RelKind | RoutineKind;
  identity_args?: string | null;
}

export interface ConstraintInfo {
  name: string;
  kind: "primary_key" | "foreign_key" | "unique" | "check" | "exclusion";
  columns: string[];
  definition: string;
}

export interface IndexInfo {
  name: string;
  definition: string;
  unique: boolean;
  primary: boolean;
  valid: boolean;
  size_bytes?: number | null;
}

export interface RelationStats {
  row_estimate?: number | null;
  live_rows?: number | null;
  dead_rows?: number | null;
  total_bytes?: number | null;
  table_bytes?: number | null;
  indexes_bytes?: number | null;
  last_analyze?: string | null;
  last_autoanalyze?: string | null;
  last_vacuum?: string | null;
  last_autovacuum?: string | null;
}

export interface RelationDetails {
  relation: RelationNode;
  constraints: ConstraintInfo[];
  indexes: IndexInfo[];
  stats: RelationStats;
  /** columns usable as an edit key (PK, or best unique NOT NULL key); empty = read-only */
  edit_key: string[];
  read_only_reason?: string | null;
  view_definition?: string | null;
  /** the full core model (sequence info, updatability, FK targets, ...) */
  raw: {
    sequence?: { data_type: string; start: number; increment: number; min: number; max: number; cache: number; cycle: boolean; last_value?: number | null; owned_by?: { table: { schema: string; name: string }; column: string } | null } | null;
    constraints: { name: string; foreign_key?: { table: { schema: string; name: string }; columns: string[]; on_update: string; on_delete: string } | null }[];
    relation: { partition_key?: string | null; partition_bound?: string | null; partitions: { schema: string; name: string }[]; owner: string };
  };
}

export type ScriptKind = "select" | "insert" | "update" | "delete" | "create";

export const scriptAs = (connectionId: string, database: string, schema: string, name: string, kind: ScriptKind) =>
  invoke<string>("explorer_script_as", { connectionId, database, schema, name, kind });

export const scriptOptions = (connectionId: string, database: string, schema: string, name: string) =>
  invoke<[ScriptKind, string | null][]>("explorer_script_options", { connectionId, database, schema, name });

export type ObjectRef =
  | { type: "relation"; schema: string; name: string }
  | { type: "routine"; schema: string; name: string; identity_args: string };

export const listDatabases = (connectionId: string) => invoke<DatabaseNode[]>("explorer_databases", { connectionId });
export const listSchemas = (connectionId: string, database: string) => invoke<SchemaNode[]>("explorer_schemas", { connectionId, database });
export const schemaContents = (connectionId: string, database: string, schema: string) =>
  invoke<SchemaContents>("explorer_schema", { connectionId, database, schema });
export const searchObjects = (connectionId: string, database: string, pattern: string) =>
  invoke<SearchHit[]>("explorer_search", { connectionId, database, pattern });
export const relationDetails = (connectionId: string, database: string, schema: string, name: string, refresh = false) =>
  invoke<RelationDetails | null>("explorer_details", { connectionId, database, schema, name, refresh });
export const refreshExplorer = (connectionId: string, database?: string, schema?: string) =>
  invoke<void>("explorer_refresh", { connectionId, database: database ?? null, schema: schema ?? null });
export const getTreeState = (connectionId: string) => invoke<string[]>("tree_state_get", { connectionId });
export const setTreeState = (connectionId: string, expanded: string[]) => invoke<void>("tree_state_set", { connectionId, expanded });
