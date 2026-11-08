import { act, fireEvent, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { mockIPC } from "@tauri-apps/api/mocks";
import { beforeAll, beforeEach, describe, expect, it } from "vitest";
import { App } from "../../app/App";
import { registerTab } from "../../app/registry";
import { useWorkspace } from "../../app/workspace";
import { useConnections } from "../../connection/store";
import { encodeChunk } from "../protocol";
import { openTopN, TopNTab } from "./TopNTab";

const ws0 = useWorkspace.getState();
const conn0 = useConnections.getState();

const rows = [["1", "ada"], ["2", "grace"], ["3", "edsger"]];

function backend(opts: { editable?: boolean; commitError?: boolean } = {}) {
  const calls: { cmd: string; args: Record<string, unknown> }[] = [];
  let pending = 0;
  let subscribed: ((m: ArrayBuffer) => void) | null = null;
  const view = (extra = {}) => ({
    result_id: "r1",
    columns: [{ name: "id", type: "integer", key: true, writable: true }, { name: "name", type: "text", key: false, writable: true }],
    row_count: rows.length, sql: "SELECT id, name FROM public.people LIMIT 100", n: 100,
    editable: opts.editable ?? true, read_only_reason: opts.editable === false ? "no primary key or unique NOT NULL key" : null, ...extra,
  });
  mockIPC((cmd, args) => {
    const a = args as Record<string, unknown>;
    calls.push({ cmd, args: a });
    switch (cmd) {
      case "grid_open":
      case "grid_requery":
        return view();
      case "result_subscribe":
        subscribed = (m) => (a.channel as { onmessage: (m: ArrayBuffer) => void }).onmessage(m);
        return null;
      case "result_window":
        queueMicrotask(() => subscribed?.(encodeChunk(a.start as number, rows.slice(a.start as number, (a.start as number) + (a.count as number)), 2)));
        return null;
      case "grid_edit_cell":
      case "grid_delete_rows":
        pending++;
        return { pending, row_count: rows.length };
      case "grid_preview":
        return [
          "UPDATE public.people SET name = 'ADA' WHERE id = 1;",
          "UPDATE public.people SET name = 'GRACE' WHERE id = 2;",
          "DELETE FROM public.people WHERE id = 3;",
        ].slice(0, pending);
      case "grid_commit":
        if (opts.commitError) throw { kind: "sql", class: "retriable", message: "row id=2 was changed by someone else; nothing was committed", data: { rows: [["2"]] } };
        pending = 0;
        return { updated: 2, inserted: 0, deleted: 1 };
      case "grid_discard":
        pending = 0;
        return { pending: 0, row_count: rows.length };
      default:
        return null;
    }
  });
  return calls;
}

beforeAll(() => {
  Object.defineProperty(HTMLElement.prototype, "offsetHeight", { configurable: true, get: () => 600 });
  Object.defineProperty(HTMLElement.prototype, "offsetWidth", { configurable: true, get: () => 1000 });
  registerTab("grid", TopNTab);
});

beforeEach(() => {
  useWorkspace.setState(ws0, true);
  useConnections.setState({ ...conn0, loaded: true, entries: [{ id: "c1", name: "Local", host: "h", port: 5432, database: "app", user: "u", maintenance_db: "postgres", sslmode: "prefer", env_tag: "local", password_saved: true }] }, true);
});

async function openPeople() {
  render(<App />);
  act(() => openTopN({ connectionId: "c1", database: "app", schema: "public", name: "people" }));
  return screen.findByText("ada");
}

async function editCell(user: ReturnType<typeof userEvent.setup>, text: string, value: string) {
  await user.dblClick(await screen.findByText(text));
  const input = screen.getByLabelText("Cell value");
  await user.clear(input);
  await user.type(input, `${value}{Enter}`);
}

describe("top-N grid", () => {
  it("two edits + one delete are pending, previewable, and commit atomically", async () => {
    const user = userEvent.setup();
    const calls = backend();
    await openPeople();
    await editCell(user, "ada", "ADA");
    await editCell(user, "grace", "GRACE");
    fireEvent.mouseDown(await screen.findByText("edsger"));
    await user.click(screen.getByRole("button", { name: "Delete rows" }));
    expect(screen.getByRole("status", { name: "Pending changes" })).toHaveTextContent("3 pending changes");
    expect(calls.filter((c) => c.cmd === "grid_edit_cell").map((c) => [c.args.row, c.args.col, c.args.value])).toEqual([[0, 1, "ADA"], [1, 1, "GRACE"]]);

    await user.click(screen.getByRole("button", { name: "Preview SQL" }));
    const sql = screen.getByLabelText("Generated SQL");
    expect(sql.textContent!.split("\n")).toHaveLength(3);

    await user.click(screen.getByRole("button", { name: "Commit" }));
    expect(await screen.findByText("Committed: 2 updated, 0 inserted, 1 deleted")).toBeInTheDocument();
    expect(screen.queryByRole("status", { name: "Pending changes" })).not.toBeInTheDocument();
    expect(calls.filter((c) => c.cmd === "grid_commit")).toHaveLength(1);
  });

  it("a failed commit keeps the pending edits and names the row", async () => {
    const user = userEvent.setup();
    backend({ commitError: true });
    await openPeople();
    await editCell(user, "grace", "GRACE");
    await user.click(screen.getByRole("button", { name: "Commit" }));
    expect(await screen.findByText(/row id=2 was changed by someone else/)).toBeInTheDocument();
    expect(screen.getByRole("status", { name: "Pending changes" })).toHaveTextContent("1 pending change");
  });

  it("sorting re-queries the server; with pending edits it asks before discarding", async () => {
    const user = userEvent.setup();
    const calls = backend();
    await openPeople();
    await user.click(screen.getByRole("button", { name: "Sort by name" }));
    expect(calls.find((c) => c.cmd === "grid_requery")?.args.sort).toEqual([{ column: "name", dir: "asc" }]);

    await editCell(user, "ada", "ADA");
    await user.click(screen.getByRole("button", { name: "Sort by name" }));
    const ask = await screen.findByRole("alert");
    expect(ask).toHaveTextContent("discards 1 pending change");
    await user.click(within(ask).getByRole("button", { name: "Keep editing" }));
    expect(screen.getByRole("status", { name: "Pending changes" })).toBeInTheDocument();
  });

  it("filters are parameterized criteria, not SQL text", async () => {
    const user = userEvent.setup();
    const calls = backend();
    await openPeople();
    await user.click(screen.getByRole("button", { name: "Filter" }));
    await user.click(screen.getByRole("button", { name: "+ condition" }));
    await user.selectOptions(screen.getByLabelText("Filter column"), "name");
    await user.selectOptions(screen.getByLabelText("Filter operator"), "ILIKE");
    await user.type(screen.getByLabelText("Filter value"), "%o'brien%");
    await user.click(screen.getByRole("button", { name: "Apply" }));
    expect(calls.find((c) => c.cmd === "grid_requery")?.args.criteria).toEqual([{ column: "name", op: "ILIKE", value: "%o'brien%" }]);
  });

  it("tables without a usable key are read-only and say why", async () => {
    const user = userEvent.setup();
    const calls = backend({ editable: false });
    await openPeople();
    expect(screen.getByText(/read-only: no primary key/)).toBeInTheDocument();
    await user.dblClick(screen.getByText("ada"));
    expect(screen.queryByLabelText("Cell value")).not.toBeInTheDocument();
    expect(calls.some((c) => c.cmd === "grid_edit_cell")).toBe(false);
  });
});
