import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { mockIPC } from "@tauri-apps/api/mocks";
import { beforeEach, describe, expect, it } from "vitest";
import { App } from "../app/App";
import { useWorkspace } from "../app/workspace";
import { useConnections } from "../connection/store";
import type { ConnectionEntry } from "../lib/ipc/connections";
import { defaultOptions, type RefreshWorkflow, type WorkflowTemplate } from "../lib/ipc/workflow";
import { initWorkflow } from "./index";
import { useWorkflows } from "./store";
import { openWorkflowEditor } from "./WorkflowEditor";

const ws0 = useWorkspace.getState();
const conn0 = useConnections.getState();
const mk = (id: string, name: string, tag: ConnectionEntry["env_tag"]): ConnectionEntry => ({
  id, name, host: `${id}.example`, port: 5432, database: "app", user: "u", maintenance_db: "postgres", sslmode: "prefer", env_tag: tag, password_saved: true,
});

const SAMPLE_TEMPLATES: WorkflowTemplate[] = [
  {
    id: "restore_reset_users",
    name: "Restore backup, delete users & seed test users",
    description: "Restore a backup archive, clear production users, insert QA test users, and verify.",
    workflow: {
      schema_version: 2,
      id: "tpl-1",
      name: "Restore backup, delete users & seed test users",
      source: { connection: "", database: "" },
      source_mode: { mode: "from_backup_file", archive: { kind: "prompt_at_run", param: null } },
      target: { connection: "", database: "app_local" },
      parameters: [
        { name: "test_password", label: "Test user password", kind: "secret", required: true, default: null },
      ],
      steps: [
        {
          id: "s-del",
          name: "Delete non-system users",
          enabled: true,
          no_transaction: false,
          kind: "delete_rows",
          schema: "public",
          table: "users",
          mode: "delete_where",
          conditions: [{ column: "email", op: "not_ilike", value: { kind: "literal", value: "%@example.test" } }],
          combinator: "and",
        },
        {
          id: "s-ins",
          name: "Seed test users",
          enabled: true,
          no_transaction: false,
          kind: "insert_rows",
          schema: "public",
          table: "users",
          columns: ["email", "password_hash"],
          rows: [[{ kind: "literal", value: "admin@example.test" }, { kind: "param", name: "test_password" }]],
          on_conflict: { mode: "do_nothing", columns: ["email"] },
        },
        {
          id: "s-ver",
          name: "Verify test users exist",
          enabled: true,
          no_transaction: false,
          kind: "verify",
          assertion: { kind: "row_count", schema: "public", table: "users", op: "gte", expected: 1, conditions: [], combinator: "and" },
        },
      ],
      options: { ...defaultOptions(), pause_before_swap: true },
      has_succeeded: false,
    },
  },
];

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
      case "workflow_templates":
        return SAMPLE_TEMPLATES;
      case "workflow_check_script":
        return a.id === "bad"
          ? { ok: false, problems: ["contains psql meta-commands, which pgWhy can't run: \\c (line 1)"], needs_no_transaction: [] }
          : { ok: true, problems: [], needs_no_transaction: [] };
      case "workflow_preview_sql": {
        const wf = a.workflow as RefreshWorkflow;
        return (wf.steps ?? [])
          .filter((s) => s.enabled)
          .map((s) => ({
            step_id: s.id,
            step_name: s.name,
            sql: s.kind === "insert_rows"
              ? `INSERT INTO "public"."users" ("email", "password_hash") VALUES ('admin@example.test', '••••••••');`
              : `DELETE FROM "public"."users";`,
            no_transaction: s.no_transaction,
            is_verify: s.kind === "verify",
            warnings: [],
            problems: [],
            referenced_params: [],
          }));
      }
      case "workflow_convert_step_to_sql": {
        const wf = a.workflow as RefreshWorkflow;
        const step = (wf.steps ?? []).find((s) => s.id === a.stepId)!;
        return {
          id: step.id,
          name: step.name,
          enabled: step.enabled,
          no_transaction: false,
          kind: "sql",
          source: "inline",
          sql: `DELETE FROM "public"."users" WHERE "email" NOT ILIKE '%@example.test';`,
        };
      }
      case "workflow_export_toml":
        return `schema_version = 2\nname = "Saved workflow"\n`;
      case "workflow_import_toml":
        return {
          ...SAMPLE_TEMPLATES[0].workflow,
          id: "imported-1",
          name: "Imported QA reset",
        };
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
  it("Staging and Production targets are selectable and show a typed-confirmation warning", async () => {
    const user = userEvent.setup();
    backend();
    render(<App />);
    openWorkflowEditor();
    const target = await screen.findByLabelText("Target connection");
    const options = within(target).getAllByRole("option");
    expect(options.find((o) => o.textContent?.startsWith("Prod"))).not.toBeDisabled();
    expect(options.find((o) => o.textContent?.startsWith("Stage"))).not.toBeDisabled();
    expect(options.find((o) => o.textContent?.startsWith("Local"))).not.toBeDisabled();

    await user.selectOptions(target, "prod");
    expect(await screen.findByRole("status")).toHaveTextContent(/typing the exact database name/i);
  });

  it("a server-side refusal is shown inline and nothing is saved", async () => {
    const user = userEvent.setup();
    backend({ workflow_save: () => { throw { kind: "refused", class: "fix_then_retry", message: "source and target cannot be the same database" }; } });
    render(<App />);
    openWorkflowEditor();
    await user.selectOptions(await screen.findByLabelText("Source connection"), "prod");
    await user.selectOptions(screen.getByLabelText("Target connection"), "local");
    await user.click(screen.getByRole("button", { name: "Save" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("source and target cannot be the same database");
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

  it("loads a starter template (restore backup, delete users & seed test users) and saves a v2 pipeline", async () => {
    const user = userEvent.setup();
    const calls = backend();
    render(<App />);
    openWorkflowEditor();
    const tplBtn = await screen.findByRole("button", { name: /Restore backup, delete users & seed test users/i });
    await user.click(tplBtn);

    expect(screen.getByRole("article", { name: /Step 1: Delete non-system users/i })).toBeInTheDocument();
    expect(screen.getByRole("article", { name: /Step 2: Seed test users/i })).toBeInTheDocument();
    expect(screen.getByRole("article", { name: /Step 3: Verify test users exist/i })).toBeInTheDocument();

    await user.selectOptions(screen.getByLabelText("Target connection"), "local");
    await user.click(screen.getByRole("button", { name: "Save" }));

    const saved = calls.find((c) => c.cmd === "workflow_save")!.args.workflow as RefreshWorkflow;
    expect(saved.schema_version).toBe(2);
    expect(saved.source_mode?.mode).toBe("from_backup_file");
    expect(saved.steps).toHaveLength(3);
    expect(saved.parameters?.[0]?.name).toBe("test_password");
    expect(saved.parameters?.[0]?.default).toBeNull();
    expect(saved.options.pause_before_swap).toBe(true);
  });

  it("previews compiled SQL with masked secrets, converts a visual step to SQL, and reorders steps", async () => {
    const user = userEvent.setup();
    backend();
    render(<App />);
    openWorkflowEditor();
    await user.click(await screen.findByRole("button", { name: /Restore backup, delete users & seed test users/i }));

    const step2 = screen.getByRole("article", { name: /Step 2: Seed test users/i });
    await user.click(within(step2).getByRole("button", { name: "Preview SQL" }));
    expect(await within(step2).findByText(/'••••••••'/)).toBeInTheDocument();

    const step1 = screen.getByRole("article", { name: /Step 1: Delete non-system users/i });
    await user.click(within(step1).getByRole("button", { name: "Convert to SQL" }));
    expect(await within(step1).findByLabelText("Inline SQL editor")).toHaveValue(
      `DELETE FROM "public"."users" WHERE "email" NOT ILIKE '%@example.test';`,
    );

    await user.click(within(step1).getByRole("button", { name: "Move step 1 down" }));
    expect(screen.getByRole("article", { name: /Step 1: Seed test users/i })).toBeInTheDocument();
    expect(screen.getByRole("article", { name: /Step 2: Delete non-system users/i })).toBeInTheDocument();
  });

  it("imports and exports shareable workflow TOML", async () => {
    const user = userEvent.setup();
    const existingWf: RefreshWorkflow = {
      ...SAMPLE_TEMPLATES[0].workflow,
      id: "saved-1",
      name: "Saved workflow",
      target: { connection: "local", database: "app" },
    };
    useWorkflows.setState({ loaded: true, workflows: [existingWf] });
    backend();
    render(<App />);
    openWorkflowEditor(existingWf.id, existingWf.name);

    await user.click(await screen.findByRole("button", { name: "Export TOML" }));
    const exportDialog = await screen.findByRole("dialog", { name: "Export workflow TOML" });
    expect(within(exportDialog).getByLabelText("Exported workflow TOML")).toHaveValue(
      `schema_version = 2\nname = "Saved workflow"\n`,
    );
    await user.click(within(exportDialog).getByRole("button", { name: "Close" }));

    await user.click(screen.getByRole("button", { name: "Import TOML" }));
    const importDialog = await screen.findByRole("dialog", { name: "Import workflow TOML" });
    await user.type(within(importDialog).getByLabelText("Workflow TOML input"), `schema_version = 2`);
    await user.click(within(importDialog).getByRole("button", { name: "Load workflow" }));
    expect(await screen.findByDisplayValue("Imported QA reset")).toBeInTheDocument();
  });
});
