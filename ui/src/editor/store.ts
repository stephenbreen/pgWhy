// Per-editor-tab state: its connection binding (R2), transaction state (R19), the current
// run's result sets (SSMS-style single results pane with a switcher) and messages.

import { create } from "zustand";
import type { ColumnInfo, QueryEvent, TxnState, TxnStatus } from "../lib/ipc/query";

export interface ResultSet {
  resultId: string;
  statementIndex: number;
  columns: ColumnInfo[];
  rowCount: number;
  done: boolean;
  truncated: boolean;
}

export interface Message {
  kind: "info" | "notice" | "error";
  text: string;
  detail?: string;
  statementIndex?: number;
}

export interface EditorTabState {
  connectionId: string;
  database: string;
  connected: boolean;
  /** core handle of the tab's connection, to match connection events */
  handle?: number;
  txn: TxnState;
  running: boolean;
  results: ResultSet[];
  activeResult: number;
  messages: Message[];
  /** byte offset of the last error, into the text that was run */
  errorAt: { from: number; to: number; message: string; text: string } | null;
  dirty: boolean;
  filePath?: string;
  lastRun?: { statements: number; errors: number; durationMs: number; cancelled: boolean };
}

interface EditorsState {
  tabs: Record<string, EditorTabState>;
  init: (tabId: string, connectionId: string, database: string, filePath?: string) => void;
  patch: (tabId: string, patch: Partial<EditorTabState>) => void;
  startRun: (tabId: string) => void;
  apply: (tabId: string, ev: QueryEvent, text: string) => void;
  remove: (tabId: string) => void;
}

export const useEditors = create<EditorsState>((set) => ({
  tabs: {},
  init: (tabId, connectionId, database, filePath) =>
    set((s) => ({
      tabs: {
        ...s.tabs,
        [tabId]: s.tabs[tabId] ?? {
          connectionId, database, connected: false, txn: "idle", running: false, results: [], activeResult: 0,
          messages: [], errorAt: null, dirty: false, filePath,
        },
      },
    })),
  patch: (tabId, patch) => set((s) => (s.tabs[tabId] ? { tabs: { ...s.tabs, [tabId]: { ...s.tabs[tabId], ...patch } } } : s)),
  startRun: (tabId) =>
    set((s) => {
      const t = s.tabs[tabId];
      if (!t) return s;
      return { tabs: { ...s.tabs, [tabId]: { ...t, running: true, results: [], activeResult: 0, messages: [], errorAt: null } } };
    }),
  apply: (tabId, ev, text) =>
    set((s) => {
      const t = s.tabs[tabId];
      if (!t) return s;
      const next: EditorTabState = { ...t };
      const txnOf = (st: TxnStatus): TxnState => (st === "failed" ? "failed_transaction" : st);
      switch (ev.type) {
        case "result":
          next.results = [
            ...t.results,
            { resultId: ev.result_id, statementIndex: ev.index, columns: ev.columns, rowCount: 0, done: false, truncated: false },
          ];
          break;
        case "rows":
          next.results = t.results.map((r) => (r.resultId === ev.result_id ? { ...r, rowCount: ev.row_count, truncated: ev.truncated } : r));
          break;
        case "statement_done": {
          if (ev.result_id) {
            next.results = t.results.map((r) =>
              r.resultId === ev.result_id ? { ...r, rowCount: ev.row_count ?? r.rowCount, done: true, truncated: ev.truncated } : r,
            );
          }
          const n = ev.rows_affected;
          const rows = ev.result_id || /^(INSERT|UPDATE|DELETE|MERGE|COPY|SELECT)/.test(ev.command) ? ` — ${n} row${n === 1 ? "" : "s"}` : "";
          next.messages = [...t.messages, { kind: "info", text: `${ev.command}${rows} (${Math.round(ev.duration_ms)} ms)`, statementIndex: ev.index }];
          break;
        }
        case "error": {
          const e = ev.error;
          const detail = [e.detail && `DETAIL: ${e.detail}`, e.hint && `HINT: ${e.hint}`, e.context && `CONTEXT: ${e.context}`, e.sqlstate && `SQLSTATE ${e.sqlstate}`]
            .filter(Boolean)
            .join("\n");
          next.messages = [...t.messages, { kind: "error", text: e.message, detail: detail || undefined, statementIndex: ev.index }];
          if (ev.result_id) next.results = t.results.map((r) => (r.resultId === ev.result_id ? { ...r, done: true, rowCount: ev.row_count ?? r.rowCount } : r));
          if (e.position != null && !t.errorAt) next.errorAt = { from: e.position, to: e.position + 1, message: e.message, text };
          break;
        }
        case "notice":
          next.messages = [...t.messages, { kind: "notice", text: `${ev.notice.severity}: ${ev.notice.message}`, detail: ev.notice.detail ?? undefined }];
          break;
        case "cancelled":
          if (ev.result_id) next.results = t.results.map((r) => (r.resultId === ev.result_id ? { ...r, done: true, rowCount: ev.row_count ?? r.rowCount } : r));
          next.messages = [...t.messages, { kind: "error", text: "Query cancelled", statementIndex: ev.index }];
          break;
        case "txn":
          next.txn = txnOf(ev.status);
          break;
        case "evicted":
          next.messages = [...t.messages, { kind: "notice", text: `Released ${ev.report.evicted.length} older result set(s) to stay within memory limits` }];
          break;
        case "finished":
          next.running = false;
          next.lastRun = { statements: ev.statements, errors: ev.errors, durationMs: Math.round(ev.duration_ms), cancelled: ev.cancelled };
          if (ev.connection_lost) {
            next.connected = false;
            next.txn = "disconnected";
            next.messages = [
              ...next.messages,
              { kind: "error", text: ev.transaction_lost ? "Connection lost — the open transaction was rolled back by the server" : "Connection lost" },
            ];
          } else next.txn = txnOf(ev.txn);
          break;
        case "statement_started":
          break;
      }
      return { tabs: { ...s.tabs, [tabId]: next } };
    }),
  remove: (tabId) =>
    set((s) => {
      const tabs = { ...s.tabs };
      delete tabs[tabId];
      return { tabs };
    }),
}));
