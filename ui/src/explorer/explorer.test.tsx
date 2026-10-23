import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { mockIPC } from "@tauri-apps/api/mocks";
import { beforeAll, beforeEach, describe, expect, it } from "vitest";
import { App } from "../app/App";
import { registerTab } from "../app/registry";
import { useWorkspace } from "../app/workspace";
import { useConnections } from "../connection/store";
import { EditorTab } from "../editor/EditorTab";
import type { ConnectionEntry } from "../lib/ipc/connections";
import { useDetails } from "./details/store";
import { initExplorer } from "./index";
import { initShortcuts } from "../app/shortcuts";
import { useTrees } from "./tree/model";

const ws0 = useWorkspace.getState();
const conn0 = useConnections.getState();
const trees0 = useTrees.getState();
const details0 = useDetails.getState();

const entry: ConnectionEntry = {
  id: "c1", name: "Local", host: "localhost", port: 5432, database: "mydb", user: "me",
  maintenance_db: "postgres", sslmode: "prefer", env_tag: "local", password_saved: true,
};

const users = {
  schema: "public", name: "users", kind: "table", partition_of: null, row_estimate: null, comment: null,
  columns: [
    { name: "id", type: "integer", not_null: true, default: null, in_primary_key: false },
    { name: "email", type: "character varying(200)", not_null: true, default: null, in_primary_key: false },
    { name: "nickname", type: "text", not_null: false, default: null, in_primary_key: false },
  ],
};

function backend(opts: { treeState?: string[]; failSchema?: boolean } = {}) {
  const calls: { cmd: string; args: Record<string, unknown> }[] = [];
  let failSchema = !!opts.failSchema;
  mockIPC((cmd, args) => {
    const a = args as Record<string, unknown>;
    calls.push({ cmd, args: a });
    switch (cmd) {
      case "connect":
        return { handle: 1, connection_id: "c1", database: "mydb", purpose: "introspection", server_version_num: 160000, backend_pid: 1, tls: "plaintext" };
      case "tree_state_get":
        return opts.treeState ?? [];
      case "live_connections":
        return [
          { handle: 1, connection_id: "c1", database: "mydb", purpose: "introspection", server_version_num: 160000, backend_pid: 1, tls: "plaintext" },
          // listing databases uses a short-lived maintenance connection: not an "open" database
          { handle: 2, connection_id: "c1", database: "postgres", purpose: "maintenance", server_version_num: 160000, backend_pid: 2, tls: "plaintext" },
        ];
      case "explorer_databases":
        return [{ name: "mydb", is_template: false, allow_connections: true }, { name: "other", is_template: false, allow_connections: true }];
      case "explorer_schemas":
        return [{ name: "public", system: false }, { name: "audit", system: false }];
      case "explorer_schema":
        if (failSchema) {
          failSchema = false;
          throw { kind: "permission", class: "fix_then_retry", message: "could not read schema: permission denied" };
        }
        return a.schema === "public"
          ? { relations: [users, { ...users, name: "orders", columns: [{ name: "id", type: "bigint", not_null: true, in_primary_key: false }] }], routines: [{ schema: "public", name: "touch", kind: "function", identity_args: "integer", result: "void" }] }
          : { relations: [{ ...users, schema: "audit", name: "log" }], routines: [] };
      case "explorer_details":
        return {
          relation: { ...users, row_estimate: 1200 },
          constraints: [{ name: "users_pkey", kind: "primary_key", columns: ["id"], definition: "PRIMARY KEY (id)" }],
          indexes: [{ name: "users_email_key", definition: "CREATE UNIQUE INDEX users_email_key ON public.users USING btree (email)", unique: true, primary: false, valid: true, size_bytes: 16384 }],
          stats: { row_estimate: 1200, total_bytes: 81920, table_bytes: 49152, indexes_bytes: 32768, last_analyze: null },
          edit_key: ["id"],
          view_definition: null,
          raw: { constraints: [], relation: { partitions: [], owner: "me" } },
        };
      case "explorer_search":
        return [{ schema: "audit", name: "log", kind: "table", identity_args: null }];
      case "explorer_script_as":
        return 'SELECT\n    id,\n    email,\n    nickname\nFROM users\nLIMIT 1000;\n';
      case "editor_open":
        return { handle: 2, connection_id: "c1", database: "mydb", purpose: "editor_tab", server_version_num: 160000, backend_pid: 2, tls: "plaintext" };
      default:
        return null;
    }
  });
  return calls;
}

