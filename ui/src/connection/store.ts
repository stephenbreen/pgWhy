// Connection list + live status. The status per entry is derived from connect
// results and core `conn-event`s; the active connection is what the status bar shows and
// what new editor tabs inherit.

import { create } from "zustand";
import * as ipc from "../lib/ipc/connections";
import { toAppError, type AppError } from "../lib/ipc/errors";

export type ConnStatus =
  | { state: "disconnected" }
  | { state: "connecting" }
  | { state: "connected"; info: ipc.ConnInfo }
  | { state: "failed"; error: AppError };

interface ConnectionsState {
  loaded: boolean;
  entries: ipc.ConnectionEntry[];
  recents: string[];
  keyringAvailable: boolean;
  status: Record<string, ConnStatus>;
  tunnels: Record<string, ipc.TunnelStatus>;
  /** live core connections per entry → database → count (tree "open database" marks) */
  openDbs: Record<string, Record<string, number>>;
  refreshLive: () => Promise<void>;
  activeId: string | null;
  load: () => Promise<void>;
  save: (entry: ipc.ConnectionEntry, password: ipc.SecretInput, ssh: ipc.SecretInput) => Promise<ipc.SaveResult>;
  remove: (id: string) => Promise<void>;
  connect: (id: string, password?: ipc.SecretInput, sshSecret?: ipc.SecretInput) => Promise<boolean>;
  applyTunnel: (s: ipc.TunnelStatus) => void;
  disconnect: (id: string) => Promise<void>;
  setActive: (id: string | null) => void;
  applyEvent: (ev: ipc.ConnEvent) => void;
}

export const useConnections = create<ConnectionsState>((set, get) => ({
  loaded: false,
  entries: [],
  recents: [],
  keyringAvailable: true,
  status: {},
  tunnels: {},
  openDbs: {},
  activeId: null,

  // The core is the source of truth for what's connected; re-read it whenever a connection
  // opens or closes (cheap: an in-memory list) instead of mirroring events by hand.
  refreshLive: async () => {
    let live: ipc.ConnInfo[];
    try {
      live = await ipc.liveConnections();
    } catch {
      return;
    }
    const openDbs: Record<string, Record<string, number>> = {};
    for (const c of live ?? []) {
      // short-lived maintenance/test connections don't make a database "open"
      if (c.purpose === "maintenance" || c.purpose === "test") continue;
      const dbs = (openDbs[c.connection_id] ??= {});
      dbs[c.database] = (dbs[c.database] ?? 0) + 1;
    }
    set({ openDbs });
  },

  load: async () => {
    const snap = await ipc.listConnections();
    set({ loaded: true, entries: snap.connections, recents: snap.recents, keyringAvailable: snap.keyring_available });
  },

  save: async (entry, password, ssh) => {
    const res = await ipc.saveConnection(entry, password, ssh);
    set((s) => {
      const exists = s.entries.some((e) => e.id === res.entry.id);
      const entries = exists ? s.entries.map((e) => (e.id === res.entry.id ? res.entry : e)) : [...s.entries, res.entry];
      return { entries };
    });
    return res;
  },

  remove: async (id) => {
    await ipc.deleteConnection(id);
    set((s) => {
      const status = { ...s.status };
      delete status[id];
      return {
        entries: s.entries.filter((e) => e.id !== id),
        recents: s.recents.filter((r) => r !== id),
        status,
        activeId: s.activeId === id ? null : s.activeId,
      };
    });
  },

  connect: async (id, password, sshSecret) => {
    set((s) => ({ status: { ...s.status, [id]: { state: "connecting" } } }));
    try {
      const info = await ipc.connect(id, password, sshSecret);
      set((s) => ({
        status: { ...s.status, [id]: { state: "connected", info } },
        recents: [id, ...s.recents.filter((r) => r !== id)].slice(0, 10),
        activeId: id,
      }));
      void get().refreshLive();
      return true;
    } catch (e) {
      set((s) => ({ status: { ...s.status, [id]: { state: "failed", error: toAppError(e) } } }));
      return false;
    }
  },

  disconnect: async (id) => {
    await ipc.disconnect(id);
    set((s) => ({ status: { ...s.status, [id]: { state: "disconnected" } } }));
    void get().refreshLive();
  },

  setActive: (id) => set({ activeId: id }),

  applyTunnel: (t) => set((s) => ({ tunnels: { ...s.tunnels, [t.connection_id]: t } })),

  // A lost explorer connection flips the entry to failed (it reopens transparently on the
  // next tree interaction); dedicated tab/grid connections are handled by their owners.
  applyEvent: (ev) => {
    void get().refreshLive();
    if (ev.event !== "closed" || ev.purpose !== "introspection") return;
    const cur = get().status[ev.connection_id];
    if (cur?.state !== "connected" || cur.info.handle !== ev.handle) return;
    if (ev.close.reason === "lost") {
      set((s) => ({
        status: {
          ...s.status,
          [ev.connection_id]: {
            state: "failed",
            error: { kind: "disconnected", class: "retriable", message: "The connection was lost", detail: ev.close.reason === "lost" ? (ev.close.error ?? undefined) : undefined },
          },
        },
      }));
    } else if (ev.close.reason === "disconnected" || ev.close.reason === "entry_changed") {
      set((s) => ({ status: { ...s.status, [ev.connection_id]: { state: "disconnected" } } }));
    }
  },
}));

export function entryById(id: string | null | undefined): ipc.ConnectionEntry | undefined {
  if (!id) return undefined;
  return useConnections.getState().entries.find((e) => e.id === id);
}

/** Group entries by their group path, ungrouped first, both sorted by name. */
export function groupEntries(entries: ipc.ConnectionEntry[]): { group: string | null; entries: ipc.ConnectionEntry[] }[] {
  const byGroup = new Map<string | null, ipc.ConnectionEntry[]>();
  for (const e of entries) {
    const g = e.group?.trim() || null;
    byGroup.set(g, [...(byGroup.get(g) ?? []), e]);
  }
  const groups = [...byGroup.entries()].map(([group, es]) => ({
    group,
    entries: es.sort((a, b) => a.name.localeCompare(b.name)),
  }));
  return groups.sort((a, b) => (a.group === null ? -1 : b.group === null ? 1 : a.group.localeCompare(b.group)));
}
