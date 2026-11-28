// The operation report offers recovery for a restore whose swap stopped part-way.

import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { mockIPC } from "@tauri-apps/api/mocks";
import { expect, test } from "vitest";
import { OperationView } from "./OperationView";

const record = (status: string, warnings: string[]) => ({
  id: "op-1",
  kind: "restore",
  name: "Restore app.dump over shop",
  started_at: "2026-10-22T12:00:00Z",
  finished_at: "2026-10-22T12:01:00Z",
  status,
  steps: [],
  warnings,
  detail: { kind: "restore", target: { connection_id: "c1" } },
});

test("an interrupted restore offers to finish or undo its swap, and performs the choice", async () => {
  const calls: { cmd: string; args: unknown }[] = [];
  let done = false;
  mockIPC((cmd, args) => {
    calls.push({ cmd, args });
    if (cmd === "operation_get") return record("interrupted", ["the swap stopped part-way"]);
    if (cmd === "operation_actions")
      return done
        ? []
        : [
            { kind: "complete_swap", label: "Complete the swap", enabled: true },
            { kind: "rollback_swap", label: "Roll back", enabled: false, disabled_reason: "shop_old is missing" },
          ];
    if (cmd === "operation_action") {
      done = true;
      return record("succeeded", []);
    }
    return null;
  });
  render(<OperationView tab={{ id: "operation:op-1", kind: "operation", title: "Restore", data: { id: "op-1" } }} />);

  const complete = await screen.findByRole("button", { name: "Complete the swap" });
  expect(screen.getByRole("button", { name: "Roll back" })).toBeDisabled();
  await userEvent.click(complete);
  await waitFor(() => expect(screen.queryByRole("button", { name: "Complete the swap" })).not.toBeInTheDocument());
  expect(calls.find((c) => c.cmd === "operation_action")?.args).toEqual({ id: "op-1", action: "complete_swap" });
  expect(screen.getByText("succeeded")).toBeInTheDocument();
});

test("a create or drop report asks for no recovery actions", async () => {
  const calls: string[] = [];
  mockIPC((cmd) => {
    calls.push(cmd);
    if (cmd === "operation_get") return { ...record("succeeded", []), kind: "drop", detail: { kind: "drop" } };
    return null;
  });
  render(<OperationView tab={{ id: "operation:op-1", kind: "operation", title: "Drop", data: { id: "op-1" } }} />);
  await screen.findByText("succeeded");
  expect(calls).not.toContain("operation_actions");
});
