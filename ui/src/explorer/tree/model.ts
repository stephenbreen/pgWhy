// Explorer tree data: lazily loaded catalog per connection, expansion state persisted by
// name (so it survives restarts and swaps), and a flattener producing the visible rows.

import { create } from "zustand";
import * as ex from "../../lib/ipc/explorer";
import { toAppError, type AppError } from "../../lib/ipc/errors";
import { completionChanged } from "../../editor/completion";

type Load<T> = { state: "loading" } | { state: "loaded"; value: T } | { state: "error"; error: AppError };

export interface ConnTree {
  databases?: Load<ex.DatabaseNode[]>;
  schemas: Record<string, Load<ex.SchemaNode[]>>;
  contents: Record<string, Load<ex.SchemaContents>>; // key: db/schema
  expanded: Set<string>;
  restored: boolean;
}

// Node ids are "kind:name" segments joined by U+001F (names may contain "/" or ":"), which
// is also how expansion state is persisted (one segment list per expanded node).
export const SEP = "\u001f";
export const dbPath = (db: string) => `db:${db}`;
export const schemaPath = (db: string, schema: string) => `${dbPath(db)}${SEP}schema:${schema}`;
export const folderPath = (db: string, schema: string, folder: Folder) => `${schemaPath(db, schema)}${SEP}folder:${folder}`;
export const relPath = (db: string, schema: string, name: string) => `${schemaPath(db, schema)}${SEP}rel:${name}`;
export const routinePath = (db: string, schema: string, name: string, args: string) => `${schemaPath(db, schema)}${SEP}fn:${name}(${args})`;
const contentKey = (db: string, schema: string) => `${db}${SEP}${schema}`;

/** Parse a node id into its db/schema and whether it's deeper than the schema. */
function parseId(id: string): { db: string; schema?: string; deeper: boolean } | null {
  const segs = id.split(SEP);
  if (!segs[0]?.startsWith("db:")) return null;
  const db = segs[0].slice(3);
  const schema = segs[1]?.startsWith("schema:") ? segs[1].slice(7) : undefined;
  return { db, schema, deeper: segs.length > 2 };
}

export type Folder = "tables" | "views" | "matviews" | "foreign" | "sequences" | "functions" | "procedures";
const FOLDERS: { folder: Folder; label: string; kinds: (ex.RelKind | ex.RoutineKind)[] }[] = [
  { folder: "tables", label: "Tables", kinds: ["table", "partitioned_table"] },
  { folder: "views", label: "Views", kinds: ["view"] },
  { folder: "matviews", label: "Materialized views", kinds: ["matview"] },
  { folder: "foreign", label: "Foreign tables", kinds: ["foreign_table"] },
  { folder: "sequences", label: "Sequences", kinds: ["sequence"] },
  { folder: "functions", label: "Functions", kinds: ["function", "aggregate", "window"] },
  { folder: "procedures", label: "Procedures", kinds: ["procedure"] },
];

export function folderFor(kind: ex.RelKind | ex.RoutineKind): Folder {
  return FOLDERS.find((f) => f.kinds.includes(kind))!.folder;
}

export type TreeRow =
  | { kind: "database"; id: string; depth: number; db: ex.DatabaseNode; expanded: boolean }
  | { kind: "schema"; id: string; depth: number; db: string; schema: ex.SchemaNode; expanded: boolean }
  | { kind: "folder"; id: string; depth: number; db: string; schema: string; folder: Folder; label: string; count: number; expanded: boolean }
  | { kind: "relation"; id: string; depth: number; db: string; rel: ex.RelationNode; expanded: boolean }
  | { kind: "column"; id: string; depth: number; db: string; rel: ex.RelationNode; column: ex.ColumnNode }
  | { kind: "routine"; id: string; depth: number; db: string; routine: ex.RoutineNode }
  | { kind: "status"; id: string; depth: number; state: "loading" | "error" | "empty"; error?: AppError; retry?: () => void };

interface TreesState {
  trees: Record<string, ConnTree>;
  selected: { connectionId: string; id: string } | null;
  ensure: (connectionId: string) => ConnTree;
  loadDatabases: (connectionId: string) => Promise<void>;
  loadSchemas: (connectionId: string, db: string) => Promise<void>;
  loadContents: (connectionId: string, db: string, schema: string, refresh?: boolean) => Promise<void>;
  toggle: (connectionId: string, id: string, open?: boolean) => void;
  restore: (connectionId: string, defaultDb: string) => Promise<void>;
  select: (connectionId: string, id: string) => void;
  invalidate: (connectionId: string, db?: string) => void;
}

const empty = (): ConnTree => ({ schemas: {}, contents: {}, expanded: new Set(), restored: false });

