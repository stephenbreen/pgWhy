// SSMS-style results pane: one grid at a time with a result-set switcher, plus Messages.
// A 50-statement script yields one pane, not 50 tabs.

import { useState } from "react";
import { ResultGrid } from "../grid/ResultGrid";
import { useResultStore } from "../grid/useResultStore";
import { resultCopy } from "../lib/ipc/query";
import { ExportButton } from "../grid/ExportButton";
import type { EditorTabState, ResultSet } from "./store";

function ResultView({ rs, label }: { rs: ResultSet; label: string }) {
  const store = useResultStore(rs.resultId);
  return (
    <>
      <ResultGrid
        columns={rs.columns.map((c) => ({ name: c.name, type: c.type_name ?? undefined }))}
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

export function ResultsPane({ state, onSelectResult }: { state: EditorTabState; onSelectResult: (i: number) => void }) {
  const [view, setView] = useState<"results" | "messages">("results");
  const hasResults = state.results.length > 0;
  const shown = hasResults && view === "results" ? "results" : "messages";
  const errors = state.messages.filter((m) => m.kind === "error").length;
  const rs = state.results[Math.min(state.activeResult, state.results.length - 1)];

  return (
    <div className="results-pane">
      <div className="results-tabs" role="tablist" aria-label="Results">
        <button type="button" role="tab" aria-selected={shown === "results"} disabled={!hasResults} onClick={() => setView("results")}>
          Results{state.results.length > 1 ? ` (${state.results.length})` : ""}
        </button>
        <button type="button" role="tab" aria-selected={shown === "messages"} onClick={() => setView("messages")}>
          Messages{errors ? ` · ${errors} error${errors === 1 ? "" : "s"}` : ""}
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
