// Smart 4-level catalog schema discovery for Workflows v2 visual step builders:
//   1. Target DB live catalog (tables + columns with types/NOT NULL/defaults)
//   2. Source DB live catalog (for from_remote_db)
//   3. Backup archive TOC (via workflow_archive_tables for from_backup_file fixed_path)
//   4. Free-text fallback (empty hints, inputs remain editable)

import { useEffect, useMemo, useState } from "react";
import { listSchemas, schemaContents, type ColumnNode } from "../lib/ipc/explorer";
import * as w from "../lib/ipc/workflow";

export interface TableHint {
  schema: string;
  name: string;
  columns: ColumnNode[];
}

export interface CatalogHints {
  schemas: string[];
  tables: TableHint[];
  sourceLabel: string | null;
  loading: boolean;
  columnsFor: (schema: string, table: string) => ColumnNode[];
  tablesInSchema: (schema: string) => TableHint[];
}

async function introspectDatabase(connectionId: string, database: string): Promise<TableHint[]> {
  if (!connectionId || !database.trim()) return [];
  const schemas = await listSchemas(connectionId, database.trim());
  if (!Array.isArray(schemas) || schemas.length === 0) return [];
  const userSchemas = schemas
    .filter((s) => !s.system)
    .sort((a, b) => (a.name === "public" ? -1 : b.name === "public" ? 1 : a.name.localeCompare(b.name)))
    .slice(0, 12);
  const out: TableHint[] = [];
  for (const s of userSchemas) {
    try {
      const contents = await schemaContents(connectionId, database.trim(), s.name);
      if (!contents?.relations) continue;
      for (const rel of contents.relations) {
        if (rel.kind === "table" || rel.kind === "partitioned_table") {
          out.push({ schema: rel.schema, name: rel.name, columns: rel.columns ?? [] });
        }
      }
    } catch {
      // Ignore individual schema introspection failures; keep whatever succeeded.
    }
  }
  return out;
}

export function useCatalogHints(wf: w.RefreshWorkflow): CatalogHints {
  const src = w.effectiveSource(wf);
  const targetConn = wf.target.connection;
  const targetDb = wf.target.database;
  const srcConn = src.mode === "from_remote_db" ? src.source.connection : "";
  const srcDb = src.mode === "from_remote_db" ? src.source.database : "";
  const archivePath =
    src.mode === "from_backup_file" && src.archive.kind === "fixed_path" ? src.archive.path.trim() : "";

  const [tables, setTables] = useState<TableHint[]>([]);
  const [sourceLabel, setSourceLabel] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    if (!targetConn && !srcConn && !archivePath) {
      setTables([]);
      setSourceLabel(null);
      setLoading(false);
      return;
    }

    let cancelled = false;
    const timer = setTimeout(() => {
      void (async () => {
        setLoading(true);
        try {
          // Level 1: Target database live catalog
          if (targetConn && targetDb.trim()) {
            try {
              const found = await introspectDatabase(targetConn, targetDb);
              if (!cancelled && found.length > 0) {
                setTables(found);
                setSourceLabel(`target (${targetDb.trim()})`);
                return;
              }
            } catch {
              // Fall through to level 2/3
            }
          }

          // Level 2: Source database live catalog
          if (srcConn && srcDb.trim()) {
            try {
              const found = await introspectDatabase(srcConn, srcDb);
              if (!cancelled && found.length > 0) {
                setTables(found);
                setSourceLabel(`source (${srcDb.trim()})`);
                return;
              }
            } catch {
              // Fall through to level 3
            }
          }

          // Level 3: Backup archive TOC
          if (archivePath) {
            try {
              const pairs = await w.workflowArchiveTables(archivePath);
              if (!cancelled && Array.isArray(pairs) && pairs.length > 0) {
                setTables(pairs.map(([schema, name]) => ({ schema, name, columns: [] })));
                setSourceLabel("backup archive TOC");
                return;
              }
            } catch {
              // Fall through to level 4
            }
          }

          // Level 4: Free-text fallback
          if (!cancelled) {
            setTables([]);
            setSourceLabel(null);
          }
        } finally {
          if (!cancelled) setLoading(false);
        }
      })();
    }, 250);

    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, [targetConn, targetDb, srcConn, srcDb, archivePath]);

  return useMemo(() => {
    const schemaSet = new Set<string>(["public"]);
    for (const t of tables) schemaSet.add(t.schema);
    const schemas = Array.from(schemaSet).sort((a, b) => (a === "public" ? -1 : b === "public" ? 1 : a.localeCompare(b)));
    return {
      schemas,
      tables,
      sourceLabel,
      loading,
      columnsFor: (schema: string, table: string) => {
        const s = schema.trim() || "public";
        const t = table.trim();
        return tables.find((x) => x.schema === s && x.name === t)?.columns ?? [];
      },
      tablesInSchema: (schema: string) => {
        const s = schema.trim();
        return s ? tables.filter((x) => x.schema === s) : tables;
      },
    };
  }, [tables, sourceLabel, loading]);
}
