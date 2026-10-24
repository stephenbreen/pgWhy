import { act, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { mockIPC } from "@tauri-apps/api/mocks";
import { beforeEach, describe, expect, it } from "vitest";
import { App } from "../app/App";
import { useWorkspace } from "../app/workspace";
import { useConnections } from "../connection/store";
import type { BackupEvent } from "../lib/ipc/backup";
import { openBackup } from "./BackupTab";
import { initWorkflow } from "./index";

const ws0 = useWorkspace.getState();
const conn0 = useConnections.getState();

beforeEach(() => {
  useWorkspace.setState(ws0, true);
  useConnections.setState({ ...conn0, loaded: true, entries: [{ id: "c1", name: "Remote", host: "h", port: 5432, database: "app", user: "u", maintenance_db: "postgres", sslmode: "prefer", env_tag: "production", password_saved: true }] }, true);
  initWorkflow();
});

describe("standalone backup", () => {
  it("shows live progress and the retained file", async () => {
    let emit: ((e: BackupEvent) => void) | null = null;
    let resolve: (() => void) | null = null;
    const calls: string[] = [];
    mockIPC((cmd, args) => {
      calls.push(cmd);
      if (cmd === "backup_database") {
        emit = (e) => (args as { events: { onmessage: (e: BackupEvent) => void } }).events.onmessage(e);
        return new Promise<void>((r) => (resolve = r));
      }
      return null;
    });
    render(<App />);
    act(() => openBackup("c1", "app"));
    await screen.findByText(/Backup of Remote\/app/);
    // options first: format and contents default to the whole database, custom format
    expect(screen.getByLabelText(/backup format/i)).toHaveValue("custom");
    expect(screen.getByLabelText(/backup contents/i)).toHaveValue("all");
    await userEvent.click(screen.getByRole("button", { name: /^back up$/i }));
    act(() => {
      emit!({ type: "started", path: "/b/app_1.dump", pg_dump: "pg_dump 16.15", server_version: "16.4" });
      emit!({ type: "progress", progress: { tool: "pg_dump", phase: "dumping", current_object: "public.orders", objects_done: 12, objects_total: 40, total_is_estimate: true, bytes_written: 5 * 1024 * 1024, elapsed_ms: 65_000, warnings: 0, errors: 0 } });
    });
    expect(screen.getByText("public.orders")).toBeInTheDocument();
    expect(screen.getByText("~12 of ~40 objects")).toBeInTheDocument();
    expect(screen.getByText("5.0 MB written")).toBeInTheDocument();
    expect(screen.getByText("1m 5s")).toBeInTheDocument();
    expect(screen.queryByText(/%/)).not.toBeInTheDocument();

    const user = userEvent.setup();
    await user.click(screen.getByRole("button", { name: "Cancel" }));
    expect(calls).toContain("backup_cancel");

    await act(async () => {
      emit!({ type: "finished", path: "/b/app_1.dump", size_bytes: 9 * 1024 * 1024, duration_ms: 70_000, removed_by_retention: ["/b/app_0.dump"] });
      resolve!();
    });
    expect(screen.getByRole("status")).toHaveTextContent("Backed up 9.0 MB in 1m 10s");
    expect(screen.getByText(/Removed 1 older backup/)).toBeInTheDocument();
  });
});

describe("backup options", () => {
  it("sends the format, contents and the node's scope", async () => {
    let sent: Record<string, unknown> | null = null;
    let finish: (() => void) | null = null;
    mockIPC((cmd, args) => {
      if (cmd === "backup_database") {
        sent = args as Record<string, unknown>;
        return new Promise<void>((r) => (finish = r));
      }
      return null;
    });
    render(<App />);
    act(() =>
      openBackup("c1", "app", { kind: "tables", tables: [{ schema: "public", name: "orders" }] }),
    );
    await screen.findByText(/Backing up/);
    expect(screen.getAllByText(/public\.orders/).length).toBeGreaterThan(0);

    const user = userEvent.setup();
    await user.selectOptions(screen.getByLabelText(/backup format/i), "directory");
    await user.type(screen.getByLabelText(/parallel jobs/i), "4");
    await user.selectOptions(screen.getByLabelText(/backup contents/i), "schema_only");
    await user.click(screen.getByRole("button", { name: /^back up$/i }));

    expect(sent).toMatchObject({
      connectionId: "c1",
      database: "app",
      request: {
        format: "directory",
        jobs: 4,
        data: "schema_only",
        scope: { kind: "tables", tables: [{ schema: "public", name: "orders" }] },
      },
    });
    await act(async () => finish!());
  });
});
