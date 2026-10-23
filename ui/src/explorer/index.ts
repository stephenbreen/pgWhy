import { registerView } from "../app/registry";
import { setSelectHandler } from "./actions";
import { useDetails } from "./details/store";
import { ExplorerView } from "./ExplorerView";
import { registerDatabaseActions } from "./DatabaseOps";
import { registerRestoreActions } from "./RestoreDialog";
import { registerExplorerActions } from "./scriptActions";
import { invoke } from "@tauri-apps/api/core";
import { setCompletionLoader } from "../editor/completion";
import type { SQLNamespace } from "@codemirror/lang-sql";

export function initExplorer() {
  registerView("explorer", ExplorerView);
  registerExplorerActions();
  registerDatabaseActions();
  registerRestoreActions();
  setSelectHandler((t) => void useDetails.getState().select(t));
  // schema.table.column completion from whatever the explorer has loaded; `public` objects
  // also complete unqualified
  setCompletionLoader(async (connectionId, database) => {
    const ns = await invoke<Record<string, Record<string, string[]>>>("completion_schema", { connectionId, database });
    const out: Record<string, SQLNamespace> = {};
    for (const [schema, rels] of Object.entries(ns)) {
      out[schema] = rels;
      if (schema === "public") for (const [rel, cols] of Object.entries(rels)) if (!(rel in out)) out[rel] = cols;
    }
    return out;
  });
}

export { DetailsPanel } from "./details/DetailsPanel";
export { DatabaseOps } from "./DatabaseOps";
export { RestoreDialog } from "./RestoreDialog";
