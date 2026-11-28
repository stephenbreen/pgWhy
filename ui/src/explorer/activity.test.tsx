import { act, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { mockIPC } from "@tauri-apps/api/mocks";
import { beforeEach, describe, expect, it } from "vitest";
import { App } from "../app/App";
import { registerTab } from "../app/registry";
import { useWorkspace } from "../app/workspace";
import { useConnections } from "../connection/store";
import type { ConnectionEntry } from "../lib/ipc/connections";
import type { ActivitySnapshot } from "../lib/ipc/activity";
import { ActivityTab, openActivityTab } from "./ActivityTab";

const ws0 = useWorkspace.getState();
const conn0 = useConnections.getState();

const prodEntry: ConnectionEntry = {
  id: "c-prod",
  name: "Prod PG",
  host: "db.prod.local",
  port: 5432,
  database: "app",
  user: "admin",
  maintenance_db: "postgres",
  sslmode: "require",
  env_tag: "production",
  password_saved: true,
};

const sampleSnapshot = (): ActivitySnapshot => ({
  captured_at_ms: Date.now(),
  server_version: "16.4",
  total_count: 4,
  active_count: 2,
  idle_in_txn_count: 1,
  blocked_count: 1,
  sessions: [
    {
      pid: 101,
      database: "app",
      username: "migrator",
      application_name: "flyway",
      client_addr: "10.0.0.5",
      state: "idle in transaction",
      wait_event_type: "Client",
      wait_event: "ClientRead",
      backend_type: "client backend",
      query_duration_secs: 45.2,
      xact_duration_secs: 60.0,
      state_duration_secs: 45.0,
      query: "ALTER TABLE public.users ADD COLUMN bio text;",
      blocking_pids: [],
      is_self: false,
    },
    {
      pid: 202,
      database: "app",
      username: "web",
      application_name: "api",
      client_addr: "10.0.0.12",
      state: "active",
      wait_event_type: "Lock",
      wait_event: "relation",
      backend_type: "client backend",
      query_duration_secs: 18.4,
      xact_duration_secs: 18.4,
      state_duration_secs: 18.4,
      query: "SELECT * FROM public.users WHERE id = 42;",
      blocking_pids: [101],
      is_self: false,
    },
    {
      pid: 303,
      database: "app",
      username: "worker",
      application_name: "jobs",
      client_addr: "10.0.0.20",
      state: "idle",
      wait_event_type: "",
      wait_event: "",
      backend_type: "client backend",
      query_duration_secs: 1.0,
      xact_duration_secs: null,
      state_duration_secs: 120.0,
      query: "SELECT 1;",
      blocking_pids: [],
      is_self: false,
    },
    {
      pid: 999,
      database: "postgres",
      username: "admin",
      application_name: "pgWhy",
      client_addr: "127.0.0.1",
      state: "active",
      wait_event_type: "",
      wait_event: "",
      backend_type: "client backend",
      query_duration_secs: 0.01,
      xact_duration_secs: 0.01,
      state_duration_secs: 0.01,
      query: "SELECT * FROM pg_stat_activity;",
      blocking_pids: [],
      is_self: true,
    },
  ],
  locks: [
    {
      pid: 202,
      database: "app",
      lock_type: "relation",
      relation: "public.users",
      mode: "AccessShareLock",
      granted: false,
      fastpath: false,
      virtual_xid: "4/12",
    },
    {
      pid: 101,
      database: "app",
      lock_type: "relation",
      relation: "public.users",
      mode: "AccessExclusiveLock",
      granted: true,
      fastpath: false,
      virtual_xid: "3/9",
    },
  ],
});

function backend() {
  const calls: { cmd: string; args: Record<string, unknown> }[] = [];
  mockIPC((cmd, args) => {
    const a = args as Record<string, unknown>;
    calls.push({ cmd, args: a });
    switch (cmd) {
      case "activity_snapshot":
        return sampleSnapshot();
      case "activity_cancel_backend":
        return true;
      case "activity_terminate_backend":
        return true;
      default:
        return null;
    }
  });
  return calls;
}

beforeEach(() => {
  useWorkspace.setState(ws0, true);
  useConnections.setState({ ...conn0, entries: [prodEntry], loaded: true }, true);
  registerTab("activity", ActivityTab);
});

describe("activity & locks monitor", () => {
  it("renders sessions, KPI summary, filters, and opens selected query in an editor tab", async () => {
    const user = userEvent.setup();
    const calls = backend();
    render(<App />);
    act(() => openActivityTab("c-prod", "app"));

    const table = await screen.findByRole("table", { name: "Active sessions" });
    expect(calls.find((c) => c.cmd === "activity_snapshot")?.args).toEqual({
      connectionId: "c-prod",
      database: "app",
    });
    expect(table).toHaveTextContent("101");
    expect(table).toHaveTextContent("202");
    expect(table).toHaveTextContent("303");

    // Hide idle hides PID 303
    await user.click(screen.getByLabelText("Hide idle"));
    expect(table).not.toHaveTextContent("worker");
    expect(table).toHaveTextContent("migrator");

    // Clicking a row opens the bottom SQL inspector & can open in editor
    await user.click(within(table).getByText("migrator"));
    const inspector = await screen.findByRole("region", { name: "Selected session details" });
    expect(inspector).toHaveTextContent("ALTER TABLE public.users ADD COLUMN bio text;");
    await user.click(within(inspector).getByRole("button", { name: "Open SQL in editor" }));
    const editorTab = useWorkspace.getState().tabs.find((t) => t.kind === "editor");
    expect(editorTab?.title).toBe("PID 101");
    expect((editorTab?.data as { text?: string })?.text).toBe("ALTER TABLE public.users ADD COLUMN bio text;");
  });

  it("renders blocking tree with root blocker and supports Cancel and confirmed Terminate", async () => {
    const user = userEvent.setup();
    const calls = backend();
    render(<App />);
    act(() => openActivityTab("c-prod"));

    await screen.findByRole("table", { name: "Active sessions" });
    await user.click(screen.getByRole("tab", { name: /Blocking tree/ }));

    const tree = await screen.findByRole("list", { name: "Blocking tree" });
    expect(tree).toHaveTextContent("ROOT BLOCKER");
    expect(tree).toHaveTextContent("PID 101");
    expect(tree).toHaveTextContent("WAITING");
    expect(tree).toHaveTextContent("PID 202");

    // Cancel the blocked query (PID 202)
    const cancelButtons = within(tree).getAllByRole("button", { name: "Cancel query" });
    await user.click(cancelButtons[1]);
    expect(calls.find((c) => c.cmd === "activity_cancel_backend")?.args).toEqual({
      connectionId: "c-prod",
      pid: 202,
    });

    // Terminate the root blocker (PID 101) requires confirmation and warns on Production
    const termButtons = within(tree).getAllByRole("button", { name: "Terminate" });
    await user.click(termButtons[0]);
    const dialog = await screen.findByRole("dialog", { name: "Confirm terminate backend" });
    expect(dialog).toHaveTextContent("Production server:");
    await user.click(within(dialog).getByRole("button", { name: "Confirm terminate" }));
    expect(calls.find((c) => c.cmd === "activity_terminate_backend")?.args).toEqual({
      connectionId: "c-prod",
      pid: 101,
    });
  });

  it("renders locks table and filters to waiting locks only", async () => {
    const user = userEvent.setup();
    backend();
    render(<App />);
    act(() => openActivityTab("c-prod", "app"));

    await screen.findByRole("table", { name: "Active sessions" });
    await user.click(screen.getByRole("tab", { name: /Locks/ }));

    const locksTable = await screen.findByRole("table", { name: "Database locks" });
    expect(locksTable).toHaveTextContent("AccessShareLock");
    expect(locksTable).toHaveTextContent("AccessExclusiveLock");

    await user.click(screen.getByLabelText("Waiting only"));
    expect(locksTable).toHaveTextContent("AccessShareLock");
    expect(locksTable).not.toHaveTextContent("AccessExclusiveLock");
  });
});
