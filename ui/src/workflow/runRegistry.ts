// The run/history entry points, provided by the run view once the engine is wired.
import type { RefreshWorkflow } from "../lib/ipc/workflow";

export const runHandlers: {
  run?: (wf: RefreshWorkflow) => void;
  history?: (wf: RefreshWorkflow) => void;
  allHistory?: () => void;
} = {};
