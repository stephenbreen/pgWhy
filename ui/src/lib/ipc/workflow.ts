// Typed wrappers over src-tauri/src/commands/workflows.rs (v1 Refresh & Workflows v2 pipelines).

import { invoke } from "@tauri-apps/api/core";

export const WORKFLOW_SCHEMA_VERSION = 2;

export type ReplaceStrategy = "staging_swap" | "drop_first";
export type RestoreErrorPolicy = "stop_on_first" | "continue_then_fail";
export type ScriptMode = "transaction" | "no_transaction";

export interface DbRef {
  connection: string;
  database: string;
}

export type BackupSource =
  | { kind: "fixed_path"; path: string }
  | { kind: "prompt_at_run"; param?: string | null }
  | { kind: "latest_from_history"; connection: string; database: string };

export type WorkflowSource =
  | { mode: "from_remote_db"; source: DbRef }
  | { mode: "from_backup_file"; archive: BackupSource }
  | { mode: "in_place_target"; pre_run_backup: boolean };

export type ParamType = "text" | "number" | "file_path" | "secret";

export interface WorkflowParam {
  name: string;
  label?: string | null;
  description?: string | null;
  kind: ParamType;
  required: boolean;
  default?: string | null;
}

export type LogicalOp = "and" | "or";

export type FilterOp =
  | "eq"
  | "neq"
  | "lt"
  | "lte"
  | "gt"
  | "gte"
  | "like"
  | "ilike"
  | "not_like"
  | "not_ilike"
  | "in"
  | "not_in"
  | "is_null"
  | "is_not_null";

export type CompareOp = "eq" | "neq" | "lt" | "lte" | "gt" | "gte";

export type CellValue =
  | { kind: "literal"; value: string }
  | { kind: "number"; value: string }
  | { kind: "bool"; value: boolean }
  | { kind: "null" }
  | { kind: "default" }
  | { kind: "param"; name: string }
  | { kind: "expr"; sql: string }
  | { kind: "list"; items: CellValue[] };

export interface Predicate {
  column: string;
  op: FilterOp;
  value?: CellValue | null;
}

export type DeleteMode =
  | { mode: "delete_where"; conditions: Predicate[]; combinator: LogicalOp }
  | { mode: "truncate"; restart_identity: boolean; cascade: boolean };

export type ConflictPolicy =
  | { mode: "error" }
  | { mode: "do_nothing"; columns?: string[] }
  | { mode: "do_update"; columns: string[]; update_columns?: string[] };

export type MaskTransform =
  | { kind: "synthetic_email"; domain: string; id_column?: string | null }
  | { kind: "fixed_value"; value: CellValue }
  | { kind: "set_null" }
  | { kind: "sha256_hash"; salt?: CellValue | null }
  | { kind: "custom_expr"; sql: string };

export interface MaskAssignment {
  column: string;
  transform: MaskTransform;
}

export type SqlStepBody =
  | { source: "inline"; sql: string }
  | { source: "library"; script_id: string };

export type CustomVerifyExpect = "succeeds" | "truthy_scalar" | "zero_rows";

export type VerifyAssertion =
  | { kind: "row_count"; schema: string; table: string; op: CompareOp; expected: number; conditions: Predicate[]; combinator: LogicalOp }
  | { kind: "zero_rows"; schema: string; table: string; conditions: Predicate[]; combinator: LogicalOp }
  | { kind: "custom_sql"; sql: string; expect: CustomVerifyExpect };

export interface WorkflowStepBase {
  id: string;
  name: string;
  enabled: boolean;
  no_transaction: boolean;
}

export type DeleteRowsStep = WorkflowStepBase & { kind: "delete_rows"; schema: string; table: string } & DeleteMode;
export type InsertRowsStep = WorkflowStepBase & {
  kind: "insert_rows";
  schema: string;
  table: string;
  columns: string[];
  rows: CellValue[][];
  on_conflict: ConflictPolicy;
};
export type UpdateMaskStep = WorkflowStepBase & {
  kind: "update_mask";
  schema: string;
  table: string;
  conditions: Predicate[];
  combinator: LogicalOp;
  assignments: MaskAssignment[];
};
export type SqlStep = WorkflowStepBase & { kind: "sql" } & SqlStepBody;
export type VerifyStep = WorkflowStepBase & { kind: "verify"; assertion: VerifyAssertion };

export type WorkflowStep = DeleteRowsStep | InsertRowsStep | UpdateMaskStep | SqlStep | VerifyStep;

