// Workflows sidebar: saved Refresh workflows, one action to run each.

import { useEffect } from "react";
import { useConnections } from "../connection/store";
import { useWorkflows } from "./store";
import { openWorkflowEditor } from "./WorkflowEditor";
import { runHandlers } from "./runRegistry";

export function WorkflowsView() {
  const { loaded, workflows, load } = useWorkflows();
  const entries = useConnections((s) => s.entries);
  useEffect(() => {
    if (!loaded) void load().catch(() => {});
  }, [loaded, load]);
  const name = (id: string) => entries.find((e) => e.id === id)?.name ?? "missing connection";
  const tag = (id: string) => entries.find((e) => e.id === id)?.env_tag;
  return (
    <div className="workflows-view">
      <div className="list-toolbar">
        <button type="button" className="icon" aria-label="New workflow" title="New workflow" onClick={() => openWorkflowEditor()}>＋</button>
        <button
          type="button"
          className="link"
          aria-label="All history"
          onClick={() => runHandlers.allHistory?.()}
          disabled={!runHandlers.allHistory}
        >
          History
        </button>
      </div>
      {loaded && workflows.length === 0 && (
        <div className="empty">
          <p>No workflows yet. A Refresh workflow copies a remote database over a local one in one action.</p>
          <button type="button" className="primary" onClick={() => openWorkflowEditor()}>New workflow</button>
        </div>
      )}
      <ul className="script-list">
        {workflows.map((wf) => (
          <li key={wf.id} className="workflow-item" data-tag={tag(wf.target.connection)}>
            <div className="workflow-main">
              <span className="script-name">{wf.name}</span>
              <span className="script-meta">
                {name(wf.source.connection)}/{wf.source.database} → {name(wf.target.connection)}/{wf.target.database}
              </span>
            </div>
            <div className="workflow-actions">
              <button type="button" className="primary" onClick={() => runHandlers.run?.(wf)} disabled={!runHandlers.run}>Run</button>
              <button type="button" onClick={() => runHandlers.history?.(wf)} disabled={!runHandlers.history}>History</button>
              <button type="button" className="icon" aria-label={`Edit ${wf.name}`} onClick={() => openWorkflowEditor(wf.id, wf.name)}>✎</button>
            </div>
          </li>
        ))}
      </ul>
    </div>
  );
}
