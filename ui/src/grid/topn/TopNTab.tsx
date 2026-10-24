// "View top N rows": one action from the tree. Sorting and filtering
// re-query the server (the whole table, not the loaded rows). Edits stay pending — marked in
// the grid, previewable as SQL — until committed as one transaction, or discarded.

import { useCallback, useEffect, useMemo, useState } from "react";
import { setCloseGuard, useWorkspace, type Tab } from "../../app/workspace";
import { useConnections } from "../../connection/store";
import { TagPill } from "../../connection/EnvTag";
import { ErrorNote } from "../../connection/ErrorNote";
import * as g from "../../lib/ipc/grid";
import { resultCopy } from "../../lib/ipc/query";
import { toAppError, type AppError } from "../../lib/ipc/errors";
import { ResultGrid } from "../ResultGrid";
import { useResultStore } from "../useResultStore";
import { ExportButton } from "../ExportButton";

export interface TopNData {
  connectionId: string;
  database: string;
  schema: string;
  name: string;
}

export function openTopN(t: TopNData) {
  const entry = useConnections.getState().entries.find((e) => e.id === t.connectionId);
  useWorkspace.getState().openTab({
    id: `topn:${crypto.randomUUID()}`,
    kind: "grid",
    title: t.name,
    connectionId: t.connectionId,
    tag: entry?.env_tag,
    data: t,
  });
}

const OPS: g.CriteriaOp[] = ["=", "<>", "<", "<=", ">", ">=", "LIKE", "ILIKE", "NOT LIKE", "IS NULL", "IS NOT NULL"];
const NO_VALUE = new Set<g.CriteriaOp>(["IS NULL", "IS NOT NULL"]);

type Confirm = { text: string; resolve: (ok: boolean) => void };

function Grid({ view, gridId, onEdit, onSelectRows, sort, onSort, refresh }: {
  view: g.GridView;
  gridId: string;
  /** bump to re-fetch the visible window in place (pending edits changed) */
  refresh: number;
  onEdit?: (row: number, col: number, v: string | null) => void;
  onSelectRows: (r: [number, number] | null) => void;
  sort: g.SortKey[];
  onSort: (col: number) => void;
}) {
  const store = useResultStore(view.result_id);
  void gridId;
  useEffect(() => {
    if (refresh) store.invalidate();
  }, [refresh, store]);
  const s = sort[0];
  const col = s ? view.columns.findIndex((c) => c.name === s.column) : -1;
  return (
    <ResultGrid
      columns={view.columns.map((c) => ({ name: c.name, type: c.type, key: c.key }))}
      store={store}
      rowCount={view.row_count}
      label={view.sql}
      sort={col >= 0 ? { column: col, dir: s.dir } : null}
      onSort={onSort}
      onEditCell={onEdit}
      onSelectionChange={onSelectRows}
      copyRange={(r0, r1, c0, c1) => resultCopy(view.result_id, r0, r1, c0, c1)}
    />
  );
}

