// The details dock follows the explorer selection (R4): serve-cached on select, refresh on
// demand, "no longer exists" when a refetch misses (post-DDL / post-swap).

import { create } from "zustand";
import * as ex from "../../lib/ipc/explorer";
import { toAppError, type AppError } from "../../lib/ipc/errors";
import type { NodeTarget } from "../actions";

interface DetailsState {
  visible: boolean;
  target: NodeTarget | null;
  details: ex.RelationDetails | null;
  missing: boolean;
  loading: boolean;
  error: AppError | null;
  show: (visible: boolean) => void;
  select: (t: NodeTarget) => Promise<void>;
  refresh: () => Promise<void>;
}

let seq = 0;

async function load(t: NodeTarget, refresh: boolean) {
  if (t.type !== "relation") return { details: null, missing: false };
  const d = await ex.relationDetails(t.connectionId, t.db, t.rel.schema, t.rel.name, refresh);
  return { details: d, missing: d === null };
}

export const useDetails = create<DetailsState>((set, get) => ({
  visible: true,
  target: null,
  details: null,
  missing: false,
  loading: false,
  error: null,
  show: (visible) => set({ visible }),
  select: async (t) => {
    const my = ++seq;
    set({ target: t, loading: true, error: null, missing: false, details: null });
    try {
      const r = await load(t, false);
      if (my === seq) set({ ...r, loading: false });
    } catch (e) {
      if (my === seq) set({ loading: false, error: toAppError(e) });
    }
  },
  refresh: async () => {
    const t = get().target;
    if (!t) return;
    const my = ++seq;
    set({ loading: true, error: null });
    try {
      const r = await load(t, true);
      if (my === seq) set({ ...r, loading: false });
    } catch (e) {
      if (my === seq) set({ loading: false, error: toAppError(e) });
    }
  },
}));
