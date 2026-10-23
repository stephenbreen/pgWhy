// The results grid: rows windowed from the core, slot-recycled DOM rows, cell-range selection
// with TSV copy, NULL distinct from empty, pending-edit marks, optional in-place editing and
// sortable headers for the top-N view.

import {
  memo,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  useSyncExternalStore,
  type CSSProperties,
} from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { ROW_DELETED, ROW_INSERTED, ROW_UPDATED } from "./protocol";
import { toAppError } from "../lib/ipc/errors";
import type { RowStore } from "./rowstore";

export interface GridColumn {
  name: string;
  /** Postgres type name when known (format_type output) */
  type?: string;
  /** part of the row key used for edits */
  key?: boolean;
}

export interface SortSpec {
  column: number;
  dir: "asc" | "desc";
}

export const ROW_H = 22;
const OVERSCAN = 8;
// must exceed visible rows + 2×overscan so two live rows never share a slot
const SLOTS = 160;
const GUTTER_W = 56;

const NUMERIC = /^(smallint|integer|bigint|numeric|real|double precision|money|int[248]?|float[48]|decimal)/;

function defaultWidth(c: GridColumn): number {
  if (c.type && NUMERIC.test(c.type)) return 100;
  if (c.type === "boolean") return 70;
  if (c.type?.startsWith("timestamp")) return 190;
  if (c.type === "date") return 100;
  if (c.type === "uuid") return 290;
  return Math.min(320, Math.max(110, c.name.length * 8 + 30));
}

interface Selection {
  anchor: { row: number; col: number };
  focus: { row: number; col: number };
}

function norm(sel: Selection) {
  return {
    r0: Math.min(sel.anchor.row, sel.focus.row),
    r1: Math.max(sel.anchor.row, sel.focus.row),
    c0: Math.min(sel.anchor.col, sel.focus.col),
    c1: Math.max(sel.anchor.col, sel.focus.col),
  };
}

