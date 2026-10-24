// Dev-only: the real UI against an in-browser fake backend (mockIPC), for visual review in
// a normal browser (`npm run dev`, then open /demo.html). Never bundled into the app.

import { mockIPC, mockWindows } from "@tauri-apps/api/mocks";
import { encodeChunk } from "../grid/protocol";

mockWindows("main");

const conn = (id: string, name: string, tag: string, extra = {}) => ({
  id, name, host: `${name.toLowerCase().replace(/\s+/g, "-")}.internal`, port: 5432, database: "shop", user: "app",
  maintenance_db: "postgres", sslmode: "verify-full", env_tag: tag, password_saved: true, ...extra,
});
const connections = [
  conn("c-local", "Local", "local", { host: "localhost", sslmode: "prefer" }),
  conn("c-prod", "Shop production", "production", { group: "Shop", tunnel: { host: "bastion.shop.io", port: 22, user: "deploy", auth: { method: "key", key_path: "~/.ssh/id_ed25519" } } }),
  conn("c-stage", "Shop staging", "staging", { group: "Shop" }),
];

const cols = (...c: [string, string, boolean?][]) => c.map(([name, type, nn]) => ({ name, type, not_null: !!nn, default: null, in_primary_key: false }));
const relations = [
  { schema: "public", name: "customers", kind: "table", columns: cols(["id", "bigint", true], ["email", "character varying(320)", true], ["name", "text"], ["created_at", "timestamp with time zone", true]) },
  { schema: "public", name: "orders", kind: "table", columns: cols(["id", "bigint", true], ["customer_id", "bigint", true], ["total", "numeric(12,2)", true], ["status", "text", true], ["placed_at", "timestamp with time zone", true]) },
  { schema: "public", name: "order_lines", kind: "table", columns: cols(["order_id", "bigint", true], ["line_no", "integer", true], ["sku", "text", true], ["qty", "integer", true]) },
  { schema: "public", name: "products", kind: "table", columns: cols(["sku", "text", true], ["title", "text", true], ["price", "numeric(10,2)", true]) },
  { schema: "public", name: "active_customers", kind: "view", columns: cols(["id", "bigint"], ["email", "character varying(320)"]) },
  { schema: "public", name: "daily_sales", kind: "matview", columns: cols(["day", "date"], ["revenue", "numeric"]) },
  { schema: "public", name: "orders_id_seq", kind: "sequence", columns: [] },
];

const statuses = ["paid", "shipped", "refunded", "pending"];
const orders = Array.from({ length: 5000 }, (_, i) => [
  String(i + 1), String(1000 + (i % 377)), (12.5 + ((i * 7.31) % 400)).toFixed(2), statuses[i % 4],
  new Date(Date.UTC(2026, 8, 1) + i * 3_600_000).toISOString().replace("T", " ").slice(0, 19) + "+00",
]);

let channels: Record<string, (b: ArrayBuffer) => void> = {};
let txn = "idle";

