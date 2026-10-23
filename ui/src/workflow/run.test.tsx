import { act, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { mockIPC } from "@tauri-apps/api/mocks";
import { emit } from "@tauri-apps/api/event";
import { beforeEach, describe, expect, it } from "vitest";
import { App } from "../app/App";
import { useWorkspace } from "../app/workspace";
import type { PreflightReport, RunRecord, RunEvent } from "../lib/ipc/runs";
import { initWorkflow } from "./index";
import { openRunTab } from "./RunTab";

const ws0 = useWorkspace.getState();

const conf = (o = {}) => ({
  target_db: "shop", target_server_label: "localhost:5432", target_tag: "local", strategy: "staging_swap", create_mode: false,
  source_label: "Shop production (db.shop.io:5432)", source_db: "shop", staging_db: "shop_staging_20261009_101500",
  warnings: ["1 editor tab(s) have an open transaction on shop; the swap rolls it back"], remembered: false, can_remember: false, fingerprint: "fp", ...o,
});

const okChecks = [
  { id: "tools", label: "Client tools", status: "ok", message: "pg_dump 16.15" },
  { id: "disk_space", label: "Disk space", status: "warn", message: "could not check free space on the target", hint: "the target runs in Docker" },
];

const record = (o: Partial<RunRecord> = {}): RunRecord => ({
  run_id: "run1", workflow_id: "w1", workflow_name: "Refresh shop", started_at: "2026-10-09T09:00:00Z", status: "running",
  strategy: "staging_swap", create_mode: false,
  source: { connection_id: "c-prod", connection_name: "Shop production", host: "db", port: 5432, database: "shop", env_tag: "production" },
  target: { connection_id: "c-local", connection_name: "Local", host: "localhost", port: 5432, database: "shop", env_tag: "local" },
  steps: [
    { step: "dump", label: "Back up source", status: "running", output_tail: [] },
    { step: "restore", label: "Restore into staging", status: "pending", output_tail: [] },
    { step: "swap", label: "Swap", status: "pending", output_tail: [] },
  ],
  renames_completed: 0, failed_items: [], actions: [], warnings: [], ...o,
});

function backend(report: PreflightReport, opts: { secretFirst?: boolean } = {}) {
  const calls: { cmd: string; args: Record<string, unknown> }[] = [];
  let needSecret = !!opts.secretFirst;
  let current = record();
  mockIPC(
    (cmd, args) => {
      const a = args as Record<string, unknown>;
      calls.push({ cmd, args: a });
      switch (cmd) {
        case "workflow_preflight":
          return needSecret
            ? { ...report, missing_secrets: [{ connection_id: "c-prod", connection_name: "Shop production", kind: "ssh_key_passphrase", reason: "missing" }] }
            : report;
        case "workflow_provide_secret":
          needSecret = false;
          return null;
        case "workflow_start":
          return "run1";
        case "run_get":
          return current;
        case "run_action":
          current = { ...current, status: "succeeded", headline: "Rolled back", actions: [] };
          return current;
        default:
          return null;
      }
    },
    { shouldMockEvents: true },
  );
  return { calls, set: (r: RunRecord) => (current = r) };
}

async function event(ev: RunEvent) {
  await act(async () => {
    await emit("workflow-event", ev);
  });
}

beforeEach(() => {
  useWorkspace.setState(ws0, true);
  initWorkflow();
});

describe("refresh run view", () => {
  it("asks one confirmation naming the exact target, server and strategy, with warnings", async () => {
    const user = userEvent.setup();
    const { calls } = backend({ ticket: "t1", checks: okChecks as PreflightReport["checks"], confirmation: conf() as PreflightReport["confirmation"], missing_secrets: [] });
    render(<App />);
    act(() => openRunTab({ workflowId: "w1", workflowName: "Refresh shop" }));
    const box = await screen.findByRole("region", { name: "Confirmation" });
    expect(box).toHaveTextContent("Replace shop on localhost:5432");
    expect(within(box).getByText("Local")).toHaveClass("tag-pill");
    expect(box).toHaveTextContent("restores into shop_staging_20261009_101500 first");
    expect(box).toHaveTextContent("open transaction on shop; the swap rolls it back");
    expect(screen.getByRole("region", { name: "Checks" })).toHaveTextContent("could not check free space");
    await user.click(within(box).getByRole("button", { name: "Replace shop" }));
    expect(calls.find((c) => c.cmd === "workflow_start")?.args).toEqual({ ticket: "t1", fingerprint: "fp", remember: false });
  });

  it("drop-first says a failure leaves no local database; failing checks block the run", async () => {
    backend({
      ticket: "t1",
      checks: [{ id: "prepared_xacts", label: "Prepared transactions", status: "fail", message: "shop has prepared transaction 'tx1'", fixes: [{ kind: "rollback_prepared", database: "shop", gid: "tx1" }] }],
      confirmation: conf({ strategy: "drop_first" }) as PreflightReport["confirmation"],
      missing_secrets: [],
    });
    render(<App />);
    act(() => openRunTab({ workflowId: "w1", workflowName: "Refresh shop" }));
    const box = await screen.findByRole("region", { name: "Confirmation" });
    expect(box).toHaveTextContent("A failed run will leave no local database");
    expect(within(box).getByRole("button", { name: "Replace shop" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Roll back prepared transaction tx1" })).toBeInTheDocument();
  });

  it("prompts for a missing SSH passphrase before the confirmation can run (EC-10)", async () => {
    const user = userEvent.setup();
    const { calls } = backend({ ticket: "t1", checks: [], confirmation: conf() as PreflightReport["confirmation"], missing_secrets: [] }, { secretFirst: true });
    render(<App />);
    act(() => openRunTab({ workflowId: "w1", workflowName: "Refresh shop" }));
    const input = await screen.findByLabelText("SSH key passphrase for Shop production");
    expect(screen.getByRole("button", { name: "Replace shop" })).toBeDisabled();
    await user.type(input, "secret");
    await user.click(screen.getByRole("button", { name: "Use" }));
    expect(calls.find((c) => c.cmd === "workflow_provide_secret")?.args).toEqual({ connectionId: "c-prod", kind: "ssh_key_passphrase", value: "secret", persist: true });
    expect(await screen.findByRole("button", { name: "Replace shop" })).toBeEnabled();
  });

  it("a remembered confirmation runs without asking", async () => {
    const { calls } = backend({ ticket: "t1", checks: [], confirmation: conf({ remembered: true }) as PreflightReport["confirmation"], missing_secrets: [] });
    render(<App />);
    act(() => openRunTab({ workflowId: "w1", workflowName: "Refresh shop" }));
    await screen.findByRole("list", { name: "Steps" });
    expect(calls.some((c) => c.cmd === "workflow_start")).toBe(true);
    expect(screen.queryByRole("region", { name: "Confirmation" })).not.toBeInTheDocument();
  });

  it("shows live progress, then the engine's state-aware actions (complete/rollback only mid-swap)", async () => {
    const user = userEvent.setup();
    const b = backend({ ticket: "t1", checks: [], confirmation: conf() as PreflightReport["confirmation"], missing_secrets: [] });
    render(<App />);
    act(() => openRunTab({ workflowId: "w1", workflowName: "Refresh shop" }));
    await user.click(await screen.findByRole("button", { name: "Replace shop" }));
    await screen.findByRole("list", { name: "Steps" });
    await event({ event: "progress", run_id: "run1", step: "dump", progress: { current_object: "public.orders", objects_done: 3, objects_total: 12, total_is_estimate: true, bytes_written: 2 * 1048576, elapsed_ms: 4000 } });
    expect(screen.getByText("public.orders")).toBeInTheDocument();
    expect(screen.getByText("~3 of ~12 objects")).toBeInTheDocument();

    const interrupted = record({
      status: "interrupted", renames_completed: 1, headline: "shop exists only as shop_old — the target name is free",
      steps: [{ step: "dump", label: "Back up source", status: "succeeded", output_tail: [] }, { step: "swap", label: "Swap", status: "interrupted", output_tail: [] }],
      actions: [
        { kind: "complete_swap", label: "Complete the swap", enabled: true },
        { kind: "rollback_swap", label: "Roll back to the old database", enabled: true },
      ],
    });
    b.set(interrupted);
    await event({ event: "run_finished", run_id: "run1", status: "interrupted", record: interrupted });
    const actions = screen.getByRole("group", { name: "Actions" });
    expect(within(actions).getAllByRole("button").map((x) => x.textContent)).toEqual(["Complete the swap", "Roll back to the old database"]);
    expect(screen.getByRole("status")).toHaveTextContent("exists only as shop_old");
    await user.click(within(actions).getByRole("button", { name: "Roll back to the old database" }));
    expect(b.calls.find((c) => c.cmd === "run_action")?.args).toEqual({ runId: "run1", action: "rollback_swap" });
  });
});
