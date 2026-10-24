import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { mockIPC } from "@tauri-apps/api/mocks";
import { beforeEach, describe, expect, it } from "vitest";
import { App } from "../app/App";
import { useWorkspace } from "../app/workspace";
import { useConnections } from "../connection/store";
import type { ConnectionEntry } from "../lib/ipc/connections";
import { initWorkflow } from "./index";
import { useWorkflows } from "./store";
import { openWorkflowEditor } from "./WorkflowEditor";

const ws0 = useWorkspace.getState();
const conn0 = useConnections.getState();
const mk = (id: string, name: string, tag: ConnectionEntry["env_tag"]): ConnectionEntry => ({
  id, name, host: `${id}.example`, port: 5432, database: "app", user: "u", maintenance_db: "postgres", sslmode: "prefer", env_tag: tag, password_saved: true,
});

function backend(overrides: Record<string, (a: Record<string, unknown>) => unknown> = {}) {
  const calls: { cmd: string; args: Record<string, unknown> }[] = [];
  mockIPC((cmd, args) => {
    const a = args as Record<string, unknown>;
    calls.push({ cmd, args: a });
    if (overrides[cmd]) return overrides[cmd](a);
    switch (cmd) {
      case "library_list":
        return { root: "/lib", tags: [], scripts: [
          { id: "ok", path: "post/anonymize.sql", name: "anonymize", folder: "post", tags: [], fingerprint: { sha256: "h", size: 1, modified_ms: 1 }, missing: false },
          { id: "bad", path: "post/psql.sql", name: "psql", folder: "post", tags: [], fingerprint: { sha256: "h", size: 1, modified_ms: 1 }, missing: false },
        ] };
      case "workflow_check_script":
        return a.id === "bad"
          ? { ok: false, problems: ["contains psql meta-commands, which pgWhy can't run: \\c (line 1)"], needs_no_transaction: [] }
          : { ok: true, problems: [], needs_no_transaction: [] };
      case "workflow_save":
        return a.workflow;
      case "workflow_list":
        return [];
      default:
        return null;
    }
  });
  return calls;
}

beforeEach(() => {
  useWorkspace.setState(ws0, true);
  useConnections.setState({ ...conn0, loaded: true, entries: [mk("prod", "Prod", "production"), mk("stage", "Stage", "staging"), mk("local", "Local", "local")] }, true);
  useWorkflows.setState({ loaded: false, workflows: [] });
  initWorkflow();
});

describe("workflow editor", () => {
  it("Production and Staging connections can't be chosen as the target", async () => {
    backend();
    render(<App />);
    openWorkflowEditor();
    const target = await screen.findByLabelText("Target connection");
    const options = within(target).getAllByRole("option");
    expect(options.find((o) => o.textContent?.startsWith("Prod"))).toBeDisabled();
    expect(options.find((o) => o.textContent?.startsWith("Stage"))).toBeDisabled();
    expect(options.find((o) => o.textContent?.startsWith("Local"))).not.toBeDisabled();
  });

  it("a server-side refusal is shown inline and nothing is saved", async () => {
    const user = userEvent.setup();
    backend({ workflow_save: () => { throw { kind: "refused", class: "fix_then_retry", message: "\"Prod\" is tagged Production; a Refresh workflow can only replace databases on Local or Dev connections" }; } });
    render(<App />);
    openWorkflowEditor();
    await user.selectOptions(await screen.findByLabelText("Source connection"), "prod");
    await user.selectOptions(screen.getByLabelText("Target connection"), "local");
    await user.click(screen.getByRole("button", { name: "Save" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("only replace databases on Local or Dev");
    expect(useWorkflows.getState().workflows).toHaveLength(0);
  });

  it("a script with psql meta-commands blocks saving with the reason", async () => {
    const user = userEvent.setup();
    const calls = backend();
    render(<App />);
    openWorkflowEditor();
    await user.selectOptions(await screen.findByLabelText("Post-restore script"), "bad");
    expect(await screen.findByText(/psql meta-commands/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Save" })).toBeDisabled();
    await user.selectOptions(screen.getByLabelText("Post-restore script"), "ok");
    await user.selectOptions(screen.getByLabelText("Source connection"), "prod");
    await user.selectOptions(screen.getByLabelText("Target connection"), "local");
    expect(screen.getByRole("button", { name: "Save" })).toBeEnabled();
    await user.click(screen.getByRole("button", { name: "Save" }));
    const saved = calls.find((c) => c.cmd === "workflow_save")!.args.workflow as { post_restore_script: string; options: { strategy: string } };
    expect(saved.post_restore_script).toBe("ok");
    expect(saved.options.strategy).toBe("staging_swap");
    expect(useWorkflows.getState().workflows).toHaveLength(1);
  });
});
