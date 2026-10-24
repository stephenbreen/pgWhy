// The SSMS-density object tree: connections at the root, then databases →
// schemas → typed folders → relations with their columns and types inline. One virtualized
// list so thousands of tables stay instant; type-to-filter over loaded nodes; Ctrl/Cmd+P
// jumps to any object in the connected database; node errors render inline with a retry.

import { useEffect, useMemo, useRef, useState } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { useConnections, groupEntries } from "../../connection/store";
import { ConnectionNode, openConnectionForm } from "../../connection/ConnectionNode";
import { ErrorNote } from "../../connection/ErrorNote";
import type { ConnectionEntry } from "../../lib/ipc/connections";
import { actionsFor, notifySelect, type NodeTarget } from "../actions";
import { flatten, useTrees, type TreeRow } from "./model";
import { ContextMenu } from "./ContextMenu";
import { GoToObject } from "./GoToObject";
import { FILTER_OBJECTS_EVENT, FIND_OBJECT_EVENT, shortcut } from "../../app/keys";

type Row =
  | { kind: "group"; id: string; label: string }
  | { kind: "connection"; id: string; entry: ConnectionEntry }
  | (TreeRow & { connectionId: string });

const KIND_GLYPH: Record<string, string> = {
  table: "▦", partitioned_table: "▤", view: "◫", matview: "◩", foreign_table: "⊡", sequence: "#",
  function: "ƒ", procedure: "ƒ", aggregate: "Σ", window: "ƒ",
};

function fmtCount(n: number | null | undefined): string | null {
  if (n == null || n < 0) return null;
  if (n >= 1e6) return `~${(n / 1e6).toFixed(1)}M rows`;
  if (n >= 1e3) return `~${Math.round(n / 1e3)}k rows`;
  return `~${n} rows`;
}

function targetOf(row: Row): NodeTarget | null {
  if (!("connectionId" in row)) return null;
  switch (row.kind) {
    case "database":
      return { type: "database", connectionId: row.connectionId, db: row.db };
    case "schema":
      return { type: "schema", connectionId: row.connectionId, db: row.db, schema: row.schema.name };
    case "relation":
      return { type: "relation", connectionId: row.connectionId, db: row.db, rel: row.rel };
    case "routine":
      return { type: "routine", connectionId: row.connectionId, db: row.db, routine: row.routine };
    default:
      return null;
  }
}

