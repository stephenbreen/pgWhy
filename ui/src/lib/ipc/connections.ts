// Typed wrappers over src-tauri/src/commands/connections.rs.

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type EnvTag = "local" | "dev" | "staging" | "production";
export type SslMode = "disable" | "prefer" | "require" | "verify-ca" | "verify-full";
export type TlsState = "plaintext" | "encrypted_unverified" | "encrypted_verified";
export type Purpose = "introspection" | "editor_tab" | "grid" | "workflow" | "maintenance" | "test";

export type TunnelAuth =
  | { method: "key"; key_path: string; passphrase_saved?: boolean }
  | { method: "password"; password_saved?: boolean };

export interface TunnelConfig {
  host: string;
  port: number;
  user: string;
  auth: TunnelAuth;
}

export interface ConnectionEntry {
  id: string;
  name: string;
  group?: string;
  host: string;
  port: number;
  database: string;
  user: string;
  maintenance_db: string;
  sslmode: SslMode;
  ssl_root_cert?: string;
  tunnel?: TunnelConfig;
  env_tag: EnvTag;
  password_saved: boolean;
}

export type SecretInput =
  | { action: "unchanged" }
  | { action: "clear" }
  | { action: "set"; value: string; persist: boolean };

export interface ConnectionsSnapshot {
  connections: ConnectionEntry[];
  recents: string[];
  keyring_available: boolean;
}

export interface SaveResult {
  entry: ConnectionEntry;
  warnings: string[];
  password_stored?: "keyring" | "session" | "none" | null;
}

export interface ConnInfo {
  handle: number;
  connection_id: string;
  database: string;
  purpose: Purpose;
  server_version_num: number;
  backend_pid: number;
  tls: TlsState;
}

export type CloseReason =
  | { reason: "lost"; error?: string | null }
  | { reason: "quiesced" }
  | { reason: "idle" }
  | { reason: "disconnected" }
  | { reason: "entry_changed" };

export type ConnEvent =
  | { event: "opened"; info: ConnInfo }
  | {
      event: "closed";
      handle: number;
      connection_id: string;
      database: string;
      purpose: Purpose;
      close: CloseReason;
    };

export const listConnections = () => invoke<ConnectionsSnapshot>("list_connections");

export const saveConnection = (entry: ConnectionEntry, password: SecretInput, sshSecret: SecretInput) =>
  invoke<SaveResult>("save_connection", { entry, password, sshSecret });

export const deleteConnection = (id: string) => invoke<void>("delete_connection", { id });

export const testConnection = (entry: ConnectionEntry, password?: string) =>
  invoke<ConnInfo>("test_connection", { entry, password: password ?? null });

export const connect = (id: string, password?: SecretInput, sshSecret?: SecretInput) =>
  invoke<ConnInfo>("connect", { id, password: password ?? null, sshSecret: sshSecret ?? null });

export type TunnelState = "starting" | "up" | "reconnecting" | "down";

export interface TunnelStatus {
  connection_id: string;
  state: TunnelState;
  local_port?: number | null;
  leases: number;
  error?: import("./errors").AppError | null;
  attempt?: number | null;
}

export interface HostKeyPrompt {
  host: string;
  port: number;
  key_type: string;
  fingerprint: string;
  status: "unknown" | "changed";
  previous_key_type?: string;
  previous_fingerprint?: string;
}

export const trustHostKey = (p: HostKeyPrompt) =>
  invoke<void>("trust_host_key", { host: p.host, port: p.port, keyType: p.key_type, fingerprint: p.fingerprint });

export function onTunnelEvent(handler: (s: TunnelStatus) => void): Promise<UnlistenFn> {
  return listen<TunnelStatus>("tunnel-event", (e) => handler(e.payload));
}

export const disconnect = (id: string) => invoke<void>("disconnect", { id });

export const liveConnections = () => invoke<ConnInfo[]>("live_connections");

export function onConnEvent(handler: (ev: ConnEvent) => void): Promise<UnlistenFn> {
  return listen<ConnEvent>("conn-event", (e) => handler(e.payload));
}

export function newConnectionEntry(): ConnectionEntry {
  return {
    id: crypto.randomUUID(),
    name: "",
    host: "localhost",
    port: 5432,
    database: "postgres",
    user: "",
    maintenance_db: "postgres",
    sslmode: "prefer",
    env_tag: "production",
    password_saved: false,
  };
}

export function serverVersionLabel(num: number): string {
  const major = Math.floor(num / 10000);
  const minor = num % 10000;
  return `PostgreSQL ${major}.${minor}`;
}
