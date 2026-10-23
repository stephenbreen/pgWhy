import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { mockIPC } from "@tauri-apps/api/mocks";
import { beforeAll, beforeEach, describe, expect, it } from "vitest";
import { App } from "../app/App";
import { registerTab, registerView } from "../app/registry";
import { useWorkspace } from "../app/workspace";
import { useConnections } from "../connection/store";
import { EditorTab } from "../editor/EditorTab";
import type { ConnectionEntry } from "../lib/ipc/connections";
import { ScriptsView } from "./ScriptsView";
import { TextTab } from "./TextTab";

const ws0 = useWorkspace.getState();
const conn0 = useConnections.getState();
const fp = { sha256: "h", size: 8, modified_ms: 1 };
const mk = (id: string, name: string): ConnectionEntry => ({
  id, name, host: "h", port: 5432, database: name.toLowerCase(), user: "u", maintenance_db: "postgres", sslmode: "prefer", env_tag: "local", password_saved: true,
});

function backend() {
  const calls: { cmd: string; args: Record<string, unknown> }[] = [];
  mockIPC((cmd, args) => {
    const a = args as Record<string, unknown>;
    calls.push({ cmd, args: a });
    switch (cmd) {
      case "library_list":
        return {
          root: "/lib",
          tags: ["daily"],
          scripts: [
            { id: "s1", path: "reports/daily.sql", name: "daily", folder: "reports", tags: ["daily"], fingerprint: fp, missing: false },
            { id: "s2", path: "fix.sql", name: "fix", folder: "", tags: [], fingerprint: fp, missing: false },
            { id: "s3", path: "runbooks/restore.md", name: "restore.md", folder: "runbooks", tags: [], fingerprint: fp, missing: false },
          ].filter((s) => !a.query || s.name.includes(String(a.query))),
        };
      case "script_recents":
        return [
          { entry: { path: "/lib/reports/daily.sql", library_id: "s1", connection_id: "c2", database: "local", last_used_ms: Date.now() - 86_400_000, last_action: "run" }, resolved_path: "/lib/reports/daily.sql", state: "available" },
          { entry: { path: "/tmp/gone.sql", last_used_ms: Date.now(), last_action: "open" }, resolved_path: "/tmp/gone.sql", state: "not_found" },
        ];
      case "script_read":
        if (a.id === "s3")
          return { info: { id: "s3", path: "runbooks/restore.md", name: "restore.md", folder: "runbooks", tags: [], fingerprint: fp, missing: false }, path: "/lib/runbooks/restore.md", file: { content: "# Restore", fingerprint: fp } };
        return { info: { id: "s1", path: "reports/daily.sql", name: "daily", folder: "reports", tags: [], fingerprint: fp, missing: false }, path: "/lib/reports/daily.sql", file: { content: "select now()", fingerprint: fp } };
      case "pick_open_path":
        return "/Users/me/notes/todo.md";
      case "file_read":
        return { content: "- vacuum", fingerprint: fp };
      case "script_save":
        return { info: { id: "s9", path: String(a.path), name: String(a.path), folder: "", tags: [], fingerprint: fp, missing: false }, path: `/lib/${a.path}`, file: { content: a.content, fingerprint: fp } };
      case "editor_open":
        return { handle: 1, connection_id: a.connectionId, database: a.database, purpose: "editor_tab", server_version_num: 160000, backend_pid: 1, tls: "plaintext" };
      default:
        return null;
    }
  });
  return calls;
}

beforeAll(() => {
  Range.prototype.getClientRects = () => [] as unknown as DOMRectList;
  Range.prototype.getBoundingClientRect = () => new DOMRect();
});

beforeEach(() => {
  useWorkspace.setState({ ...ws0, activeView: "scripts" }, true);
  useConnections.setState({ ...conn0, loaded: true, entries: [mk("c1", "Remote"), mk("c2", "Local")], activeId: "c1" }, true);
  registerView("scripts", ScriptsView);
  registerTab("editor", EditorTab);
  registerTab("text", TextTab);
});

describe("scripts view", () => {
  it("reopens yesterday's script with the connection it last ran against (AE3)", async () => {
    const user = userEvent.setup();
    const calls = backend();
    render(<App />);
    const recent = await screen.findByRole("region", { name: "Recent scripts" });
    expect(within(recent).getByText("1 d ago · Local/local")).toBeInTheDocument();
    await user.click(within(recent).getByRole("button", { name: /daily\.sql/ }));
    expect(calls.find((c) => c.cmd === "editor_open")?.args).toMatchObject({ connectionId: "c2", database: "local" });
    const toolbar = await screen.findByRole("toolbar", { name: "Editor" });
    expect(within(toolbar).getByLabelText("Run against connection")).toHaveDisplayValue("Local");
  });

  it("library scripts open bound to the active connection", async () => {
    const user = userEvent.setup();
    const calls = backend();
    render(<App />);
    const lib = await screen.findByRole("region", { name: "Library" });
    await user.click(within(lib).getByRole("button", { name: /fix/ }));
    expect(calls.find((c) => c.cmd === "editor_open")?.args).toMatchObject({ connectionId: "c1", database: "remote" });
  });

  it("missing recents can be removed, and search filters the library", async () => {
    const user = userEvent.setup();
    const calls = backend();
    render(<App />);
    await user.click(await screen.findByRole("button", { name: "Remove gone.sql from recents" }));
    expect(calls.find((c) => c.cmd === "script_recent_remove")?.args).toEqual({ path: "/tmp/gone.sql" });
    await user.type(screen.getByLabelText("Search scripts"), "dai");
    const lib = screen.getByRole("region", { name: "Library" });
    expect(await within(lib).findByText("daily", { selector: ".script-name" })).toBeInTheDocument();
    expect(within(lib).queryByText("fix")).not.toBeInTheDocument();
  });

  it("opens a .md file from disk as a note with no connection, and adds it to the library", async () => {
    const user = userEvent.setup();
    const calls = backend();
    render(<App />);
    await user.click(await screen.findByRole("button", { name: "Open…" }));
    const toolbar = await screen.findByRole("toolbar", { name: "Text editor" });
    expect(screen.getByRole("tab", { name: /todo\.md/ })).toHaveAttribute("aria-selected", "true");
    expect(calls.some((c) => c.cmd === "editor_open")).toBe(false);
    await user.click(within(toolbar).getByRole("button", { name: "Add to library" }));
    const path = screen.getByLabelText("Script path");
    expect(path).toHaveValue("todo.md");
    await user.clear(path);
    await user.type(path, "notes/todo{Enter}");
    // the note keeps its extension instead of becoming notes/todo.sql
    expect(calls.find((c) => c.cmd === "script_save")?.args).toEqual({ path: "notes/todo.md", content: "- vacuum" });
    expect(await screen.findByRole("tab", { name: /notes\/todo\.md/ })).toBeInTheDocument();
    expect(within(toolbar).queryByRole("button", { name: "Add to library" })).not.toBeInTheDocument();
  });

  it("library notes open in a text tab", async () => {
    const user = userEvent.setup();
    const calls = backend();
    render(<App />);
    const lib = await screen.findByRole("region", { name: "Library" });
    await user.click(within(lib).getByRole("button", { name: /restore\.md/ }));
    expect(await screen.findByRole("toolbar", { name: "Text editor" })).toBeInTheDocument();
    expect(calls.some((c) => c.cmd === "editor_open")).toBe(false);
  });
});
