// Database lifecycle dialogs: the typed-name gate, the tag gate, and the report the operation
// record turns into. The core enforces both gates too — this proves the UI does not let a user
// click through them.

import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { mockIPC } from "@tauri-apps/api/mocks";
import { afterEach, expect, test } from "vitest";
import { useConnections } from "../connection/store";
import type { ConnectionEntry } from "../lib/ipc/connections";
import { DatabaseOps, useDbOps } from "./DatabaseOps";
import { useTrees } from "./tree/model";

const entry: ConnectionEntry = {
  id: "c1", name: "Local", host: "localhost", port: 5432, database: "app", user: "me",
  maintenance_db: "postgres", sslmode: "prefer", env_tag: "local", password_saved: true,
};

function backend(record: unknown) {
  const calls: { cmd: string; args: Record<string, unknown> }[] = [];
  mockIPC((cmd, args) => {
    const a = args as Record<string, unknown>;
    calls.push({ cmd, args: a });
    switch (cmd) {
      case "db_capabilities":
        return { can_create_database: true, is_superuser: false, server_version: "16.4", current_user: "me" };
      case "explorer_databases":
        return [{ name: "app", is_template: false, allow_connections: true }];
      case "db_create":
      case "db_rename":
      case "db_drop":
        return record;
      default:
        return null;
    }
  });
  return calls;
}

const record = {
  id: "0123456789abcdef",
  kind: "drop",
  name: "Drop database app",
  started_at: "2026-10-22T10:00:00Z",
  status: "succeeded",
  steps: [
    { id: "check", label: "Check for prepared transactions", status: "succeeded" },
    { id: "drop", label: "Terminate sessions on app and drop it", status: "succeeded" },
  ],
  warnings: [],
};

function openDrop(envTag: ConnectionEntry["env_tag"]) {
  useConnections.setState({
    entries: [{ ...entry, env_tag: envTag }],
    recents: [],
    status: {},
  } as never);
  useTrees.setState({ trees: {} } as never);
  useDbOps.setState({ mode: "drop", connectionId: "c1", database: "app", envTag });
}

afterEach(() => useDbOps.setState({ connectionId: "", database: undefined }));

test("drop needs the exact database name before it can be submitted", async () => {
  const calls = backend(record);
  openDrop("local");
  render(<DatabaseOps />);

  const submit = await screen.findByRole("button", { name: /drop database/i });
  expect(submit).toBeDisabled();

  const typed = screen.getByLabelText(/type the database name/i);
  await userEvent.type(typed, "ap");
  expect(submit).toBeDisabled();
  await userEvent.type(typed, "p");
  expect(submit).toBeEnabled();

  await userEvent.click(submit);
  await waitFor(() => expect(screen.getByText(/done/i)).toBeInTheDocument());
  const drop = calls.find((c) => c.cmd === "db_drop");
  expect(drop?.args).toMatchObject({ connectionId: "c1", database: "app", typedName: "app", backupFirst: true });
  expect(screen.getByText(/check for prepared transactions/i)).toBeInTheDocument();
});

test("a production-tagged connection cannot be dropped from the UI", async () => {
  const calls = backend(record);
  openDrop("production");
  render(<DatabaseOps />);

  expect(await screen.findByText(/destructive actions only run against local or dev/i)).toBeInTheDocument();
  expect(screen.getByRole("button", { name: /drop database/i })).toBeDisabled();
  expect(calls.some((c) => c.cmd === "db_drop")).toBe(false);
});
