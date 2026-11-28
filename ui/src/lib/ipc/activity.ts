// Typed wrappers over src-tauri/src/commands/activity.rs (pg_stat_activity + pg_locks).

import { invoke } from "@tauri-apps/api/core";

export interface SessionInfo {
  pid: number;
  database: string;
  username: string;
  application_name: string;
  client_addr: string;
  state: string;
  wait_event_type: string;
  wait_event: string;
  backend_type: string;
  query_duration_secs?: number | null;
  xact_duration_secs?: number | null;
  state_duration_secs?: number | null;
  query: string;
  blocking_pids: number[];
  is_self: boolean;
}

export interface LockInfo {
  pid: number;
  database: string;
  lock_type: string;
  relation: string;
  mode: string;
  granted: boolean;
  fastpath: boolean;
  virtual_xid: string;
}

export interface ActivitySnapshot {
  sessions: SessionInfo[];
  locks: LockInfo[];
  captured_at_ms: number;
  server_version: string;
  total_count: number;
  active_count: number;
  idle_in_txn_count: number;
  blocked_count: number;
}

export const activitySnapshot = (connectionId: string, database?: string | null) =>
  invoke<ActivitySnapshot>("activity_snapshot", { connectionId, database: database ?? null });

export const cancelBackend = (connectionId: string, pid: number) =>
  invoke<boolean>("activity_cancel_backend", { connectionId, pid });

export const terminateBackend = (connectionId: string, pid: number) =>
  invoke<boolean>("activity_terminate_backend", { connectionId, pid });
