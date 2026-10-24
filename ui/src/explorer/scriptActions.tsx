// Built-in explorer actions: New query, Script as, Refresh.

import { openEditorTab } from "../editor/EditorTab";
import * as ex from "../lib/ipc/explorer";
import { toAppError } from "../lib/ipc/errors";
import { registerNodeAction, type NodeTarget } from "./actions";
import { useTrees } from "./tree/model";

const dbOf = (t: NodeTarget) => (t.type === "database" ? t.db.name : t.db);

/** Script-as output goes to the active editor tab when it's on the same connection, else a new tab. */
async function scriptInto(t: NodeTarget & { type: "relation" }, kind: ex.ScriptKind) {
  try {
    const sql = await ex.scriptAs(t.connectionId, t.db, t.rel.schema, t.rel.name, kind);
    openEditorTab(t.connectionId, t.db, { text: sql, title: `${t.rel.name} (${kind})` });
  } catch (e) {
    const err = toAppError(e);
    openEditorTab(t.connectionId, t.db, { text: `-- Script as ${kind.toUpperCase()} failed: ${err.message}${err.hint ? `\n-- ${err.hint}` : ""}\n`, title: `${t.rel.name} (${kind})` });
  }
}

const SCRIPT_KINDS: { kind: ex.ScriptKind; label: string }[] = [
  { kind: "select", label: "SELECT" },
  { kind: "insert", label: "INSERT" },
  { kind: "update", label: "UPDATE" },
  { kind: "delete", label: "DELETE" },
  { kind: "create", label: "CREATE (DDL)" },
];

function dmlReason(t: NodeTarget, kind: ex.ScriptKind): string | null {
  if (t.type !== "relation") return null;
  const k = t.rel.kind;
  if (k === "sequence" && kind !== "select" && kind !== "create") return "sequences have no rows to change";
  if (k === "matview" && (kind === "insert" || kind === "update" || kind === "delete")) return "materialized views can't be changed directly";
  return null;
}

export function registerExplorerActions() {
  registerNodeAction({
    id: "new-query",
    label: "New query",
    applies: (t) => t.type === "database" || t.type === "schema" || t.type === "relation",
    run: (t) => void openEditorTab(t.connectionId, dbOf(t)),
  });
  for (const { kind, label } of SCRIPT_KINDS) {
    registerNodeAction({
      id: `script-as-${kind}`,
      label,
      group: "Script as",
      applies: (t) => t.type === "relation",
      disabledReason: (t) => dmlReason(t, kind),
      run: (t) => scriptInto(t as NodeTarget & { type: "relation" }, kind),
    });
  }
  registerNodeAction({
    id: "refresh",
    label: "Refresh",
    applies: (t) => t.type === "database" || t.type === "schema",
    run: (t) => {
      const trees = useTrees.getState();
      if (t.type === "schema") void trees.loadContents(t.connectionId, t.db, t.schema, true);
      else if (t.type === "database") {
        const db = t.db.name;
        void ex.refreshExplorer(t.connectionId, db).then(() => trees.invalidate(t.connectionId, db));
      }
    },
  });
}
