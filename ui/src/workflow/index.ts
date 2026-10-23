import { registerTab } from "../app/registry";
import { registerNodeAction } from "../explorer/actions";
import { registerView } from "../app/registry";
import { BackupTab, openBackup } from "./BackupTab";
import { WorkflowEditor } from "./WorkflowEditor";
import { WorkflowsView } from "./WorkflowsView";
import { RunTab, openRunTab } from "./RunTab";
import { HistoryTab, openHistory } from "./HistoryTab";
import { HistoryView, openHistoryView } from "./HistoryView";
import { OperationView } from "./OperationView";
import { runHandlers } from "./runRegistry";

export function initWorkflow() {
  registerTab("backup", BackupTab);
  registerTab("workflow-editor", WorkflowEditor);
  registerView("workflows", WorkflowsView);
  registerTab("workflow-run", RunTab);
  registerTab("workflow-history", HistoryTab);
  registerTab("history", HistoryView);
  registerTab("operation", OperationView);
  runHandlers.run = (wf) => openRunTab({ workflowId: wf.id, workflowName: wf.name });
  runHandlers.history = (wf) => openHistory(wf.id, wf.name);
  runHandlers.allHistory = () => openHistoryView();
  registerNodeAction({
    id: "backup",
    label: "Back up…",
    applies: (t) => t.type === "database",
    run: (t) => {
      if (t.type === "database") openBackup(t.connectionId, t.db.name);
    },
  });
  // Scoped backups come from the node itself, so there is no schema/table picker to get wrong.
  registerNodeAction({
    id: "backup-schema",
    label: "Back up this schema…",
    applies: (t) => t.type === "schema",
    run: (t) => {
      if (t.type === "schema")
        openBackup(t.connectionId, t.db, { kind: "schema", schema: t.schema });
    },
  });
  registerNodeAction({
    id: "backup-table",
    label: "Back up this table…",
    applies: (t) => t.type === "relation",
    run: (t) => {
      if (t.type === "relation")
        openBackup(t.connectionId, t.db, {
          kind: "tables",
          tables: [{ schema: t.rel.schema, name: t.rel.name }],
        });
    },
  });
}