/** TSV with Excel/SSMS conventions: tabs/newlines inside values are quoted. */
export function toTsv(rows: (string | null | undefined)[][], header?: string[]): string {
  const esc = (v: string | null | undefined) => {
    if (v === null || v === undefined) return "";
    return /[\t\n\r"]/.test(v) ? `"${v.replace(/"/g, '""')}"` : v;
  };
  const lines = rows.map((r) => r.map(esc).join("\t"));
  if (header) lines.unshift(header.map(esc).join("\t"));
  return lines.join("\n");
}

const Row = memo(function Row({
  index,
  top,
  cols,
  widths,
  store,
  sel,
  editing,
  numeric,
  onCellDown,
  onCellDouble,
  onEditDone,
}: {
  index: number;
  top: number;
  cols: number;
  widths: number[];
  store: RowStore;
  sel: { r0: number; r1: number; c0: number; c1: number } | null;
  editing: { row: number; col: number } | null;
  numeric: boolean[];
  onCellDown: (row: number, col: number, shift: boolean) => void;
  onCellDouble: (row: number, col: number) => void;
  onEditDone: (value: string | null, commit: boolean) => void;
}) {
  // Subscribe to this row's window only: a window arriving re-renders the rows it holds, not the
  // whole viewport (the parent re-renders on every change, but memo bails out for other rows).
  useSyncExternalStore(store.subscribe, () => store.versionOf(index));
  const flag = store.rowFlag(index);
  const rowState = flag === ROW_UPDATED ? "updated" : flag === ROW_INSERTED ? "inserted" : flag === ROW_DELETED ? "deleted" : undefined;
  const loaded = store.cell(index, 0) !== undefined || cols === 0;
  return (
    <div className="grid-row" role="row" aria-rowindex={index + 2} data-row-state={rowState} style={{ transform: `translateY(${top}px)` }}>
      <span className="grid-gutter" role="rowheader">{index + 1}</span>
      {Array.from({ length: cols }, (_, c) => {
        const v = store.cell(index, c);
        const selected = sel && index >= sel.r0 && index <= sel.r1 && c >= sel.c0 && c <= sel.c1;
        const isEditing = editing && editing.row === index && editing.col === c;
        const style: CSSProperties = { width: widths[c] };
        return (
          <span
            key={c}
            role="gridcell"
            className="grid-cell"
            style={style}
            data-null={v === null || undefined}
            data-numeric={numeric[c] || undefined}
            data-pending={store.pending(index, c) || undefined}
            aria-selected={selected || undefined}
            onMouseDown={(e) => {
              if ((e.target as HTMLElement).closest(".cell-editor")) return;
              onCellDown(index, c, e.shiftKey);
            }}
            onClick={(e) => {
              // also select on click: some input paths (assistive tech, WebDriver) send no mousedown
              if (!(e.target as HTMLElement).closest(".cell-editor")) onCellDown(index, c, e.shiftKey);
            }}
            onDoubleClick={() => onCellDouble(index, c)}
          >
            {isEditing ? (
              <CellEditor initial={v ?? null} onDone={onEditDone} />
            ) : !loaded ? (
              <span className="grid-loading">…</span>
            ) : v === null ? (
              "NULL"
            ) : (
              v
            )}
          </span>
        );
      })}
    </div>
  );
});

function CellEditor({ initial, onDone }: { initial: string | null; onDone: (v: string | null, commit: boolean) => void }) {
  const [value, setValue] = useState(initial ?? "");
  const [isNull, setNull] = useState(initial === null);
  return (
    <span className="cell-editor">
      <input
        autoFocus
        aria-label="Cell value"
        value={isNull ? "" : value}
        placeholder={isNull ? "NULL" : ""}
        onChange={(e) => {
          setNull(false);
          setValue(e.target.value);
        }}
        onKeyDown={(e) => {
          e.stopPropagation();
          if (e.key === "Enter") onDone(isNull ? null : value, true);
          else if (e.key === "Escape") onDone(null, false);
          // Ctrl/Cmd+0 sets NULL, like SSMS
          else if ((e.ctrlKey || e.metaKey) && e.key === "0") {
            e.preventDefault();
            setNull(true);
          }
        }}
        onBlur={() => onDone(isNull ? null : value, true)}
      />
    </span>
  );
}

export interface ResultGridProps {
  columns: GridColumn[];
  store: RowStore;
  rowCount: number;
  label?: string;
  sort?: SortSpec | null;
  onSort?: (column: number) => void;
  /** enables double-click editing; called with null for SQL NULL */
  onEditCell?: (row: number, col: number, value: string | null) => void;
  /** core-side copy for ranges that aren't fully resident */
  copyRange?: (r0: number, r1: number, c0: number, c1: number) => Promise<string>;
  onSelectionChange?: (rows: [number, number] | null) => void;
}

export function ResultGrid({ columns, store, rowCount, label, sort, onSort, onEditCell, copyRange, onSelectionChange }: ResultGridProps) {
  const scrollRef = useRef<HTMLDivElement>(null);

  const [widths, setWidths] = useState(() => columns.map(defaultWidth));
  const [selection, setSelection] = useState<Selection | null>(null);
  const [editing, setEditing] = useState<{ row: number; col: number } | null>(null);
  const [copyError, setCopyError] = useState<string | null>(null);
  const dragging = useRef(false);

  useEffect(() => setWidths(columns.map(defaultWidth)), [columns]);
  // Any store change can make the visible window re-fetchable (invalidate, a new partial chunk);
  // the rows themselves subscribe for their content.
  const revision = useSyncExternalStore(store.subscribe, () => store.revision);
  useEffect(() => store.setRowCount(rowCount), [store, rowCount]);

  const numeric = useMemo(() => columns.map((c) => !!c.type && NUMERIC.test(c.type)), [columns]);
  const totalWidth = GUTTER_W + widths.reduce((a, b) => a + b, 0);

  const virtualizer = useVirtualizer({
    count: rowCount,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => ROW_H,
    overscan: OVERSCAN,
    // first paint before the scroll element is measured (and jsdom, which never measures)
    initialRect: { width: 1000, height: 600 },
  });
  const items = virtualizer.getVirtualItems();

  // keep the visible window (+ prefetch) resident
  const first = items[0]?.index ?? 0;
  const last = items[items.length - 1]?.index ?? 0;
  // re-ensured after every store change too, so an invalidated window refetches in place
  useEffect(() => {
    store.ensureRange(first - 128, last + 512);
  }, [store, first, last, rowCount, revision]);

  const sel = selection ? norm(selection) : null;
  useEffect(() => onSelectionChange?.(sel ? [sel.r0, sel.r1] : null), [sel?.r0, sel?.r1]); // eslint-disable-line react-hooks/exhaustive-deps

  const onCellDown = useCallback((row: number, col: number, shift: boolean) => {
    dragging.current = true;
    // keyboard (copy, arrows, F2/Enter) follows the selection even for synthetic clicks
    scrollRef.current?.focus({ preventScroll: true });
    setSelection((s) => (shift && s ? { anchor: s.anchor, focus: { row, col } } : { anchor: { row, col }, focus: { row, col } }));
  }, []);
  const onCellDouble = useCallback(
    (row: number, col: number) => {
      if (onEditCell && store.rowFlag(row) !== ROW_DELETED) setEditing({ row, col });
    },
    [onEditCell, store],
  );
  const onEditDone = useCallback(
    (value: string | null, commit: boolean) => {
      setEditing((ed) => {
        if (ed && commit && onEditCell && value !== store.cell(ed.row, ed.col)) onEditCell(ed.row, ed.col, value);
        return null;
      });
      scrollRef.current?.focus();
    },
    [onEditCell, store],
  );

  useEffect(() => {
    const up = () => (dragging.current = false);
    window.addEventListener("mouseup", up);
    return () => window.removeEventListener("mouseup", up);
  }, []);

  const copy = async (withHeaders: boolean) => {
    if (!sel) return;
    const { r0, r1, c0, c1 } = sel;
    setCopyError(null);
    let text: string;
    let resident = true;
    for (let r = r0; r <= r1 && resident; r++) if (store.cell(r, c0) === undefined) resident = false;
    try {
      if (!resident) {
        if (!copyRange) {
          // never copy a partially-loaded selection as if it were complete
          setCopyError("only the rows loaded so far are available — wait for the result to finish, or export it");
          return;
        }
        text = await copyRange(r0, r1, c0, c1);
      } else {
        const rows: (string | null | undefined)[][] = [];
        for (let r = r0; r <= r1; r++) {
          const row: (string | null | undefined)[] = [];
          for (let c = c0; c <= c1; c++) row.push(store.cell(r, c));
          rows.push(row);
        }
        text = toTsv(rows);
      }
    } catch (e) {
      setCopyError(toAppError(e).message);
      return;
    }
    if (withHeaders) text = toTsv([], columns.slice(c0, c1 + 1).map((c) => c.name)) + "\n" + text;
    await navigator.clipboard?.writeText(text);
  };

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (editing) return;
    const mod = e.metaKey || e.ctrlKey;
    if (mod && e.key.toLowerCase() === "c") {
      e.preventDefault();
      void copy(e.shiftKey);
      return;
    }
    if (mod && e.key.toLowerCase() === "a") {
      e.preventDefault();
      setSelection({ anchor: { row: 0, col: 0 }, focus: { row: Math.max(0, rowCount - 1), col: columns.length - 1 } });
      return;
    }
    if (!selection) return;
    const move: Record<string, [number, number]> = { ArrowUp: [-1, 0], ArrowDown: [1, 0], ArrowLeft: [0, -1], ArrowRight: [0, 1] };
    const m = move[e.key];
    if (m) {
      e.preventDefault();
      const f = selection.focus;
      const next = {
        row: Math.min(Math.max(0, f.row + m[0]), Math.max(0, rowCount - 1)),
        col: Math.min(Math.max(0, f.col + m[1]), columns.length - 1),
      };
      setSelection(e.shiftKey ? { anchor: selection.anchor, focus: next } : { anchor: next, focus: next });
      virtualizer.scrollToIndex(next.row);
    } else if ((e.key === "F2" || e.key === "Enter") && onEditCell) {
      e.preventDefault();
      onCellDouble(selection.focus.row, selection.focus.col);
    }
  };

  const startResize = (col: number, e: React.MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
    const x0 = e.clientX;
    const w0 = widths[col];
    const onMove = (ev: MouseEvent) => setWidths((w) => w.map((x, i) => (i === col ? Math.max(40, w0 + ev.clientX - x0) : x)));
    const onUp = () => {
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
    };
    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
  };

  return (
    <div className="result-grid" role="grid" aria-label={label ?? "Results"} aria-rowcount={rowCount + 1} aria-colcount={columns.length}>
      {copyError && (
        <div className="grid-alert" role="alert">
          <span>{copyError}</span>
          <button type="button" className="link" onClick={() => setCopyError(null)}>
            Dismiss
          </button>
        </div>
      )}
      <div ref={scrollRef} className="grid-scroll" tabIndex={0} onKeyDown={onKeyDown}>
        <div className="grid-header" role="row" aria-rowindex={1} style={{ width: totalWidth }}>
          <span className="grid-gutter" />
          {columns.map((c, i) => (
            <span
              key={i}
              role="columnheader"
              className="grid-col-header"
              style={{ width: widths[i] }}
              title={c.type ? `${c.name} — ${c.type}` : c.name}
              aria-sort={sort?.column === i ? (sort.dir === "asc" ? "ascending" : "descending") : undefined}
              onClick={() => onSort?.(i)}
              data-sortable={!!onSort || undefined}
            >
              <span className="col-name">
                {c.key && <span className="key-mark" title="row key">KEY</span>}
                {c.name}
              </span>
              {c.type && <span className="col-type">{c.type}</span>}
              {sort?.column === i && <span className="sort-mark">{sort.dir === "asc" ? "▲" : "▼"}</span>}
              <span className="col-resizer" onMouseDown={(e) => startResize(i, e)} />
            </span>
          ))}
        </div>
        <div className="grid-body" style={{ height: virtualizer.getTotalSize(), width: totalWidth }}>
          {items.map((vi) => (
            <Row
              key={vi.index % SLOTS}
              index={vi.index}
              top={vi.start}
              cols={columns.length}
              widths={widths}
              store={store}
              sel={sel && vi.index >= sel.r0 && vi.index <= sel.r1 ? sel : null}
              editing={editing && editing.row === vi.index ? editing : null}
              numeric={numeric}
              onCellDown={onCellDown}
              onCellDouble={onCellDouble}
              onEditDone={onEditDone}
            />
          ))}
        </div>
      </div>
    </div>
  );
}