export function TopNTab({ tab }: { tab: Tab }) {
  const data = tab.data as TopNData;
  const entry = useConnections((s) => s.entries.find((e) => e.id === data.connectionId));
  const gridId = tab.id;
  const [view, setView] = useState<g.GridView | null>(null);
  const [error, setError] = useState<AppError | null>(null);
  const [criteria, setCriteria] = useState<g.Criterion[]>([]);
  const [showFilter, setShowFilter] = useState(false);
  const [sort, setSort] = useState<g.SortKey[]>([]);
  const [pending, setPending] = useState(0);
  const [preview, setPreview] = useState<string[] | null>(null);
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [selRows, setSelRows] = useState<[number, number] | null>(null);
  const [confirm, setConfirm] = useState<Confirm | null>(null);
  const [version, setVersion] = useState(0); // remount the grid's store after edits

  const ask = (text: string) => new Promise<boolean>((resolve) => setConfirm({ text, resolve }));

  const open = useCallback(async () => {
    setError(null);
    try {
      const v = await g.gridOpen({ gridId, ...data, criteria: [], sort: [] });
      setView(v);
      setPending(0);
    } catch (e) {
      setError(toAppError(e));
    }
  }, [gridId, data]);

  useEffect(() => {
    void open();
    return () => void g.gridClose(gridId).catch(() => {});
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    setCloseGuard(tab.id, async () => (pending > 0 ? ask(`Discard ${pending} pending change${pending === 1 ? "" : "s"} to ${data.name}?`) : true));
    return () => setCloseGuard(tab.id, null);
  }, [tab.id, pending, data.name]);

  const requery = async (nextCriteria = criteria, nextSort = sort) => {
    if (pending > 0 && !(await ask(`Re-running the query discards ${pending} pending change${pending === 1 ? "" : "s"}.`))) return;
    setBusy(true);
    setError(null);
    try {
      if (pending > 0) await g.gridDiscard(gridId);
      const v = await g.gridRequery(gridId, nextCriteria.filter((c) => c.column), nextSort);
      setView(v);
      setPending(0);
      setPreview(null);
      setVersion((x) => x + 1);
    } catch (e) {
      setError(toAppError(e));
    } finally {
      setBusy(false);
    }
  };

  const onSort = (colIdx: number) => {
    if (!view) return;
    const name = view.columns[colIdx].name;
    const cur = sort[0]?.column === name ? sort[0].dir : null;
    const next: g.SortKey[] = cur === "asc" ? [{ column: name, dir: "desc" }] : cur === "desc" ? [] : [{ column: name, dir: "asc" }];
    setSort(next);
    void requery(criteria, next);
  };

  const applyPending = (p: g.PendingInfo) => {
    setPending(p.pending);
    setView((v) => (v ? { ...v, row_count: p.row_count } : v));
    setVersion((x) => x + 1);
    if (preview) void g.gridPreview(gridId).then(setPreview);
  };

  const onEdit = view?.editable
    ? (row: number, col: number, value: string | null) => {
        if (!view.columns[col].writable) {
          setNotice(`${view.columns[col].name} can't be written (generated or identity column)`);
          return;
        }
        void g.gridEditCell(gridId, row, col, value).then(applyPending).catch((e) => setError(toAppError(e)));
      }
    : undefined;

  const commit = async () => {
    setBusy(true);
    setError(null);
    try {
      const r = await g.gridCommit(gridId);
      setNotice(`Committed: ${r.updated} updated, ${r.inserted} inserted, ${r.deleted} deleted`);
      setPreview(null);
      const v = await g.gridRequery(gridId, criteria.filter((c) => c.column), sort);
      setView(v);
      setPending(0);
      setVersion((x) => x + 1);
    } catch (e) {
      // pending edits are preserved on any failure
      setError(toAppError(e));
    } finally {
      setBusy(false);
    }
  };

  const deleteSelected = () => {
    if (!view?.editable || !selRows) return;
    const rows = [];
    for (let r = selRows[0]; r <= selRows[1]; r++) rows.push(r);
    void g.gridDeleteRows(gridId, rows).then(applyPending).catch((e) => setError(toAppError(e)));
  };

  const filterRow = useMemo(
    () =>
      showFilter && view ? (
        <div className="criteria" role="group" aria-label="Filter">
          {criteria.map((c, i) => (
            <div key={i} className="criterion">
              <select aria-label="Filter column" value={c.column} onChange={(e) => setCriteria(criteria.map((x, j) => (j === i ? { ...x, column: e.target.value } : x)))}>
                {view.columns.map((col) => (
                  <option key={col.name} value={col.name}>{col.name}</option>
                ))}
              </select>
              <select aria-label="Filter operator" value={c.op} onChange={(e) => setCriteria(criteria.map((x, j) => (j === i ? { ...x, op: e.target.value as g.CriteriaOp } : x)))}>
                {OPS.map((o) => (
                  <option key={o} value={o}>{o}</option>
                ))}
              </select>
              {!NO_VALUE.has(c.op) && (
                <input aria-label="Filter value" value={c.value ?? ""} onChange={(e) => setCriteria(criteria.map((x, j) => (j === i ? { ...x, value: e.target.value } : x)))} onKeyDown={(e) => e.key === "Enter" && void requery()} />
              )}
              <button type="button" className="icon" aria-label="Remove filter" onClick={() => setCriteria(criteria.filter((_, j) => j !== i))}>✕</button>
            </div>
          ))}
          <button type="button" onClick={() => setCriteria([...criteria, { column: view.columns[0]?.name ?? "", op: "=", value: "" }])}>+ condition</button>
          <button type="button" className="primary" onClick={() => void requery()} disabled={busy}>Apply</button>
        </div>
      ) : null,
    [showFilter, view, criteria, busy], // eslint-disable-line react-hooks/exhaustive-deps
  );

  return (
    <div className="topn-tab" onKeyDown={(e) => e.key === "Delete" && (e.target as HTMLElement).closest(".grid-scroll") && deleteSelected()}>
      <div className="editor-toolbar" role="toolbar" aria-label="Rows">
        <span className="topn-title">
          {data.schema}.{data.name}
        </span>
        <span className="muted">top {view?.n ?? "…"}</span>
        <button type="button" onClick={() => void requery()} disabled={busy}>↻ Refresh</button>
        <button type="button" aria-pressed={showFilter} onClick={() => setShowFilter(!showFilter)}>Filter{criteria.length ? ` (${criteria.length})` : ""}</button>
        {view?.editable ? (
          <>
            <button type="button" onClick={() => void g.gridInsertRow(gridId).then(applyPending).catch((e) => setError(toAppError(e)))}>+ Row</button>
            <button type="button" disabled={!selRows} onClick={deleteSelected}>Delete rows</button>
          </>
        ) : (
          view && <span className="readonly-chip" title={view.read_only_reason ?? undefined}>read-only{view.read_only_reason ? `: ${view.read_only_reason}` : ""}</span>
        )}
        <span className="spacer" />
        <span className="binding">
          {entry && <TagPill tag={entry.env_tag} />}
          <span className="binding-name">{entry?.name}</span>
          <span className="binding-db">/{data.database}</span>
        </span>
      </div>
      {filterRow}
      {confirm && (
        <div className="txn-ask" role="alert">
          <span>{confirm.text}</span>
          <button type="button" onClick={() => { confirm.resolve(true); setConfirm(null); }}>Discard</button>
          <button type="button" onClick={() => { confirm.resolve(false); setConfirm(null); }}>Keep editing</button>
        </div>
      )}
      {pending > 0 && (
        <div className="pending-bar" role="status" aria-label="Pending changes">
          <span>
            {pending} pending change{pending === 1 ? "" : "s"}
          </span>
          <button type="button" onClick={() => void (preview ? setPreview(null) : g.gridPreview(gridId).then(setPreview))}>
            {preview ? "Hide SQL" : "Preview SQL"}
          </button>
          <span className="spacer" />
          <button type="button" onClick={() => void g.gridDiscard(gridId).then((p) => { applyPending(p); setPreview(null); })} disabled={busy}>Discard</button>
          <button type="button" className="primary" onClick={() => void commit()} disabled={busy}>Commit</button>
        </div>
      )}
      {preview && (
        <pre className="sql-preview" aria-label="Generated SQL">{preview.join("\n")}</pre>
      )}
      {error && <div className="editor-banner"><ErrorNote error={error} /></div>}
      {notice && <div className="editor-banner" role="status"><span>{notice}</span><button type="button" className="icon" aria-label="Dismiss" onClick={() => setNotice(null)}>✕</button></div>}
      {view ? (
        <>
          <Grid key={view.result_id} refresh={version} view={view} gridId={gridId} onEdit={onEdit} onSelectRows={setSelRows} sort={sort} onSort={onSort} />
          <div className="grid-status" role="status">
            <span>{view.row_count.toLocaleString()} rows</span>
            <span className="mono muted sql-line" title={view.sql}>{view.sql}</span>
            <span className="spacer" />
            <ExportButton resultId={view.result_id} />
          </div>
        </>
      ) : (
        !error && <p className="empty">Loading…</p>
      )}
    </div>
  );
}