export interface RefreshOptions {
  strategy: ReplaceStrategy;
  exclude_table_data: string[];
  jobs?: number | null;
  no_owner: boolean;
  no_privileges: boolean;
  error_policy: RestoreErrorPolicy;
  script_mode: ScriptMode;
  sanity_query?: string | null;
  pause_before_swap?: boolean;
}

export interface RefreshWorkflow {
  schema_version?: number;
  id: string;
  name: string;
  source: DbRef;
  source_mode?: WorkflowSource | null;
  target: DbRef;
  post_restore_script?: string | null;
  parameters?: WorkflowParam[];
  steps?: WorkflowStep[];
  options: RefreshOptions;
  remembered_confirmation?: { fingerprint: string; remembered_at: string } | null;
  has_succeeded: boolean;
}

export interface CompiledStep {
  step_id: string;
  step_name: string;
  sql: string;
  no_transaction: boolean;
  is_verify: boolean;
  warnings: string[];
  problems: string[];
  referenced_params: string[];
}

export interface WorkflowTemplate {
  id: string;
  name: string;
  description: string;
  workflow: RefreshWorkflow;
}

export interface ScriptCheck {
  ok: boolean;
  problems: string[];
  needs_no_transaction: string[];
  content_hash?: string | null;
}

export const defaultOptions = (): RefreshOptions => ({
  strategy: "staging_swap",
  exclude_table_data: [],
  jobs: null,
  no_owner: true,
  no_privileges: true,
  error_policy: "stop_on_first",
  script_mode: "transaction",
  sanity_query: null,
  pause_before_swap: false,
});

export function effectiveSource(wf: RefreshWorkflow): WorkflowSource {
  return wf.source_mode ?? { mode: "from_remote_db", source: wf.source };
}

export function isUnaryOp(op: FilterOp): boolean {
  return op === "is_null" || op === "is_not_null";
}

export function isListOp(op: FilterOp): boolean {
  return op === "in" || op === "not_in";
}

export function createBlankStep(kind: WorkflowStep["kind"]): WorkflowStep {
  const base: WorkflowStepBase = {
    id: crypto.randomUUID(),
    name: "",
    enabled: true,
    no_transaction: false,
  };
  switch (kind) {
    case "delete_rows":
      return {
        ...base,
        name: "Delete rows",
        kind: "delete_rows",
        schema: "public",
        table: "",
        mode: "delete_where",
        conditions: [],
        combinator: "and",
      };
    case "insert_rows":
      return {
        ...base,
        name: "Insert rows",
        kind: "insert_rows",
        schema: "public",
        table: "",
        columns: ["email", "role"],
        rows: [[{ kind: "literal", value: "test@example.test" }, { kind: "literal", value: "user" }]],
        on_conflict: { mode: "do_nothing" },
      };
    case "update_mask":
      return {
        ...base,
        name: "Update / Mask columns",
        kind: "update_mask",
        schema: "public",
        table: "",
        conditions: [],
        combinator: "and",
        assignments: [{ column: "email", transform: { kind: "synthetic_email", domain: "example.test" } }],
      };
    case "sql":
      return {
        ...base,
        name: "SQL step",
        kind: "sql",
        source: "inline",
        sql: "-- Write SQL here (use {{param_name}} for parameters)\n",
      };
    case "verify":
      return {
        ...base,
        name: "Verify result",
        kind: "verify",
        assertion: {
          kind: "row_count",
          schema: "public",
          table: "",
          op: "gte",
          expected: 1,
          conditions: [],
          combinator: "and",
        },
      };
  }
}

export const workflowList = () => invoke<RefreshWorkflow[]>("workflow_list");
export const workflowSave = (workflow: RefreshWorkflow) => invoke<RefreshWorkflow>("workflow_save", { workflow });
export const workflowDelete = (id: string) => invoke<void>("workflow_delete", { id });
export const workflowCheckScript = (id: string, mode: ScriptMode) => invoke<ScriptCheck>("workflow_check_script", { id, mode });
export const workflowPreviewSql = (workflow: RefreshWorkflow, params?: Record<string, string>) =>
  invoke<CompiledStep[]>("workflow_preview_sql", params ? { workflow, params } : { workflow });
export const workflowConvertStepToSql = (workflow: RefreshWorkflow, stepId: string) =>
  invoke<WorkflowStep>("workflow_convert_step_to_sql", { workflow, stepId });
export const workflowTemplates = () => invoke<WorkflowTemplate[]>("workflow_templates");
export const workflowExportToml = (id: string) => invoke<string>("workflow_export_toml", { id });
export const workflowImportToml = (tomlText: string) => invoke<RefreshWorkflow>("workflow_import_toml", { tomlText });
export const workflowArchiveTables = (path: string) => invoke<[string, string][]>("workflow_archive_tables", { path });
