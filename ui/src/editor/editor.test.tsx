import { act, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { mockIPC } from "@tauri-apps/api/mocks";
import { beforeAll, beforeEach, describe, expect, it } from "vitest";
import { App } from "../app/App";
import { registerTab } from "../app/registry";
import { useWorkspace } from "../app/workspace";
import { useConnections } from "../connection/store";
import type { ConnectionEntry } from "../lib/ipc/connections";
import type { QueryEvent } from "../lib/ipc/query";
import { EditorTab, openEditorTab } from "./EditorTab";
import { useEditors } from "./store";

const ws0 = useWorkspace.getState();
const conn0 = useConnections.getState();

const entry: ConnectionEntry = {
  id: "c1", name: "Local dev", host: "localhost", port: 5432, database: "app", user: "me",
  maintenance_db: "postgres", sslmode: "prefer", env_tag: "local", password_saved: true,
};

type Emit = (ev: QueryEvent) => void;

const finished = (o: Partial<Extract<QueryEvent, { type: "finished" }>> = {}): QueryEvent => ({
  type: "finished", statements: 1, succeeded: 1, errors: 0, skipped: 0, cancelled: false,
  connection_lost: false, transaction_lost: false, txn: "idle", duration_ms: 4, ...o,
});

function backend(onRun?: (emit: Emit, args: Record<string, unknown>) => void) {
  const calls: { cmd: string; args: Record<string, unknown> }[] = [];
  mockIPC((cmd, args) => {
    const a = args as Record<string, unknown>;
    calls.push({ cmd, args: a });
    switch (cmd) {
      case "editor_open":
        return { handle: 7, connection_id: "c1", database: "app", purpose: "editor_tab", server_version_num: 160000, backend_pid: 9, tls: "plaintext" };
      case "editor_run": {
        const ch = a.events as { onmessage: Emit };
        onRun?.((ev) => ch.onmessage(ev), a);
        return null;
      }
      default:
        return null;
    }
  });
  return calls;
}

beforeAll(() => {
  // CodeMirror measures text; jsdom has no layout
  Range.prototype.getClientRects = () => [] as unknown as DOMRectList;
  Range.prototype.getBoundingClientRect = () => new DOMRect();
});

beforeEach(() => {
  useWorkspace.setState(ws0, true);
  useConnections.setState({ ...conn0, entries: [entry], loaded: true }, true);
  useEditors.setState({ tabs: {} });
  registerTab("editor", EditorTab);
});

describe("editor tab", () => {
  it("is bound to its connection from creation and shows the env tag", async () => {
    const calls = backend();
    render(<App />);
    act(() => void openEditorTab("c1", "app", { text: "select 1" }));
    const toolbar = await screen.findByRole("toolbar", { name: "Editor" });
    expect(within(toolbar).getByLabelText("Run against connection")).toHaveDisplayValue("Local dev");
    expect(within(toolbar).getByLabelText("Database")).toHaveValue("app");
    expect(within(toolbar).getByText("Local")).toHaveClass("tag-pill");
    expect(screen.getByRole("tab", { name: /Query/ })).toHaveAttribute("data-tag", "local");
    expect(calls.find((c) => c.cmd === "editor_open")?.args).toMatchObject({ connectionId: "c1", database: "app" });
  });

  it("switching the connection rebinds the tab and reopens on the new target", async () => {
    const user = userEvent.setup();
    useConnections.setState({ entries: [entry, { ...entry, id: "c2", name: "Staging", env_tag: "staging", database: "stage" }] });
    const calls = backend();
    render(<App />);
    act(() => void openEditorTab("c1", "app", { text: "select 1" }));
    const toolbar = await screen.findByRole("toolbar", { name: "Editor" });
    await user.selectOptions(within(toolbar).getByLabelText("Run against connection"), "Staging");
    const opens = calls.filter((c) => c.cmd === "editor_open");
    expect(opens.at(-1)?.args).toMatchObject({ connectionId: "c2", database: "stage" });
    expect(screen.getByRole("tab", { name: /Query/ })).toHaveAttribute("data-tag", "staging");
  });

  it("saves an untitled buffer into the library under a chosen name", async () => {
    const user = userEvent.setup();
    const calls = backend();
    mockIPC((cmd, args) => {
      calls.push({ cmd, args: args as Record<string, unknown> });
      if (cmd === "script_save")
        return { info: { id: "s1", path: "daily.sql", name: "daily", folder: "", tags: [], fingerprint: { sha256: "x", size: 1, modified_ms: 1 }, missing: false }, path: "/lib/daily.sql", file: { content: "select 1", fingerprint: { sha256: "x", size: 1, modified_ms: 1 } } };
      if (cmd === "editor_open") return { handle: 1, connection_id: "c1", database: "app", purpose: "editor_tab", server_version_num: 160000, backend_pid: 1, tls: "plaintext" };
      return null;
    });
    render(<App />);
    act(() => void openEditorTab("c1", "app", { text: "select 1" }));
    await user.click(await screen.findByRole("button", { name: /^Save/ }));
    const input = await screen.findByLabelText("Script path");
    await user.clear(input);
    await user.type(input, "daily{Enter}");
    expect(calls.find((c) => c.cmd === "script_save")?.args).toMatchObject({ path: "daily", content: "select 1" });
    expect(await screen.findByRole("tab", { name: /daily/ })).toBeInTheDocument();
  });

  it("runs the buffer and shows results, messages and notices", async () => {
    const user = userEvent.setup();
    const calls = backend((emit) => {
      emit({ type: "statement_started", index: 0, from: 0, to: 8 });
      emit({ type: "notice", index: 0, notice: { severity: "NOTICE", code: "00000", message: "hello from plpgsql" } });
      emit({ type: "statement_done", index: 0, command: "SELECT", rows_affected: 1, duration_ms: 3, truncated: false });
      emit(finished());
    });
    render(<App />);
    act(() => void openEditorTab("c1", "app", { text: "select 'é'; " }));
    await user.click(await screen.findByRole("button", { name: /Run/ }));
    const run = calls.find((c) => c.cmd === "editor_run")!;
    expect(run.args).toMatchObject({ sql: "select 'é'; ", from: 0, to: 13, policy: "stop" }); // byte range
    const messages = await screen.findByRole("list", { name: "Messages" });
    expect(messages).toHaveTextContent("SELECT — 1 row (3 ms)");
    expect(messages).toHaveTextContent("NOTICE: hello from plpgsql");
    expect(screen.getByText(/1 statement, 0 errors/)).toBeInTheDocument();
  });

  it("tracks the transaction and asks before closing a tab with one open", async () => {
    const user = userEvent.setup();
    const calls = backend((emit) => {
      emit({ type: "txn", status: "in_transaction" });
      emit(finished({ txn: "in_transaction" }));
    });
    render(<App />);
    act(() => void openEditorTab("c1", "app", { text: "begin" }));
    await user.click(await screen.findByRole("button", { name: /Run/ }));
    expect(await screen.findByRole("status", { name: "Transaction state" })).toHaveTextContent("in transaction");

    await user.click(screen.getByRole("button", { name: /Close Query/ }));
    const ask = await screen.findByRole("alert");
    expect(ask).toHaveTextContent("open transaction");
    expect(screen.getByRole("tab", { name: /Query/ })).toBeInTheDocument(); // not closed yet
    await user.click(within(ask).getByRole("button", { name: "Roll back" }));
    expect(calls.find((c) => c.cmd === "editor_close")?.args).toMatchObject({ txn: "rollback" });
    expect(screen.queryByRole("tab", { name: /Query/ })).not.toBeInTheDocument();
  });

  it("keep-open leaves the tab and its transaction alone", async () => {
    const user = userEvent.setup();
    const calls = backend((emit) => {
      emit({ type: "txn", status: "in_transaction" });
      emit(finished({ txn: "in_transaction" }));
    });
    render(<App />);
    act(() => void openEditorTab("c1", "app", { text: "begin" }));
    await user.click(await screen.findByRole("button", { name: /Run/ }));
    await screen.findByText("in transaction");
    await user.click(screen.getByRole("button", { name: /Close Query/ }));
    await user.click(await screen.findByRole("button", { name: "Keep open" }));
    expect(screen.getByRole("tab", { name: /Query/ })).toBeInTheDocument();
    expect(calls.some((c) => c.cmd === "editor_close")).toBe(false);
  });

  it("errors land in messages and connection loss offers reconnect with the text kept", async () => {
    const user = userEvent.setup();
    backend((emit) => {
      emit({ type: "error", index: 0, error: { kind: "sql", class: "fix_then_retry", message: 'syntax error at or near "frm"', sqlstate: "42601", position: 9 } });
      emit(finished({ errors: 1, succeeded: 0, connection_lost: true }));
    });
    render(<App />);
    act(() => void openEditorTab("c1", "app", { text: "select 1 frm t" }));
    await user.click(await screen.findByRole("button", { name: /Run/ }));
    expect(await screen.findByRole("list", { name: "Messages" })).toHaveTextContent('syntax error at or near "frm"');
    expect(screen.getByText(/SQLSTATE 42601/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Reconnect" })).toBeInTheDocument();
    expect(document.querySelector(".cm-content")?.textContent).toContain("select 1 frm t");
  });
});
