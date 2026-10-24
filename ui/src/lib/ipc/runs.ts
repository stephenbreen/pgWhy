// Typed wrappers over src-tauri/src/commands/runs.rs (Refresh workflow runs).

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { AppError } from "./errors";
import type { EnvTag } from "./connections";
import type { ReplaceStrategy } from "./workflow";

export type CheckStatus = "ok" | "warn" | "fail" | "skipped";
export type SecretNeed = "db_password" | "ssh_key_passphrase" | "ssh_password";
export type StepId = "dump" | "drop_target" | "create_database" | "restore" | "post_restore" | "verify" | "swap";
export type StepStatus = "pending" | "running" | "succeeded" | "failed" | "cancelled" | "skipped" | "reused" | "interrupted";
export type RunStatus = "running" | "succeeded" | "succeeded_cleanup_required" | "failed_at_step" | "failed_at_swap" | "cancelled" | "interrupted";
export type ActionKind = "retry" | "drop_staging" | "keep_staging" | "drop_old" | "rollback_swap" | "complete_swap";
export type SwapPhase = "quiescing" | "checking_prepared_transactions" | "terminating_sessions" | "renaming_target_to_old" | "renaming_staging_to_target" | "dropping_old" | "done";

export interface PreflightFix {
  kind: "drop_leftover" | "rollback_prepared";
  database: string;
  gid?: string | null;
}

export interface PreflightCheck {
  id: string;
  label: string;
  status: CheckStatus;
  message: string;
  hint?: string | null;
  fixes?: PreflightFix[];
}

export interface MissingSecret {
  connection_id: string;
  connection_name: string;
  kind: SecretNeed;
  reason: string;
}

export interface Confirmation {
  target_db: string;
  target_server_label: string;
  target_tag: EnvTag;
  strategy: ReplaceStrategy;
  create_mode: boolean;
  source_label: string;
  source_db: string;
  staging_db?: string | null;
  warnings: string[];
  remembered: boolean;
  can_remember: boolean;
  fingerprint: string;
}

/** A backup a history row can hand to the restore dialog. */
export interface HistoryBackup {
  path: string;
  connection: { id: string; name: string };
  database: string;
}

/** One row of the merged history: a Refresh run or a one-off operation. */
export interface HistoryEntry {
  id: string;
  kind: "refresh" | "create" | "rename" | "drop" | "backup" | "restore";
  name: string;
  started_at: string;
  finished_at?: string | null;
  status: RunStatus;
  headline?: string | null;
  target?: string | null;
  workflow_id?: string | null;
  workflow_name?: string | null;
  backup?: HistoryBackup | null;
  /** A record file that could not be parsed: `name`/`headline` describe it, nothing else is set. */
  unreadable?: boolean;
}

export interface RetryOffer {
  from: StepId;
  begins_at: StepId;
  reuses_backup: boolean;
  reuses_work_db: boolean;
}

export interface PreflightReport {
  workflow_id?: string | null;
  ticket?: string | null;
  checks: PreflightCheck[];
  confirmation?: Confirmation | null;
  missing_secrets: MissingSecret[];
  refused?: AppError | null;
  retry?: RetryOffer | null;
}

export interface StepRecord {
  step: StepId;
  label: string;
  status: StepStatus;
  started_at?: string | null;
  finished_at?: string | null;
  duration_ms?: number | null;
  exit_code?: number | null;
  output_tail: string[];
  log_path?: string | null;
  note?: string | null;
  error?: AppError | null;
}

export interface ActionOffer {
  kind: ActionKind;
  label: string;
  enabled: boolean;
  disabled_reason?: string | null;
  database?: string | null;
  retry?: RetryOffer | null;
}

export interface EndpointRecord {
  connection_id: string;
  connection_name: string;
  host: string;
  port: number;
  database: string;
  env_tag: EnvTag;
}

export interface RunRecord {
  run_id: string;
  workflow_id: string;
  workflow_name: string;
  retry_of?: string | null;
  started_at: string;
  finished_at?: string | null;
  status: RunStatus;
  strategy: ReplaceStrategy;
  create_mode: boolean;
  source: EndpointRecord;
  target: EndpointRecord;
  staging_db?: string | null;
  old_db?: string | null;
  steps: StepRecord[];
  backup?: { path: string; size_bytes: number; dumped_by?: string | null; reusable: boolean; reason?: string | null } | null;
  renames_completed: number;
  swap_phase?: SwapPhase | null;
  failed_step?: StepId | null;
  failed_items: { object?: string | null; error: string; command?: string | null }[];
  error?: AppError | null;
  headline?: string | null;
  actions: ActionOffer[];
  warnings: string[];
}

export interface StepProgress {
  phase?: string | null;
  current_object?: string | null;
  objects_done: number;
  objects_total?: number | null;
  total_is_estimate: boolean;
  bytes_written?: number | null;
  elapsed_ms: number;
}

export type RunEvent =
  | { event: "run_started"; run_id: string; record: RunRecord }
  | { event: "step_started"; run_id: string; step: StepId; label: string }
  | { event: "progress"; run_id: string; step: StepId; progress: StepProgress }
  | { event: "output"; run_id: string; step: StepId; line: string }
  | { event: "swap_phase"; run_id: string; phase: SwapPhase; renames_completed: number }
  | { event: "step_finished"; run_id: string; step: StepId; status: StepStatus; duration_ms: number; error?: AppError | null }
  | { event: "run_finished"; run_id: string; status: RunStatus; record: RunRecord };

export const preflight = (workflowId: string) => invoke<PreflightReport>("workflow_preflight", { workflowId });
export const retryPreflight = (runId: string, from: StepId) => invoke<PreflightReport>("workflow_retry_preflight", { runId, from });
export const discardPreflight = (ticket: string) => invoke<void>("workflow_discard_preflight", { ticket });
export const applyFix = (workflowId: string, fix: PreflightFix) => invoke<void>("workflow_apply_fix", { workflowId, fix });
export const provideSecret = (connectionId: string, kind: SecretNeed, value: string, persist: boolean) =>
  invoke<void>("workflow_provide_secret", { connectionId, kind, value, persist });
export const startRun = (ticket: string, fingerprint: string, remember: boolean) => invoke<string>("workflow_start", { ticket, fingerprint, remember });
export const cancelRun = (runId: string) => invoke<void>("workflow_cancel", { runId });
export const historyList = () => invoke<HistoryEntry[]>("history_list");

export const operationGet = (id: string) => invoke<import("./databases").OperationRecord>("operation_get", { id });

export const listRuns = (workflowId?: string) => invoke<RunRecord[]>("run_list", { workflowId: workflowId ?? null });
export const getRun = (runId: string) => invoke<RunRecord>("run_get", { runId });
export const reinventory = (runId: string) => invoke<RunRecord>("run_reinventory", { runId });
export const runAction = (runId: string, action: ActionKind) => invoke<RunRecord>("run_action", { runId, action });

export function onRunEvent(handler: (ev: RunEvent) => void): Promise<UnlistenFn> {
  return listen<RunEvent>("workflow-event", (e) => handler(e.payload));
}
