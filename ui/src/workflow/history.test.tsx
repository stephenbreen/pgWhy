// The merged history: Refresh runs and one-off operations in one list, and a backup-bearing row
// handing its archive to the restore dialog.

import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { mockIPC } from "@tauri-apps/api/mocks";
import { afterEach, expect, test } from "vitest";
import { useWorkspace } from "../app/workspace";
import { useConnections } from "../connection/store";
import { useRestore } from "../explorer/RestoreDialog";
import { initWorkflow } from "./index";
import { HistoryView } from "./HistoryView";
import type { HistoryEntry } from "../lib/ipc/runs";

const entries: HistoryEntry[] = [
  {
    id: "run-1",
    kind: "refresh",
    name: 'Refresh "Nightly" → app',
    started_at: "2026-10-22T10:00:00Z",
    finished_at: "2026-10-22T10:01:00Z",
    status: "failed_at_step",
    headline: "the restore failed; app is untouched.",
    target: "app on Local",
    workflow_id: "wf-1",
    workflow_name: "Nightly",
    backup: { path: "/backups/app_20261022.dump", connection: { id: "c1", name: "Local" }, database: "app" },
  },
  {
    id: "op-1",
    kind: "drop",
    name: "Drop database scratch",
    started_at: "2026-10-22T11:00:00Z",
    status: "succeeded",
    target: "scratch",
    backup: { path: "/backups/scratch_predrop.dump", connection: { id: "c1", name: "Local" }, database: "scratch" },
  },
  {
    id: "op-2",
    kind: "restore",
    name: "Restore app.dump into app_restore",
    started_at: "2026-10-22T12:00:00Z",
    status: "succeeded",
    target: "app_restore on Local",
  },
];

function backend() {
  const calls: string[] = [];
  mockIPC((cmd) => {
    calls.push(cmd);
    if (cmd === "history_list") return entries;
    return null;
  });
  return calls;
}

afterEach(() => {
  useRestore.setState({ connectionId: "", database: undefined, archive: undefined });
  useWorkspace.setState({ tabs: [{ id: "welcome", kind: "welcome", title: "Welcome" }], activeTabId: "welcome" });
});

test("lists runs and operations together, with their kinds", async () => {
  backend();
  initWorkflow();
  useConnections.setState({
    entries: [{ id: "c1", name: "Local", host: "h", port: 5432, database: "app", user: "me", maintenance_db: "postgres", sslmode: "prefer", env_tag: "local", password_saved: true }],
    recents: [],
    status: {},
  } as never);
  render(<HistoryView />);

  expect(await screen.findByText("Refresh")).toBeInTheDocument();
  expect(screen.getByText("Drop DB")).toBeInTheDocument();
  expect(screen.getByText("Restore")).toBeInTheDocument();
  expect(screen.getByText(/nightly/i)).toBeInTheDocument();
  expect(screen.getByText("failed at step")).toBeInTheDocument();
});

test("only rows that own a backup offer to restore it, and the archive is preselected", async () => {
  backend();
  initWorkflow();
  useConnections.setState({
    entries: [{ id: "c1", name: "Local", host: "h", port: 5432, database: "app", user: "me", maintenance_db: "postgres", sslmode: "prefer", env_tag: "local", password_saved: true }],
    recents: [],
    status: {},
  } as never);
  render(<HistoryView />);

  const buttons = await screen.findAllByRole("button", { name: /restore backup/i });
  expect(buttons).toHaveLength(2);

  await userEvent.click(buttons[1]);
  const s = useRestore.getState();
  expect(s.connectionId).toBe("c1");
  expect(s.database).toBe("scratch");
  expect(s.archive).toBe("/backups/scratch_predrop.dump");
  expect(s.envTag).toBe("local");
});

test("an operation row opens its own report tab", async () => {
  backend();
  initWorkflow();
  render(<HistoryView />);

  const open = await screen.findAllByRole("button", { name: /^open$/i });
  await userEvent.click(open[2]); // the restore operation
  const tabs = useWorkspace.getState().tabs;
  const tab = tabs.find((t) => t.kind === "operation");
  expect(tab?.id).toBe("operation:op-2");
});

test("an unreadable record is listed plainly, with nothing to open", async () => {
  mockIPC((cmd) => {
    if (cmd === "history_list")
      return [
        {
          id: "op-9",
          kind: "refresh",
          name: "Unreadable record op-9.json",
          started_at: "",
          status: "interrupted",
          headline: "op-9.json in the history folder could not be read (EOF while parsing).",
          unreadable: true,
        },
      ] satisfies HistoryEntry[];
    return null;
  });
  initWorkflow();
  render(<HistoryView />);

  expect(await screen.findByText("Unreadable")).toBeInTheDocument();
  expect(screen.getByText(/could not be read/)).toBeInTheDocument();
  expect(screen.queryByText("Invalid Date")).not.toBeInTheDocument();
  expect(screen.queryByRole("button", { name: /^open$/i })).not.toBeInTheDocument();
});
