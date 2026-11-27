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
import { useScriptLibrary } from "./store";

const ws0 = useWorkspace.getState();
const conn0 = useConnections.getState();
const fp = { sha256: "h", size: 8, modified_ms: 1 };
const mk = (id: string, name: string): ConnectionEntry => ({
  id, name, host: "h", port: 5432, database: name.toLowerCase(), user: "u", maintenance_db: "postgres", sslmode: "prefer", env_tag: "local", password_saved: true,
});

function backend() {
  const calls: { cmd: string; args: Record<string, unknown> }[] = [];
  let scripts = [
    { id: "s1", path: "reports/daily.sql", name: "daily", folder: "reports", tags: ["daily"], fingerprint: fp, missing: false },
    { id: "s2", path: "fix.sql", name: "fix", folder: "", tags: [], fingerprint: fp, missing: false },
    { id: "s3", path: "runbooks/restore.md", name: "restore.md", folder: "runbooks", tags: [], fingerprint: fp, missing: false },
  ];
  mockIPC((cmd, args) => {
    const a = args as Record<string, unknown>;
    calls.push({ cmd, args: a });
    switch (cmd) {
      case "library_list": {
        const q = a.query ? String(a.query) : "";
        const tags = Array.from(new Set(scripts.flatMap((s) => s.tags))).sort();
        return {
          root: "/lib",
          tags,
          scripts: scripts.filter((s) => !q || s.name.includes(q) || s.tags.some((t) => t.includes(q))),
        };
      }
      case "script_recents":
        return [
          { entry: { path: "/lib/reports/daily.sql", library_id: "s1", connection_id: "c2", database: "local", last_used_ms: Date.now() - 86_400_000, last_action: "run" }, resolved_path: "/lib/reports/daily.sql", state: "available" },
          { entry: { path: "/tmp/gone.sql", last_used_ms: Date.now(), last_action: "open" }, resolved_path: "/tmp/gone.sql", state: "not_found" },
        ];
      case "script_read": {
        const found = scripts.find((s) => s.id === a.id);
        if (a.id === "s3")
          return { info: scripts.find((s) => s.id === "s3"), path: "/lib/runbooks/restore.md", file: { content: "# Restore", fingerprint: fp } };
        return {
          info: found ?? scripts[0],
          path: `/lib/${found?.path ?? "reports/daily.sql"}`,
          file: { content: "select now()", fingerprint: fp },
        };
      }
      case "pick_open_path":
        return "/Users/me/notes/todo.md";
      case "file_read":
        return { content: "- vacuum", fingerprint: fp };
      case "script_save": {
        const p = String(a.path);
        const parts = p.split("/");
        const folder = parts.length > 1 ? parts.slice(0, -1).join("/") : "";
        const base = parts[parts.length - 1];
        const name = base.endsWith(".sql") ? base.slice(0, -4) : p;
        const info = { id: "s9", path: p, name, folder, tags: [] as string[], fingerprint: fp, missing: false };
        scripts = [...scripts.filter((x) => x.path !== p), info];
        return { info, path: `/lib/${p}`, file: { content: a.content, fingerprint: fp } };
      }
      case "script_rename": {
        const nextPath = String(a.path);
        const parts = nextPath.split("/");
        const folder = parts.length > 1 ? parts.slice(0, -1).join("/") : "";
        const base = parts[parts.length - 1];
        const name = base.endsWith(".sql") ? base.slice(0, -4) : base;
        let updated = scripts[0];
        scripts = scripts.map((s) => {
          if (s.id !== a.id) return s;
          updated = { ...s, path: nextPath, folder, name };
          return updated;
        });
        return updated;
      }
      case "script_set_tags": {
        const tags = (a.tags as string[]) ?? [];
        let updated = scripts[0];
        scripts = scripts.map((s) => {
          if (s.id !== a.id) return s;
          updated = { ...s, tags };
          return updated;
        });
        return updated;
      }
      case "script_delete":
        scripts = scripts.filter((s) => s.id !== a.id);
        return null;
      case "meta_scan": {
        const text = String(a.text ?? "");
        return text.includes("\\c")
          ? [{ command: "\\c", line: 1, column: 1, offset: 0, text: "\\c prod", position: "line_start" }]
          : [];
      }
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
  useScriptLibrary.setState({ version: 0 });
  registerView("scripts", ScriptsView);
  registerTab("editor", EditorTab);
  registerTab("text", TextTab);
});

describe("scripts view", () => {
  it("reopens yesterday's script with the connection it last ran against", async () => {
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
    await user.click(within(lib).getByRole("button", { name: /^fix$/ }));
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
    await user.click(within(lib).getByRole("button", { name: /^restore\.md$/ }));
    expect(await screen.findByRole("toolbar", { name: "Text editor" })).toBeInTheDocument();
    expect(calls.some((c) => c.cmd === "editor_open")).toBe(false);
  });

  it("creates a new script in a folder and warns live on psql meta-commands", async () => {
    const user = userEvent.setup();
    const calls = backend();
    render(<App />);
    await user.click(await screen.findByRole("button", { name: "+ New" }));
    const dialog = await screen.findByRole("dialog", { name: "New script" });
    await user.type(within(dialog).getByLabelText("New script path"), "post/seed_users");
    const content = within(dialog).getByLabelText("New script content");
    await user.clear(content);
    await user.type(content, "\\c prod");
    expect(await within(dialog).findByRole("alert")).toHaveTextContent(/\\c \(line 1\)/);

    await user.clear(content);
    await user.type(content, "DELETE FROM public.users;");
    await user.click(within(dialog).getByRole("button", { name: "Create script" }));

    expect(calls.find((c) => c.cmd === "script_save")?.args).toEqual({
      path: "post/seed_users.sql",
      content: "DELETE FROM public.users;",
    });
    const lib = screen.getByRole("region", { name: "Library" });
    expect(await within(lib).findByText("seed_users", { selector: ".script-name" })).toBeInTheDocument();
  });

  it("renames, edits tags, filters by tag chip, and deletes a script with confirmation", async () => {
    const user = userEvent.setup();
    const calls = backend();
    render(<App />);
    const lib = await screen.findByRole("region", { name: "Library" });

    // Filter by existing tag chip
    await user.click(await screen.findByRole("button", { name: "#daily" }));
    expect(within(lib).getByText("daily", { selector: ".script-name" })).toBeInTheDocument();
    expect(within(lib).queryByText("fix", { selector: ".script-name" })).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "#daily" }));
    expect(await within(lib).findByText("fix", { selector: ".script-name" })).toBeInTheDocument();

    // Edit tags
    await user.click(within(lib).getByRole("button", { name: "Edit tags for fix" }));
    const tagDialog = await screen.findByRole("dialog", { name: "Edit script tags" });
    await user.type(within(tagDialog).getByLabelText("Script tags"), "qa, seed");
    await user.click(within(tagDialog).getByRole("button", { name: "Save tags" }));
    expect(calls.find((c) => c.cmd === "script_set_tags")?.args).toEqual({ id: "s2", tags: ["qa", "seed"] });

    // Rename / move into a folder
    await user.click(within(lib).getByRole("button", { name: "Rename fix" }));
    const renameDialog = await screen.findByRole("dialog", { name: "Rename script" });
    const pathInput = within(renameDialog).getByLabelText("New path");
    await user.clear(pathInput);
    await user.type(pathInput, "post/fix_users.sql");
    await user.click(within(renameDialog).getByRole("button", { name: "Rename" }));
    expect(calls.find((c) => c.cmd === "script_rename")?.args).toEqual({ id: "s2", path: "post/fix_users.sql" });
    expect(await within(lib).findByText("fix_users", { selector: ".script-name" })).toBeInTheDocument();

    // Delete with confirmation
    await user.click(within(lib).getByRole("button", { name: "Delete fix_users" }));
    const deleteDialog = await screen.findByRole("dialog", { name: "Delete script" });
    expect(deleteDialog).toHaveTextContent("post/fix_users.sql");
    await user.click(within(deleteDialog).getByRole("button", { name: "Confirm delete" }));
    expect(calls.find((c) => c.cmd === "script_delete")?.args).toEqual({ id: "s2" });
    expect(within(lib).queryByText("fix_users", { selector: ".script-name" })).not.toBeInTheDocument();
  });
});