const persistTimers: Record<string, ReturnType<typeof setTimeout>> = {};
function persist(connectionId: string, expanded: Set<string>) {
  clearTimeout(persistTimers[connectionId]);
  persistTimers[connectionId] = setTimeout(() => void ex.setTreeState(connectionId, [...expanded].sort()).catch(() => {}), 400);
}

export const useTrees = create<TreesState>((set, get) => {
  const update = (connectionId: string, f: (t: ConnTree) => ConnTree) =>
    set((s) => ({ trees: { ...s.trees, [connectionId]: f(s.trees[connectionId] ?? empty()) } }));

  // expanding a node loads what it needs
  const loadFor = (connectionId: string, id: string) => {
    const p = parseId(id);
    if (!p) return;
    if (!p.schema) void get().loadSchemas(connectionId, p.db);
    else if (!p.deeper) void get().loadContents(connectionId, p.db, p.schema);
  };

  return {
    trees: {},
    selected: null,
    ensure: (connectionId) => get().trees[connectionId] ?? empty(),

    loadDatabases: async (connectionId) => {
      update(connectionId, (t) => ({ ...t, databases: { state: "loading" } }));
      try {
        const value = await ex.listDatabases(connectionId);
        update(connectionId, (t) => ({ ...t, databases: { state: "loaded", value } }));
      } catch (e) {
        update(connectionId, (t) => ({ ...t, databases: { state: "error", error: toAppError(e) } }));
      }
    },

    loadSchemas: async (connectionId, db) => {
      const cur = get().trees[connectionId]?.schemas[db];
      if (cur?.state === "loaded" || cur?.state === "loading") return;
      update(connectionId, (t) => ({ ...t, schemas: { ...t.schemas, [db]: { state: "loading" } } }));
      try {
        const value = await ex.listSchemas(connectionId, db);
        update(connectionId, (t) => ({ ...t, schemas: { ...t.schemas, [db]: { state: "loaded", value } } }));
      } catch (e) {
        update(connectionId, (t) => ({ ...t, schemas: { ...t.schemas, [db]: { state: "error", error: toAppError(e) } } }));
      }
    },

    loadContents: async (connectionId, db, schema, refresh) => {
      const key = contentKey(db, schema);
      const cur = get().trees[connectionId]?.contents[key];
      if (!refresh && (cur?.state === "loaded" || cur?.state === "loading")) return;
      if (refresh) await ex.refreshExplorer(connectionId, db, schema).catch(() => {});
      update(connectionId, (t) => ({ ...t, contents: { ...t.contents, [key]: { state: "loading" } } }));
      try {
        const value = await ex.schemaContents(connectionId, db, schema);
        update(connectionId, (t) => ({ ...t, contents: { ...t.contents, [key]: { state: "loaded", value } } }));
        completionChanged();
      } catch (e) {
        update(connectionId, (t) => ({ ...t, contents: { ...t.contents, [key]: { state: "error", error: toAppError(e) } } }));
      }
    },

    toggle: (connectionId, id, open) => {
      const t = get().ensure(connectionId);
      const expanded = new Set(t.expanded);
      const willOpen = open ?? !expanded.has(id);
      if (willOpen) expanded.add(id);
      else expanded.delete(id);
      update(connectionId, (x) => ({ ...x, expanded }));
      persist(connectionId, expanded);
      if (willOpen) loadFor(connectionId, id);
    },

    // first connect: default database, its public schema and its Tables folder open, so a
    // table's columns are one click away (AE1); later connects restore what was open
    restore: async (connectionId, defaultDb) => {
      if (get().trees[connectionId]?.restored) return;
      let saved: string[] = [];
      try {
        saved = await ex.getTreeState(connectionId);
      } catch {
        /* no saved state */
      }
      const expanded = new Set(
        saved.length ? saved : [dbPath(defaultDb), schemaPath(defaultDb, "public"), folderPath(defaultDb, "public", "tables")],
      );
      update(connectionId, (t) => ({ ...t, expanded, restored: true }));
      await get().loadDatabases(connectionId);
      for (const id of expanded) loadFor(connectionId, id);
    },

    select: (connectionId, id) => set({ selected: { connectionId, id } }),

    // post-swap / manual refresh: drop cached catalog, keep expansion (re-loads by name)
    invalidate: (connectionId, db) => {
      update(connectionId, (t) => {
        if (!db) return { ...empty(), expanded: t.expanded, restored: false };
        const schemas = { ...t.schemas };
        delete schemas[db];
        const contents = Object.fromEntries(Object.entries(t.contents).filter(([k]) => !k.startsWith(`${db}${SEP}`)));
        return { ...t, schemas, contents };
      });
      const t = get().trees[connectionId];
      if (t) for (const id of t.expanded) if (!db || id.startsWith(dbPath(db))) loadFor(connectionId, id);
    },
  };
});

