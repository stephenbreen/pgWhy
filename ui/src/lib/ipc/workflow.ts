// Typed wrappers over src-tauri/src/commands/workflows.rs (definitions; runs are added with
// the engine integration).

import { invoke } from "@tauri-apps/api/core";

export type ReplaceStrategy = "staging_swap" | "drop_first";
export type RestoreErrorPolicy = "stop_on_first" | "continue_then_fail";
export type ScriptMode = "transaction" | "no_transaction";

export interface DbRef {
  connection: string;
  database: string;
}

export interface RefreshOptions {
  strategy: ReplaceStrategy;
  exclude_table_data: string[];
  jobs?: number | null;
  no_owner: boolean;
  no_privileges: boolean;
  error_policy: RestoreErrorPolicy;
  script_mode: ScriptMode;
  sanity_query?: string | null;
}

export interface RefreshWorkflow {
  id: string;
  name: string;
  source: DbRef;
  target: DbRef;
  post_restore_script?: string | null;
  options: RefreshOptions;
  remembered_confirmation?: { fingerprint: string; remembered_at: string } | null;
  has_succeeded: boolean;
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
});

export const workflowList = () => invoke<RefreshWorkflow[]>("workflow_list");
export const workflowSave = (workflow: RefreshWorkflow) => invoke<RefreshWorkflow>("workflow_save", { workflow });
export const workflowDelete = (id: string) => invoke<void>("workflow_delete", { id });
export const workflowCheckScript = (id: string, mode: ScriptMode) => invoke<ScriptCheck>("workflow_check_script", { id, mode });