mockIPC(async (cmd, args) => {
  const a = args as Record<string, any>;
  switch (cmd) {
    case "list_connections":
      return { connections, recents: ["c-local", "c-prod"], keyring_available: true };
    case "connect":
      await new Promise((r) => setTimeout(r, 250));
      return { handle: 1, connection_id: a.id, database: "shop", purpose: "introspection", server_version_num: 170002, backend_pid: 4242, tls: a.id === "c-local" ? "plaintext" : "encrypted_verified" };
    case "tree_state_get":
      return [];
    case "live_connections":
      return [
        { handle: 1, connection_id: "c-prod", database: "shop", purpose: "introspection", server_version_num: 170002, backend_pid: 1, tls: "encrypted_verified" },
      ];
    case "explorer_databases":
      return [{ name: "shop", is_template: false, allow_connections: true }, { name: "analytics", is_template: false, allow_connections: true }, { name: "postgres", is_template: false, allow_connections: true }];
    case "explorer_schemas":
      return [{ name: "public", system: false }, { name: "audit", system: false }, { name: "pg_catalog", system: true }];
    case "explorer_schema":
      return a.schema === "public"
        ? { relations, routines: [{ schema: "public", name: "refresh_daily_sales", kind: "procedure", identity_args: "", result: null }, { schema: "public", name: "order_total", kind: "function", identity_args: "order_id bigint", result: "numeric" }] }
        : { relations: [{ schema: a.schema, name: "events", kind: "partitioned_table", columns: cols(["id", "bigint", true], ["at", "timestamp with time zone", true], ["payload", "jsonb"]) }], routines: [] };
    case "explorer_details": {
      const rel = relations.find((r) => r.name === a.name) ?? relations[1];
      return {
        relation: rel,
        constraints: [
          { name: `${rel.name}_pkey`, kind: "primary_key", columns: ["id"], definition: "PRIMARY KEY (id)" },
          { name: `${rel.name}_customer_id_fkey`, kind: "foreign_key", columns: ["customer_id"], definition: "FOREIGN KEY (customer_id) REFERENCES customers(id) ON DELETE RESTRICT" },
          { name: `${rel.name}_total_check`, kind: "check", columns: ["total"], definition: "CHECK ((total >= (0)::numeric))" },
        ],
        indexes: [
          { name: `${rel.name}_pkey`, definition: `CREATE UNIQUE INDEX ${rel.name}_pkey ON public.${rel.name} USING btree (id)`, unique: true, primary: true, valid: true, size_bytes: 11_337_728 },
          { name: `${rel.name}_placed_at_idx`, definition: `CREATE INDEX ${rel.name}_placed_at_idx ON public.${rel.name} USING btree (placed_at DESC)`, unique: false, primary: false, valid: true, size_bytes: 8_978_432 },
        ],
        stats: { row_estimate: 482_113, total_bytes: 96_468_992, table_bytes: 71_303_168, indexes_bytes: 20_316_160, last_analyze: "2026-10-08T22:14:03Z", last_autovacuum: "2026-10-09T03:02:41Z" },
        edit_key: ["id"],
        raw: { constraints: [], relation: { partitions: [], owner: "app" } },
      };
    }
    case "explorer_search":
      return relations.filter((r) => r.name.includes(a.pattern)).map((r) => ({ schema: r.schema, name: r.name, kind: r.kind }));
    case "completion_schema":
      return { public: Object.fromEntries(relations.map((r) => [r.name, r.columns.map((c) => c.name)])) };
    case "explorer_script_as":
      return "SELECT\n    id,\n    customer_id,\n    total,\n    status,\n    placed_at\nFROM public.orders\nLIMIT 1000;\n";
    case "editor_open":
      return { handle: 7, connection_id: a.connectionId, database: a.database, purpose: "editor_tab", server_version_num: 170002, backend_pid: 5151, tls: "encrypted_verified" };
    case "editor_run": {
      const ch = a.events;
      const sql: string = a.sql;
      setTimeout(() => {
        ch.onmessage({ type: "statement_started", index: 0, from: 0, to: sql.length });
        if (/\bbegin\b/i.test(sql)) { txn = "in_transaction"; ch.onmessage({ type: "txn", status: "in_transaction" }); }
        if (/frm/.test(sql)) {
          const pos = new TextEncoder().encode(sql.slice(0, sql.indexOf("frm"))).length;
          ch.onmessage({ type: "error", index: 0, error: { kind: "sql", class: "fix_then_retry", message: 'syntax error at or near "frm"', sqlstate: "42601", position: pos } });
        } else {
          ch.onmessage({ type: "notice", index: 0, notice: { severity: "NOTICE", code: "00000", message: "using index scan on orders_placed_at_idx" } });
          ch.onmessage({ type: "result", index: 0, result_id: "r-orders", columns: [{ name: "id", type_name: "bigint" }, { name: "customer_id", type_name: "bigint" }, { name: "total", type_name: "numeric(12,2)" }, { name: "status", type_name: "text" }, { name: "placed_at", type_name: "timestamp with time zone" }] });
          ch.onmessage({ type: "statement_done", index: 0, command: "SELECT", rows_affected: orders.length, duration_ms: 38.2, result_id: "r-orders", row_count: orders.length, truncated: false });
        }
        ch.onmessage({ type: "finished", statements: 1, succeeded: 1, errors: /frm/.test(sql) ? 1 : 0, skipped: 0, cancelled: false, connection_lost: false, transaction_lost: false, txn: txn === "in_transaction" ? "in_transaction" : "idle", duration_ms: 41 });
      }, 120);
      return null;
    }
    case "result_subscribe":
      channels[a.resultId] = (b) => a.channel.onmessage(b);
      return null;
    case "result_window":
      setTimeout(() => channels[a.resultId]?.(encodeChunk(a.start, orders.slice(a.start, a.start + a.count), 5)), 5);
      return null;
    case "grid_open":
    case "grid_requery":
      return { result_id: "r-orders", columns: [{ name: "id", type: "bigint", key: true, writable: true }, { name: "customer_id", type: "bigint", key: false, writable: true }, { name: "total", type: "numeric(12,2)", key: false, writable: true }, { name: "status", type: "text", key: false, writable: true }, { name: "placed_at", type: "timestamp with time zone", key: false, writable: true }], row_count: 100, sql: "SELECT id, customer_id, total, status, placed_at FROM public.orders ORDER BY id LIMIT 100", editable: true, read_only_reason: null, n: 100 };
    case "workflow_list":
      return [{ id: "w1", name: "Refresh local shop", source: { connection: "c-prod", database: "shop" }, target: { connection: "c-local", database: "shop" }, post_restore_script: null, options: { strategy: "staging_swap", exclude_table_data: ["audit.events"], jobs: null, no_owner: true, no_privileges: true, error_policy: "stop_on_first", script_mode: "transaction", sanity_query: null }, has_succeeded: true }];
    case "library_list":
      return { root: "~/pgWhy/scripts", tags: ["daily"], scripts: [{ id: "s1", path: "reports/daily_sales.sql", name: "daily_sales", folder: "reports", tags: ["daily"], fingerprint: { sha256: "", size: 1, modified_ms: 1 }, missing: false }, { id: "s2", path: "post-restore/anonymize.sql", name: "anonymize", folder: "post-restore", tags: [], fingerprint: { sha256: "", size: 1, modified_ms: 1 }, missing: false }] };
    case "script_recents":
      return [{ entry: { path: "~/pgWhy/scripts/reports/daily_sales.sql", library_id: "s1", connection_id: "c-prod", database: "shop", last_used_ms: Date.now() - 86_400_000, last_action: "run" }, resolved_path: "~/pgWhy/scripts/reports/daily_sales.sql", state: "available" }];
    default:
      return null;
  }
});

