// Mirrors crates/pgwhy-errors: the single error taxonomy (flow ER-3).

export type ErrorClass = "retriable" | "fix_then_retry" | "fatal";

export type ErrorKind =
  | "network" | "auth" | "tls" | "tunnel" | "host_key" | "sql" | "permission"
  | "tool_missing" | "tool_version" | "tool_failed" | "disk" | "credentials" | "config"
  | "disconnected" | "cancelled" | "refused" | "internal";

export interface AppError {
  kind: ErrorKind;
  class: ErrorClass;
  message: string;
  hint?: string;
  detail?: string;
  /** structured payload for UI affordances (host-key prompt, blocking pids, ...) */
  data?: unknown;
}

export function isAppError(e: unknown): e is AppError {
  return typeof e === "object" && e !== null && "kind" in e && "class" in e && "message" in e;
}

/** Normalize anything thrown by `invoke` into an AppError. */
export function toAppError(e: unknown): AppError {
  if (isAppError(e)) return e;
  return { kind: "internal", class: "fatal", message: String(e) };
}