export function ExplorerTree() {
  const entries = useConnections((s) => s.entries);
  const recents = useConnections((s) => s.recents);
  const status = useConnections((s) => s.status);
  const loaded = useConnections((s) => s.loaded);
  const trees = useTrees((s) => s.trees);
  const openDbs = useConnections((s) => s.openDbs);
  const selected = useTrees((s) => s.selected);
  const { toggle, select, restore } = useTrees.getState();
  const [openConns, setOpenConns] = useState<Record<string, boolean>>({});
  const [filter, setFilter] = useState("");
  const [menu, setMenu] = useState<{ target: NodeTarget; x: number; y: number } | null>(null);
  const [goTo, setGoTo] = useState(false);
  const scrollRef = useRef<HTMLDivElement>(null);

  // a newly connected entry restores its tree (first connect: default db/public/Tables)
  useEffect(() => {
    for (const e of entries) {
      if (status[e.id]?.state === "connected" && !trees[e.id]?.restored) {
        void restore(e.id, e.database);
        setOpenConns((o) => (e.id in o ? o : { ...o, [e.id]: true }));
      }
    }
  }, [entries, status, trees, restore]);

  const f = filter.trim().toLowerCase();
  const rows: Row[] = useMemo(() => {
    const out: Row[] = [];
    for (const { group, entries: es } of groupEntries(entries)) {
      if (group) out.push({ kind: "group", id: `group:${group}`, label: group });
      for (const e of es) {
        out.push({ kind: "connection", id: `conn:${e.id}`, entry: e });
        const connected = status[e.id]?.state === "connected";
        const t = trees[e.id];
        if (connected && t && (openConns[e.id] || f)) {
          for (const r of flatten(e.id, t, 0, f)) out.push({ ...r, connectionId: e.id });
        }
      }
    }
    return out;
  }, [entries, status, trees, openConns, f]);

  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: (i) => (rows[i]?.kind === "group" ? 24 : 22),
    overscan: 20,
    getItemKey: (i) => rows[i].id,
    initialRect: { width: 300, height: 800 },
  });

  // ⌘P / Ctrl+P → find object; ⇧⌘P / Ctrl+Shift+P → filter (bound in app/shortcuts.ts)
  const filterRef = useRef<HTMLInputElement>(null);
  useEffect(() => {
    const find = () => setGoTo(true);
    const focusFilter = () => {
      filterRef.current?.focus();
      filterRef.current?.select();
    };
    window.addEventListener(FIND_OBJECT_EVENT, find);
    window.addEventListener(FILTER_OBJECTS_EVENT, focusFilter);
    return () => {
      window.removeEventListener(FIND_OBJECT_EVENT, find);
      window.removeEventListener(FILTER_OBJECTS_EVENT, focusFilter);
    };
  }, []);

  if (!loaded) return <p className="empty">Loading connections…</p>;
  if (entries.length === 0) {
    return (
      <div className="empty">
        <p>No saved connections.</p>
        <button type="button" className="primary" onClick={() => openConnectionForm()}>
          New connection
        </button>
      </div>
    );
  }

  const activate = (row: Row) => {
    if (!("connectionId" in row)) return;
    select(row.connectionId, row.id);
    const t = targetOf(row);
    if (t) notifySelect(t);
  };
  const runPrimary = (row: Row) => {
    const t = targetOf(row);
    const primary = t && actionsFor(t).find((a) => a.primary && !a.disabledReason?.(t));
    if (primary && t) void primary.run(t);
  };
  const recentEntries = recents.map((id) => entries.find((e) => e.id === id)).filter((e): e is ConnectionEntry => !!e).slice(0, 5);

  const renderRow = (row: Row) => {
    if (row.kind === "group") return <header className="group-header">{row.label}</header>;
    if (row.kind === "connection") {
      return (
        <ConnectionNode
          entry={row.entry}
          expanded={!!openConns[row.entry.id]}
          onToggle={(open) => setOpenConns((o) => ({ ...o, [row.entry.id]: open ?? !o[row.entry.id] }))}
        />
      );
    }
    const pad = { paddingLeft: 8 + row.depth * 14 };
    const isSel = selected?.connectionId === row.connectionId && selected.id === row.id;
    // `data-connection` lets the e2e specs (and anyone reading the DOM) tell two connections'
    // same-named databases apart; tree rows are a flat list, so there is no ancestor to scope by.
    const connectionName =
      "connectionId" in row ? entries.find((e) => e.id === row.connectionId)?.name : undefined;
    const common = {
      role: "treeitem" as const,
      "aria-level": row.depth + 1,
      "aria-selected": isSel,
      tabIndex: isSel ? 0 : -1,
      style: pad,
      "data-connection": connectionName,
    };
    const onContext = (e: React.MouseEvent) => {
      const t = targetOf(row);
      if (!t) return;
      e.preventDefault();
      activate(row);
      setMenu({ target: t, x: e.clientX, y: e.clientY });
    };
    switch (row.kind) {
      case "status":
        return row.state === "error" ? (
          <div className="tree-row node-error" style={pad}>
            <ErrorNote error={row.error!} compact />
            <button type="button" className="icon" aria-label="Retry" onClick={row.retry}>↻</button>
          </div>
        ) : (
          <div className="tree-row status-row" style={pad}>{row.state === "loading" ? "loading…" : "empty"}</div>
        );
      case "database": {
        const live = openDbs[row.connectionId]?.[row.db.name] ?? 0;
        return (
          <div
            {...common}
            className="tree-row db-row"
            data-open={live > 0 || undefined}
            aria-expanded={row.expanded}
            aria-label={row.db.name}
            aria-description={live > 0 ? "connected" : "not connected"}
            title={live > 0 ? `Connected (${live} connection${live === 1 ? "" : "s"})` : "Not connected — expand to open"}
            onClick={() => { activate(row); toggle(row.connectionId, row.id); }}
            onContextMenu={onContext}
          >
            <span className="twisty">{row.expanded ? "▾" : "▸"}</span>
            <span className="node-glyph db-glyph">⛁</span>
            <span className="node-label">{row.db.name}</span>
            {live > 0 && <span className="db-open-dot" aria-hidden="true" />}
            {!row.db.allow_connections && <span className="node-meta">no connections</span>}
          </div>
        );
      }
      case "schema":
        return (
          <div {...common} className="tree-row" aria-expanded={row.expanded} aria-label={row.schema.name} onClick={() => { activate(row); toggle(row.connectionId, row.id); }} onContextMenu={onContext}>
            <span className="twisty">{row.expanded ? "▾" : "▸"}</span>
            <span className="node-glyph">◇</span>
            <span className="node-label">{row.schema.name}</span>
            {row.schema.system && <span className="node-meta">system</span>}
          </div>
        );
      case "folder":
        return (
          <div {...common} className="tree-row folder-row" aria-expanded={row.expanded} aria-label={row.label} onClick={() => toggle(row.connectionId, row.id)}>
            <span className="twisty">{row.expanded ? "▾" : "▸"}</span>
            <span className="node-label">{row.label}</span>
            <span className="node-meta">{row.count}</span>
          </div>
        );
      case "relation":
        return (
          <div
            {...common}
            className="tree-row"
            aria-expanded={row.rel.kind === "sequence" ? undefined : row.expanded}
            aria-label={`${row.rel.schema}.${row.rel.name}`}
            onClick={(e) => {
              activate(row);
              // the twisty expands; the label selects (details panel) — one click either way
              if ((e.target as HTMLElement).closest(".twisty") || row.rel.kind === "sequence") {
                if (row.rel.kind !== "sequence") toggle(row.connectionId, row.id);
              }
            }}
            onDoubleClick={() => runPrimary(row)}
            onKeyDown={(e) => e.key === "Enter" && runPrimary(row)}
            onContextMenu={onContext}
          >
            <span className="twisty" aria-label={row.expanded ? "Collapse" : "Expand"}>{row.rel.kind === "sequence" ? "" : row.expanded ? "▾" : "▸"}</span>
            <span className="node-glyph" title={row.rel.kind.replace("_", " ")}>{KIND_GLYPH[row.rel.kind]}</span>
            <span className="node-label">{row.rel.name}</span>
            <span className="node-meta">{fmtCount(row.rel.row_estimate)}</span>
          </div>
        );
      case "column":
        return (
          <div {...common} className="tree-row column-row" aria-label={`${row.column.name} ${row.column.type}`} onClick={() => activate(row)}>
            <span className="twisty" />
            <span className="node-glyph col-glyph" title={row.column.in_primary_key ? "primary key" : undefined}>{row.column.in_primary_key ? "⚿" : "·"}</span>
            <span className="node-label">{row.column.name}</span>
            <span className="col-type">
              {row.column.type}
              {row.column.not_null ? "" : ", null"}
            </span>
          </div>
        );
      case "routine":
        return (
          <div {...common} className="tree-row" aria-label={`${row.routine.name}(${row.routine.identity_args})`} onClick={() => activate(row)} onContextMenu={onContext}>
            <span className="twisty" />
            <span className="node-glyph">{KIND_GLYPH[row.routine.kind]}</span>
            <span className="node-label">{row.routine.name}</span>
            <span className="node-meta">({row.routine.identity_args})</span>
          </div>
        );
    }
  };

  const items = virtualizer.getVirtualItems();

  return (
    <div className="explorer">
      <div className="explorer-toolbar">
        <input
          ref={filterRef}
          type="search"
          aria-label="Filter objects"
          placeholder={`Filter (${shortcut("P", { shift: true })}) · find any object (${shortcut("P")})`}
          title={`Filter the loaded tree (${shortcut("P", { shift: true })}). To search the whole database, use Find object (${shortcut("P")}).`}
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          onKeyDown={(e) => e.key === "Escape" && setFilter("")}
        />
        <button type="button" className="icon" aria-label="New connection" title="New connection" onClick={() => openConnectionForm()}>＋</button>
      </div>
      {recentEntries.length > 0 && !f && (
        <section aria-label="Recent connections" className="recents">
          {recentEntries.map((e) => (
            <button key={e.id} type="button" className="recent-chip" data-tag={e.env_tag} onClick={() => void useConnections.getState().connect(e.id)}>
              {e.name}
            </button>
          ))}
        </section>
      )}
      {goTo && <GoToObject onClose={() => setGoTo(false)} />}
      <div ref={scrollRef} className="explorer-scroll" role="tree" aria-label="Database objects">
        <div style={{ height: virtualizer.getTotalSize(), position: "relative" }}>
          {items.map((vi) => (
            <div key={vi.key} data-index={vi.index} ref={virtualizer.measureElement} className="tree-slot" style={{ transform: `translateY(${vi.start}px)` }}>
              {renderRow(rows[vi.index])}
            </div>
          ))}
        </div>
      </div>
      {menu && <ContextMenu target={menu.target} x={menu.x} y={menu.y} onClose={() => setMenu(null)} />}
    </div>
  );
}
