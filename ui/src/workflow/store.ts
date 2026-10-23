import { create } from "zustand";
import * as w from "../lib/ipc/workflow";

interface WorkflowsState {
  loaded: boolean;
  workflows: w.RefreshWorkflow[];
  load: () => Promise<void>;
  upsert: (wf: w.RefreshWorkflow) => void;
  remove: (id: string) => void;
}

export const useWorkflows = create<WorkflowsState>((set) => ({
  loaded: false,
  workflows: [],
  load: async () => set({ loaded: true, workflows: await w.workflowList() }),
  upsert: (wf) =>
    set((s) => ({
      workflows: s.workflows.some((x) => x.id === wf.id) ? s.workflows.map((x) => (x.id === wf.id ? wf : x)) : [...s.workflows, wf],
    })),
  remove: (id) => set((s) => ({ workflows: s.workflows.filter((x) => x.id !== id) })),
}));