await import("../main");

// ?scene=… drives the UI into a state for screenshots (headless review)
const scene = new URLSearchParams(location.search).get("scene");
if (scene) {
  const { useConnections } = await import("../connection/store");
  const { openEditorTab } = await import("../editor/EditorTab");
  const { openTopN } = await import("../grid/topn/TopNTab");
  const { useWorkspace } = await import("../app/workspace");
  const wait = (ms: number) => new Promise((r) => setTimeout(r, ms));
  await wait(300);
  await useConnections.getState().connect("c-prod");
  await wait(600);
  if (scene === "editor") {
    openEditorTab("c-prod", "shop", { text: "-- last 5000 orders\nselect id, customer_id, total, status, placed_at\nfrom orders\norder by placed_at desc;\n", title: "orders.sql" });
    await wait(500);
    document.querySelector<HTMLButtonElement>(".editor-toolbar .primary")?.click();
  } else if (scene === "grid") {
    openTopN({ connectionId: "c-prod", database: "shop", schema: "public", name: "orders" });
  } else if (scene === "workflows") {
    useWorkspace.getState().setView("workflows");
  }
  if (scene === "details" || scene === "editor" || scene === "grid" || scene === "perf-tree") {
    await wait(300);
    const label = [...document.querySelectorAll(".explorer .node-label")].find((n) => n.textContent === "orders");
    (label as HTMLElement | undefined)?.click();
  }
}

// ?scene=perf-tree: "tree expand/select < 100 ms on cached data" — toggles a loaded
// table node repeatedly and records click → next paint; result lands in document.title
if (scene === "perf-tree") {
  const wait = (ms: number) => new Promise((r) => setTimeout(r, ms));
  await wait(1500);
  const samples: number[] = [];
  for (let i = 0; i < 20; i++) {
    const node = [...document.querySelectorAll<HTMLElement>('.explorer [role="treeitem"]')].find((n) => n.getAttribute("aria-label") === "public.orders");
    const twisty = node?.querySelector<HTMLElement>(".twisty");
    if (!twisty) break;
    const t0 = performance.now();
    twisty.click();
    await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
    samples.push(performance.now() - t0);
    await wait(50);
  }
  samples.sort((a, b) => a - b);
  document.title = `PERF tree toggles=${samples.length} p50=${samples[Math.floor(samples.length / 2)]?.toFixed(1)} max=${samples.at(-1)?.toFixed(1)}`;
}

// ?scene=scroll: expand every table so the tree is taller than the window, then report
// whether the explorer scrolls (title) — layout regression check for the sidebar
if (scene === "scroll") {
  const wait = (ms: number) => new Promise((r) => setTimeout(r, ms));
  await wait(1200);
  for (const t of document.querySelectorAll<HTMLElement>('.explorer [role="treeitem"][aria-expanded="false"] .twisty')) t.click();
  await wait(400);
  for (const t of document.querySelectorAll<HTMLElement>('.explorer [role="treeitem"][aria-expanded="false"] .twisty')) t.click();
  await wait(800);
  const el = document.querySelector<HTMLElement>(".explorer-scroll")!;
  const before = el.scrollTop;
  el.scrollTop = 10_000;
  document.title = `SCROLL client=${el.clientHeight} scroll=${el.scrollHeight} window=${window.innerHeight} moved=${el.scrollTop - before}`;
}
