// Local searchable query execution history (persisted in localStorage, newest first).

import { create } from "zustand";

export type QueryHistoryKind = "run" | "explain" | "explain_analyze";

export interface QueryHistoryEntry {
  id: string;
  timestamp: number;
  connectionId: string;
  connectionName: string;
  database: string;
  sql: string;
  durationMs: number;
  statements: number;
  errors: number;
  cancelled: boolean;
  kind: QueryHistoryKind;
}

const KEY = "pgwhy.queryHistory";
const MAX_ENTRIES = 250;

function load(): QueryHistoryEntry[] {
  try {
    const raw = JSON.parse(localStorage.getItem(KEY) ?? "[]");
    if (!Array.isArray(raw)) return [];
    return raw.filter(
      (e): e is QueryHistoryEntry =>
        !!e && typeof e.id === "string" && typeof e.sql === "string" && typeof e.timestamp === "number",
    );
  } catch {
    return [];
  }
}

function save(entries: QueryHistoryEntry[]) {
  try {
    localStorage.setItem(KEY, JSON.stringify(entries.slice(0, MAX_ENTRIES)));
  } catch {
    // storage unavailable or full
  }
}

interface QueryHistoryState {
  entries: QueryHistoryEntry[];
  record: (entry: Omit<QueryHistoryEntry, "id" | "timestamp">) => void;
  remove: (id: string) => void;
  clear: () => void;
}

export const useQueryHistory = create<QueryHistoryState>((set) => ({
  entries: load(),
  record: (entry) =>
    set((s) => {
      const sql = entry.sql.trim();
      if (!sql) return s;
      const item: QueryHistoryEntry = {
        ...entry,
        sql,
        id: crypto.randomUUID(),
        timestamp: Date.now(),
      };
      const entries = [item, ...s.entries].slice(0, MAX_ENTRIES);
      save(entries);
      return { entries };
    }),
  remove: (id) =>
    set((s) => {
      const entries = s.entries.filter((e) => e.id !== id);
      save(entries);
      return { entries };
    }),
  clear: () => {
    save([]);
    set({ entries: [] });
  },
}));
