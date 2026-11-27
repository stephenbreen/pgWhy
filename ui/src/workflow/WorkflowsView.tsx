// Workflows sidebar: saved Refresh & Workflows v2 pipelines, one action to run each.

import { useEffect } from "react";
import { useConnections } from "../connection/store";
import { effectiveSource, type RefreshWorkflow } from "../lib/ipc/workflow";
import { useWorkflows } from "./store";
import { openWorkflowEditor } from "./WorkflowEditor";
import { runHandlers } from "./runRegistry";

function basename(path: string): string {
  const parts = path.split(/[\\/]/).filter(Boolean);
  return parts[parts.length - 1] ?? path;
}

export function WorkflowsView() {
  const { loaded, workflows, load } = useWorkflows();
  const entries = useConnections((s) => s.entries);
  useEffect(() => {
    if (!loaded) void load().catch(() => {});
  }, [loaded, load]);
  const name = (id: string) => entries.find((e) => e.id === id)?.name ?? "missing connection";
  const tag = (id: string) => entries.find((e) => e.id === id)?.env_tag;

  const describeFlow = (wf: RefreshWorkflow): string => {
    const target = `${name(wf.target.connection)}/${wf.target.database}`;
    const src = effectiveSource(wf);
    switch (src.mode) {
      case "from_remote_db":
        return `${name(src.source.connection)}/${src.source.database} → ${target}`;
      case "from_backup_file":
        if (src.archive.kind === "fixed_path") {
          return `Backup (${basename(src.archive.path)}) → ${target}`;
        }
        if (src.archive.kind === "prompt_at_run") {
          return `Backup file → ${target}`;
        }
        return `Latest backup (${name(src.archive.connection)}/${src.archive.database}) → ${target}`;
      case "in_place_target":
        return `In-place · ${target}`;
    }
  };

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
          <p>No workflows yet. Build repeatable restore, data cleanup, test-user seeding, or anonymization pipelines.</p>
          <button type="button" className="primary" onClick={() => openWorkflowEditor()}>New workflow</button>
        </div>
      )}
      <ul className="script-list">
        {workflows.map((wf) => {
          const stepCount = wf.steps?.filter((s) => s.enabled).length ?? 0;
          return (
            <li key={wf.id} className="workflow-item" data-tag={tag(wf.target.connection)}>
              <div className="workflow-main">
                <span className="script-name">
                  {wf.name}
                  {stepCount > 0 && <span className="wf-step-count">{stepCount} {stepCount === 1 ? "step" : "steps"}</span>}
                </span>
                <span className="script-meta">{describeFlow(wf)}</span>
              </div>
              <div className="workflow-actions">
                <button type="button" className="primary" onClick={() => runHandlers.run?.(wf)} disabled={!runHandlers.run}>Run</button>
                <button type="button" onClick={() => runHandlers.history?.(wf)} disabled={!runHandlers.history}>History</button>
                <button type="button" className="icon" aria-label={`Edit ${wf.name}`} onClick={() => openWorkflowEditor(wf.id, wf.name)}>✎</button>
              </div>
            </li>
          );
        })}
      </ul>
    </div>
  );
}