beforeAll(() => {
  Range.prototype.getClientRects = () => [] as unknown as DOMRectList;
  Range.prototype.getBoundingClientRect = () => new DOMRect();
  Object.defineProperty(HTMLElement.prototype, "offsetHeight", { configurable: true, get: () => 800 });
  Object.defineProperty(HTMLElement.prototype, "offsetWidth", { configurable: true, get: () => 300 });
  initExplorer();
  initShortcuts();
  registerTab("editor", EditorTab);
});

beforeEach(() => {
  useWorkspace.setState(ws0, true);
  useConnections.setState({ ...conn0, loaded: true, entries: [entry] }, true);
  useTrees.setState(trees0, true);
  useDetails.setState(details0, true);
});

async function connect(user: ReturnType<typeof userEvent.setup>) {
  await user.click(screen.getByRole("treeitem", { name: "Local" }));
  return screen.findByRole("treeitem", { name: "public.users" });
}

describe("object explorer", () => {
  it("AE1: on first connect a table's columns are one click away, details need no dialog", async () => {
    const user = userEvent.setup();
    backend();
    render(<App />);
    const usersNode = await connect(user); // first connect opened mydb → public → Tables
    expect(screen.getByRole("treeitem", { name: "mydb" })).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByRole("treeitem", { name: "Tables" })).toHaveAttribute("aria-expanded", "true");

    // interaction 1: expand the table → columns and types inline
    await user.click(within(usersNode).getByLabelText("Expand"));
    expect(screen.getByRole("treeitem", { name: "email character varying(200)" })).toBeInTheDocument();
    expect(screen.getByRole("treeitem", { name: "nickname text" })).toHaveTextContent("text, null");

    // interaction 2: select it → details dock shows keys, indexes, stats
    await user.click(within(screen.getByRole("treeitem", { name: "public.users" })).getByText("users"));
    const dock = await screen.findByRole("complementary", { name: "Details" });
    expect(await within(dock).findByText("users_pkey")).toBeInTheDocument();
    expect(within(dock).getByText("users_email_key")).toBeInTheDocument();
    expect(dock).toHaveTextContent("~1,200");
    expect(dock).toHaveTextContent("80 kB");
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });

  it("marks databases with a live connection differently from ones not opened yet", async () => {
    const user = userEvent.setup();
    backend();
    render(<App />);
    await connect(user);
    const mydb = screen.getByRole("treeitem", { name: "mydb" });
    const other = screen.getByRole("treeitem", { name: "other" });
    await waitFor(() => expect(mydb).toHaveAttribute("data-open", "true"));
    expect(mydb).toHaveAttribute("title", "Connected (1 connection)");
    expect(other).not.toHaveAttribute("data-open");
    expect(other).toHaveAttribute("title", "Not connected — expand to open");
  });

  it("restores the saved expansion state instead of the defaults", async () => {
    const user = userEvent.setup();
    const calls = backend({ treeState: ["db:mydb", "db:mydb\u001fschema:audit", "db:mydb\u001fschema:audit\u001ffolder:tables"] });
    render(<App />);
    await user.click(screen.getByRole("treeitem", { name: "Local" }));
    expect(await screen.findByRole("treeitem", { name: "audit.log" })).toBeInTheDocument();
    expect(screen.queryByRole("treeitem", { name: "public.users" })).not.toBeInTheDocument();
    expect(calls.filter((c) => c.cmd === "explorer_schema").map((c) => c.args.schema)).toEqual(["audit"]);
  });

  it("Script as SELECT from the context menu opens the SQL in an editor", async () => {
    const user = userEvent.setup();
    const calls = backend();
    render(<App />);
    const usersNode = await connect(user);
    await user.pointer({ keys: "[MouseRight]", target: usersNode });
    const menu = screen.getByRole("menu");
    await user.click(within(menu).getByRole("menuitem", { name: "SELECT" }));
    expect(calls.find((c) => c.cmd === "explorer_script_as")?.args).toMatchObject({ schema: "public", name: "users", kind: "select" });
    expect(await screen.findByRole("tab", { name: /users \(select\)/ })).toBeInTheDocument();
    expect(document.querySelector(".cm-content")?.textContent).toContain("FROM users");
  });

  it("type-to-filter shows matches across loaded nodes with their ancestors", async () => {
    const user = userEvent.setup();
    backend();
    render(<App />);
    await connect(user);
    await user.type(screen.getByLabelText("Filter objects"), "ord");
    expect(screen.getByRole("treeitem", { name: "public.orders" })).toBeInTheDocument();
    expect(screen.queryByRole("treeitem", { name: "public.users" })).not.toBeInTheDocument();
    expect(screen.getByRole("treeitem", { name: "mydb" })).toBeInTheDocument();
  });

  it("go-to-object jumps to a table in an unexpanded schema", async () => {
    const user = userEvent.setup();
    backend();
    render(<App />);
    await connect(user);
    await user.keyboard("{Control>}p{/Control}");
    await user.type(await screen.findByRole("textbox", { name: "Find object" }), "log");
    await user.click(await screen.findByRole("option", { name: /log/ }));
    const node = await screen.findByRole("treeitem", { name: "audit.log" });
    expect(node).toHaveAttribute("aria-selected", "true");
  });

  it("Ctrl+Shift+P focuses the filter and Ctrl+P finds objects (⌘ on macOS; the other modifier does nothing)", async () => {
    const user = userEvent.setup();
    backend();
    render(<App />);
    await connect(user);
    // jsdom is not macOS: Ctrl is the primary modifier, so ⌘P must not trigger anything
    await user.keyboard("{Meta>}p{/Meta}");
    expect(screen.queryByRole("textbox", { name: "Find object" })).not.toBeInTheDocument();
    await user.keyboard("{Control>}{Shift>}p{/Shift}{/Control}");
    await waitFor(() => expect(screen.getByLabelText("Filter objects")).toHaveFocus());
    expect(screen.queryByRole("textbox", { name: "Find object" })).not.toBeInTheDocument();
    expect(screen.getByLabelText("Filter objects")).toHaveAttribute("placeholder", "Filter (Ctrl+Shift+P) · find any object (Ctrl+P)");
    await user.keyboard("{Control>}p{/Control}");
    expect(await screen.findByRole("textbox", { name: "Find object" })).toBeInTheDocument();
  });

  it("the shortcuts bring the Explorer back when another view is showing", async () => {
    const user = userEvent.setup();
    backend();
    render(<App />);
    await connect(user);
    act(() => useWorkspace.getState().setView("scripts"));
    expect(screen.queryByLabelText("Filter objects")).not.toBeInTheDocument();
    await user.keyboard("{Control>}{Shift>}p{/Shift}{/Control}");
    await waitFor(() => expect(screen.getByLabelText("Filter objects")).toHaveFocus());
  });

  it("a failing node shows its error inline with a retry; the rest of the tree works", async () => {
    const user = userEvent.setup();
    backend({ failSchema: true });
    render(<App />);
    await user.click(screen.getByRole("treeitem", { name: "Local" }));
    expect(await screen.findByText("could not read schema: permission denied")).toBeInTheDocument();
    expect(screen.getByRole("treeitem", { name: "audit" })).toBeInTheDocument();
    await act(async () => {
      await user.click(screen.getByRole("button", { name: "Retry" }));
    });
    expect(await screen.findByRole("treeitem", { name: "public.users" })).toBeInTheDocument();
  });
});
