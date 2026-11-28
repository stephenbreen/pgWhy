// SSMS-style results pane: one grid at a time with a result-set switcher, Messages,
// visual EXPLAIN Plan tree, and searchable Query Execution History.

import { useMemo } from "react";
import { ErrorNote } from "../connection/ErrorNote";
import { ResultGrid } from "../grid/ResultGrid";
import { useResultStore } from "../grid/useResultStore";
import { resultCopy } from "../lib/ipc/query";
import { ExportButton } from "../grid/ExportButton";
import { ExplainPlanView } from "./ExplainPlanView";
import { QueryHistoryPanel } from "./QueryHistoryPanel";
import { useQueryHistory, type QueryHistoryEntry } from "./history";
import type { EditorTabState, ResultSet, ResultsPaneTab } from "./store";

function ResultView({ rs, label }: { rs: ResultSet; label: string }) {
  const store = useResultStore(rs.resultId);
  const columns = useMemo(
    () => rs.columns.map((c) => ({ name: c.name, type: c.type_name ?? undefined })),
    [rs.columns],
  );
  return (
    <>
      <ResultGrid
        columns={columns}
        store={store}
        rowCount={rs.rowCount}
        label={label}
        copyRange={(r0, r1, c0, c1) => resultCopy(rs.resultId, r0, r1, c0, c1)}
      />
      <div className="grid-status" role="status">
        <span>
          {rs.rowCount.toLocaleString()} row{rs.rowCount === 1 ? "" : "s"}
          {!rs.done && " · streaming…"}
        </span>
        {rs.truncated && <span className="warn">result truncated at the row or memory cap — add a LIMIT</span>}
        <span className="spacer" />
        <ExportButton resultId={rs.resultId} disabled={!rs.done && rs.rowCount === 0} />
      </div>
    </>
  );
}

export function ResultsPane({
  state,
  onSelectResult,
  onSelectPane,
  onLoadSql,
  onOpenHistoryTab,
}: {
  state: EditorTabState;
  onSelectResult: (i: number) => void;
  onSelectPane: (pane: ResultsPaneTab) => void;
  onLoadSql: (sql: string) => void;
  onOpenHistoryTab: (entry: QueryHistoryEntry) => void;
}) {
  const historyCount = useQueryHistory((s) => s.entries.length);
  const hasResults = state.results.length > 0;
  const hasPlan = !!state.explain || !!state.explainError;
  const shown: ResultsPaneTab =
    state.activePane === "plan" && hasPlan
      ? "plan"
      : state.activePane === "history"
        ? "history"
        : state.activePane === "results" && hasResults
          ? "results"
          : "messages";

  const errors = state.messages.filter((m) => m.kind === "error").length;
  const rs = state.results[Math.min(state.activeResult, state.results.length - 1)];

  return (
    <div className="results-pane">
      <div className="results-tabs" role="tablist" aria-label="Results">
        <button type="button" role="tab" aria-selected={shown === "results"} disabled={!hasResults} onClick={() => onSelectPane("results")}>
          Results{state.results.length > 1 ? ` (${state.results.length})` : ""}
        </button>
        <button type="button" role="tab" aria-selected={shown === "messages"} onClick={() => onSelectPane("messages")}>
          Messages{errors ? ` · ${errors} error${errors === 1 ? "" : "s"}` : ""}
        </button>
        <button type="button" role="tab" aria-selected={shown === "plan"} disabled={!hasPlan} onClick={() => onSelectPane("plan")}>
          Plan{state.explain?.analyze ? " (Analyze)" : ""}
        </button>
        <button type="button" role="tab" aria-selected={shown === "history"} onClick={() => onSelectPane("history")}>
          History{historyCount > 0 ? ` (${historyCount})` : ""}
        </button>
        {shown === "results" && state.results.length > 1 && (
          <select aria-label="Result set" value={state.activeResult} onChange={(e) => onSelectResult(Number(e.target.value))}>
            {state.results.map((r, i) => (
              <option key={r.resultId} value={i}>
                Result {i + 1} (statement {r.statementIndex + 1}, {r.rowCount.toLocaleString()} rows)
              </option>
            ))}
          </select>
        )}
      </div>
      {shown === "results" && rs ? (
        <ResultView key={rs.resultId} rs={rs} label={`Result ${state.activeResult + 1}`} />
      ) : shown === "plan" ? (
        state.explainError ? (
          <div className="explain-pane" aria-label="Execution plan">
            <ErrorNote error={state.explainError} />
          </div>
        ) : state.explain ? (
          <ExplainPlanView output={state.explain} />
        ) : null
      ) : shown === "history" ? (
        <QueryHistoryPanel
          connectionId={state.connectionId}
          database={state.database}
          onLoadSql={onLoadSql}
          onOpenNewTab={onOpenHistoryTab}
        />
      ) : (
        <ol className="messages" aria-label="Messages">
          {state.messages.length === 0 && <li className="empty">{state.running ? "Running…" : "No messages."}</li>}
          {state.messages.map((m, i) => (
            <li key={i} data-kind={m.kind}>
              <span>{m.text}</span>
              {m.detail && <pre>{m.detail}</pre>}
            </li>
          ))}
        </ol>
      )}
    </div>
  );
}
