// Restore dialog gates: an archive with blockers cannot be started, a production-tagged target is
// refused, and an in-place restore needs the database name typed. The core enforces all three as
// well; this proves the dialog does not let a user click through them.

import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { mockIPC } from "@tauri-apps/api/mocks";
import { afterEach, expect, test } from "vitest";
import { useConnections } from "../connection/store";
import type { ConnectionEntry } from "../lib/ipc/connections";
import { RestoreDialog, useRestore } from "./RestoreDialog";
import { useTrees } from "./tree/model";

const entry: ConnectionEntry = {
  id: "c1", name: "Local", host: "localhost", port: 5432, database: "app", user: "me",
  maintenance_db: "postgres", sslmode: "prefer", env_tag: "local", password_saved: true,
};

function backend(opts: { blockers?: string[]; canCreate?: boolean } = {}) {
  const calls: { cmd: string; args: Record<string, unknown> }[] = [];
  mockIPC((cmd, args) => {
    const a = args as Record<string, unknown>;
    calls.push({ cmd, args: a });
    switch (cmd) {
      case "restore_targets":
        return { databases: ["app", "postgres"], can_create_database: opts.canCreate ?? true };
      case "pick_restore_archive":
        return "/tmp/src.dump";
      case "restore_inspect":
        return {
          archive: { path: "/tmp/src.dump", size_bytes: 4096, reusable: !opts.blockers?.length },
          tool: "/usr/local/bin/pg_restore 16.15",
          dbname: "src_db",
          dumped_from: "16.11",
          dumped_by: "16.15",
          toc_entries: 12,
          blockers: opts.blockers ?? [],
        };
      case "explorer_databases":
        return [{ name: "app", is_template: false, allow_connections: true }];
      default:
        return null;
    }
  });
  return calls;
}

function open(envTag: ConnectionEntry["env_tag"], database = "app") {
  useConnections.setState({ entries: [{ ...entry, env_tag: envTag }], recents: [], status: {} } as never);
  useTrees.setState({ trees: {} } as never);
  useRestore.setState({ connectionId: "c1", database, envTag });
}

afterEach(() => useRestore.setState({ connectionId: "", database: undefined }));

test("an archive with a blocker cannot be restored", async () => {
  backend({ blockers: ["this is a plain-SQL dump"] });
  open("local");
  render(<RestoreDialog />);

  const start = await screen.findByRole("button", { name: /^restore$/i });
  expect(start).toBeDisabled();

  await userEvent.click(await screen.findByRole("button", { name: /choose/i }));
  expect(await screen.findByText(/plain-SQL dump/)).toBeInTheDocument();
  expect(screen.getByRole("button", { name: /^restore$/i })).toBeDisabled();
});

test("a readable archive enables Start, and a production tag blocks it again", async () => {
  const calls = backend();
  open("local");
  render(<RestoreDialog />);
  await userEvent.click(await screen.findByRole("button", { name: /choose/i }));
  // the header is shown and Start becomes available
  const start = await screen.findByRole("button", { name: /^restore$/i });
  expect(await screen.findByText(/dumped by pg_dump 16\.15/)).toBeInTheDocument();
  expect(start).toBeEnabled();

  useRestore.setState({ envTag: "production" });
  expect(await screen.findByText(/restores only run against local or dev/i)).toBeInTheDocument();
  expect(screen.getByRole("button", { name: /^restore$/i })).toBeDisabled();
  expect(calls.some((c) => c.cmd === "restore_start")).toBe(false);
});

test("an in-place restore needs the database name typed", async () => {
  backend();
  open("local", "app");
  render(<RestoreDialog />);
  await userEvent.click(await screen.findByRole("button", { name: /choose/i }));
  await userEvent.click(await screen.findByRole("radio", { name: /restore on top/i }));

  const start = screen.getByRole("button", { name: /^restore$/i });
  expect(start).toBeDisabled();
  await userEvent.type(screen.getByLabelText(/type the database name/i), "app");
  expect(start).toBeEnabled();
});

test("replace mode is offered when the role can create databases, and asks for the name", async () => {
  backend();
  open("local", "app");
  render(<RestoreDialog />);
  await userEvent.click(await screen.findByRole("button", { name: /choose/i }));

  const replace = await screen.findByRole("radio", { name: /replace database/i });
  expect(replace).toBeEnabled();
  await userEvent.click(replace);
  const start = screen.getByRole("button", { name: /^restore$/i });
  expect(start).toBeDisabled();
  await userEvent.type(screen.getByLabelText(/type the database name/i), "app");
  expect(start).toBeEnabled();
});

test("replace mode is disabled for a role that cannot create databases", async () => {
  backend({ canCreate: false });
  open("local", "app");
  render(<RestoreDialog />);
  await userEvent.click(await screen.findByRole("button", { name: /choose/i }));

  expect(await screen.findByRole("radio", { name: /replace database/i })).toBeDisabled();
  // on-top stays available: it needs no CREATE DATABASE
  expect(screen.getByRole("radio", { name: /restore on top/i })).toBeEnabled();
});