function statusRow(id: string, depth: number, l: Load<unknown> | undefined, retry: () => void): TreeRow | null {
  if (!l || l.state === "loading") return { kind: "status", id: `${id}#loading`, depth, state: "loading" };
  if (l.state === "error") return { kind: "status", id: `${id}#error`, depth, state: "error", error: l.error, retry };
  return null;
}

const matches = (name: string, filter: string) => name.toLowerCase().includes(filter);

/** Flatten one connection's tree into visible rows. `filter` (lower-cased) shows matching
 *  loaded objects and their ancestors, expanding through collapsed nodes. */
export function flatten(connectionId: string, t: ConnTree, depth0: number, filter: string): TreeRow[] {
  const rows: TreeRow[] = [];
  const st = useTrees.getState();
  const open = (id: string) => (filter ? true : t.expanded.has(id));
  const dbs = t.databases;
  const s = statusRow(`${connectionId}`, depth0, dbs, () => void st.loadDatabases(connectionId));
  if (s) return [s];
  if (dbs?.state !== "loaded") return rows;
  for (const db of dbs.value) {
    if (db.is_template) continue;
    const dId = dbPath(db.name);
    const dbRows: TreeRow[] = [];
    const dbOpen = open(dId) && (!filter || t.expanded.has(dId) || t.schemas[db.name]?.state === "loaded");
    if (dbOpen) {
      const schemas = t.schemas[db.name];
      const ss = statusRow(dId, depth0 + 1, schemas, () => void st.loadSchemas(connectionId, db.name));
      if (ss && !filter) dbRows.push(ss);
      if (schemas?.state === "loaded") {
        for (const sc of schemas.value) {
          const sId = schemaPath(db.name, sc.name);
          const contents = t.contents[contentKey(db.name, sc.name)];
          const scRows: TreeRow[] = [];
          if (open(sId) && (!filter || contents?.state === "loaded")) {
            const cs = statusRow(sId, depth0 + 2, contents, () => void st.loadContents(connectionId, db.name, sc.name, true));
            if (cs && !filter) scRows.push(cs);
            if (contents?.state === "loaded") {
              for (const f of FOLDERS) {
                const rels = contents.value.relations.filter((r) => f.kinds.includes(r.kind) && !r.partition_of);
                const fns = contents.value.routines.filter((r) => f.kinds.includes(r.kind));
                const count = rels.length + fns.length;
                if (count === 0) continue;
                const fId = folderPath(db.name, sc.name, f.folder);
                const fRows: TreeRow[] = [];
                if (open(fId)) {
                  for (const rel of rels) {
                    const rId = relPath(db.name, sc.name, rel.name);
                    const colRows: TreeRow[] = [];
                    const relOpen = filter ? false : t.expanded.has(rId);
                    const relHit = !filter || matches(rel.name, filter);
                    const parts = contents.value.relations.filter((p) => p.partition_of === `${rel.schema}.${rel.name}`);
                    if (relOpen || (filter && rel.columns.some((c) => matches(c.name, filter)))) {
                      for (const c of rel.columns) {
                        if (filter && !relHit && !matches(c.name, filter)) continue;
                        colRows.push({ kind: "column", id: `${rId}${SEP}col:${c.name}`, depth: depth0 + 5, db: db.name, rel, column: c });
                      }
                    }
                    if (relHit || colRows.length) {
                      fRows.push({ kind: "relation", id: rId, depth: depth0 + 4, db: db.name, rel, expanded: relOpen || colRows.length > 0 });
                      fRows.push(...colRows);
                      if (relOpen) {
                        for (const p of parts) {
                          fRows.push({ kind: "relation", id: relPath(db.name, sc.name, p.name), depth: depth0 + 5, db: db.name, rel: p, expanded: false });
                        }
                      }
                    }
                  }
                  for (const fn of fns) {
                    if (filter && !matches(fn.name, filter)) continue;
                    fRows.push({ kind: "routine", id: routinePath(db.name, sc.name, fn.name, fn.identity_args), depth: depth0 + 4, db: db.name, routine: fn });
                  }
                }
                if (!filter || fRows.length) {
                  scRows.push({ kind: "folder", id: fId, depth: depth0 + 3, db: db.name, schema: sc.name, folder: f.folder, label: f.label, count, expanded: open(fId) });
                  scRows.push(...fRows);
                }
              }
            }
          }
          if (!filter || scRows.length || matches(sc.name, filter)) {
            dbRows.push({ kind: "schema", id: sId, depth: depth0 + 2, db: db.name, schema: sc, expanded: open(sId) && (!filter || scRows.length > 0) });
            dbRows.push(...scRows);
          }
        }
      }
    }
    if (!filter || dbRows.length || matches(db.name, filter)) {
      rows.push({ kind: "database", id: dId, depth: depth0 + 1, db, expanded: dbOpen });
      rows.push(...dbRows);
    }
  }
  return rows;
}

export const FOLDER_LIST = FOLDERS;
