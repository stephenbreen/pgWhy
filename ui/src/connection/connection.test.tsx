import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { mockIPC } from "@tauri-apps/api/mocks";
import { beforeEach, describe, expect, it } from "vitest";
import { App } from "../app/App";
import { registerTab, registerView } from "../app/registry";
import { useWorkspace } from "../app/workspace";
import type { ConnectionEntry } from "../lib/ipc/connections";
import { ConnectionForm } from "./ConnectionForm";
import { ConnectionList } from "./ConnectionList";
import { openConnectionForm } from "./ConnectionNode";
import { useConnections } from "./store";

const workspace0 = useWorkspace.getState();
const connections0 = useConnections.getState();

function entry(over: Partial<ConnectionEntry> = {}): ConnectionEntry {
  return {
    id: "c1", name: "Local dev", host: "localhost", port: 5432, database: "app", user: "me",
    maintenance_db: "postgres", sslmode: "prefer", env_tag: "local", password_saved: true, ...over,
  };
}

const info = (id: string) => ({
  handle: 1, connection_id: id, database: "app", purpose: "introspection", server_version_num: 160011, backend_pid: 42, tls: "plaintext",
});

type Call = { cmd: string; args: Record<string, unknown> };

function backend(entries: ConnectionEntry[], overrides: Record<string, (args: Record<string, unknown>) => unknown> = {}) {
  const calls: Call[] = [];
  mockIPC((cmd, args) => {
    calls.push({ cmd, args: args as Record<string, unknown> });
    if (overrides[cmd]) return overrides[cmd](args as Record<string, unknown>);
    switch (cmd) {
      case "list_connections":
        return { connections: entries, recents: [], keyring_available: true };
      case "save_connection": {
        const e = (args as { entry: ConnectionEntry }).entry;
        return { entry: { ...e, password_saved: true }, warnings: [], password_stored: "keyring" };
      }
      case "connect":
        return info((args as { id: string }).id);
      default:
        return null;
    }
  });
  return calls;
}

beforeEach(() => {
  useWorkspace.setState(workspace0, true);
  useConnections.setState(connections0, true);
  registerTab("connection-form", ConnectionForm);
  registerView("explorer", () => <ConnectionList />);
});

describe("connection manager UI", () => {
  it("creates a connection that defaults to the Production tag", async () => {
    const user = userEvent.setup();
    const calls = backend([]);
    await useConnections.getState().load();
    render(<App />);
    openConnectionForm();
    const form = await screen.findByRole("form", { name: "Connection" });
    await user.type(within(form).getByLabelText("Name"), "Prod replica");
    await user.type(within(form).getByLabelText("User"), "reader");
    await user.type(within(form).getByLabelText("Password"), "s3cret");
    expect(within(form).getByLabelText("Production")).toBeChecked();
    await user.click(within(form).getByRole("button", { name: "Save" }));

    const save = calls.find((c) => c.cmd === "save_connection")!;
    expect((save.args.entry as ConnectionEntry).env_tag).toBe("production");
    expect(save.args.password).toEqual({ action: "set", value: "s3cret", persist: true });
    expect(useConnections.getState().entries.map((e) => e.name)).toEqual(["Prod replica"]);
    // the form tab closes after a clean save
    expect(screen.queryByRole("form", { name: "Connection" })).not.toBeInTheDocument();
  });

  it("editing keeps the stored password unless a new one is typed", async () => {
    const user = userEvent.setup();
    const calls = backend([entry()]);
    await useConnections.getState().load();
    render(<App />);
    openConnectionForm("c1", "Local dev");
    const form = await screen.findByRole("form", { name: "Connection" });
    expect(within(form).getByLabelText("Password")).toHaveValue("");
    expect(within(form).getByLabelText("Password")).toHaveAttribute("placeholder", "saved — leave blank to keep");
    await user.click(within(form).getByLabelText("Dev"));
    await user.click(within(form).getByRole("button", { name: "Save" }));
    const save = calls.find((c) => c.cmd === "save_connection")!;
    expect(save.args.password).toEqual({ action: "unchanged" });
    expect((save.args.entry as ConnectionEntry).id).toBe("c1");
    expect((save.args.entry as ConnectionEntry).env_tag).toBe("dev");
  });

  it("shows save-time tag warnings instead of closing", async () => {
    const user = userEvent.setup();
    backend([entry()], {
      save_connection: (a) => ({ entry: a.entry, warnings: ["\"Prod\" points at the same server and database but is tagged Production (this one is Local)"] }),
    });
    await useConnections.getState().load();
    render(<App />);
    openConnectionForm("c1", "Local dev");
    const form = await screen.findByRole("form", { name: "Connection" });
    await user.click(within(form).getByRole("button", { name: "Save" }));
    expect(await within(form).findByRole("alert")).toHaveTextContent("tagged Production");
  });

  it("connecting shows the active connection and its tag in the status bar, and updates recents", async () => {
    const user = userEvent.setup();
    backend([entry(), entry({ id: "c2", name: "Staging", env_tag: "staging" })]);
    await useConnections.getState().load();
    render(<App />);
    await user.click(screen.getByRole("treeitem", { name: "Staging" }));
    const status = screen.getByRole("contentinfo", { name: "Status" });
    expect(status).toHaveTextContent("Staging");
    expect(within(status).getByText("Staging", { selector: ".tag-pill" })).toBeInTheDocument();
    expect(status).toHaveTextContent("PostgreSQL 16.11");
    expect(status).toHaveTextContent("not encrypted");
    expect(useConnections.getState().recents[0]).toBe("c2");
    expect(screen.getByRole("treeitem", { name: "Staging" })).toHaveTextContent("connected");
  });

  it("a failed connect shows an inline error and password prompt, never a modal", async () => {
    const user = userEvent.setup();
    let attempts = 0;
    const calls = backend([entry({ password_saved: false })], {
      connect: (a) => {
        attempts++;
        if (!a.password) throw { kind: "credentials", class: "fix_then_retry", message: "could not connect: the server asked for a password" };
        return info("c1");
      },
    });
    await useConnections.getState().load();
    render(<App />);
    await user.click(screen.getByRole("treeitem", { name: "Local dev" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("asked for a password");
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(screen.getByRole("treeitem", { name: "Local dev" })).toHaveTextContent("failed");

    await user.type(screen.getByLabelText("Password for Local dev"), "pw");
    await user.click(screen.getByRole("button", { name: "Connect" }));
    expect(attempts).toBe(2);
    expect(calls.filter((c) => c.cmd === "connect")[1].args.password).toEqual({ action: "set", value: "pw", persist: true });
    expect(screen.getByRole("treeitem", { name: "Local dev" })).toHaveTextContent("connected");
  });

  it("an unknown SSH host key shows its fingerprint inline; trusting records it and connects", async () => {
    const user = userEvent.setup();
    let trusted = false;
    const calls = backend([entry({ tunnel: { host: "bastion", port: 22, user: "me", auth: { method: "key", key_path: "~/.ssh/id_ed25519" } } })], {
      connect: () => {
        if (!trusted)
          throw {
            kind: "host_key", class: "fix_then_retry", message: "unknown SSH host key for bastion:22",
            data: { host: "bastion", port: 22, key_type: "ssh-ed25519", fingerprint: "SHA256:abc", status: "unknown" },
          };
        return info("c1");
      },
      trust_host_key: () => {
        trusted = true;
        return null;
      },
    });
    await useConnections.getState().load();
    render(<App />);
    await user.click(screen.getByRole("treeitem", { name: "Local dev" }));
    const prompt = await screen.findByRole("group", { name: "Host key" });
    expect(prompt).toHaveTextContent("ssh-ed25519");
    expect(prompt).toHaveTextContent("SHA256:abc");
    await user.click(within(prompt).getByRole("button", { name: "Trust and connect" }));
    expect(calls.find((c) => c.cmd === "trust_host_key")?.args).toEqual({ host: "bastion", port: 22, keyType: "ssh-ed25519", fingerprint: "SHA256:abc" });
    expect(screen.getByRole("treeitem", { name: "Local dev" })).toHaveTextContent("connected");
  });

  it("groups connections and renders tag bars", async () => {
    backend([entry(), entry({ id: "c3", name: "Acme prod", group: "Clients", env_tag: "production" })]);
    await useConnections.getState().load();
    render(<App />);
    const group = screen.getByRole("region", { name: "Clients" });
    expect(within(group).getByRole("treeitem", { name: "Acme prod" }).querySelector(".tag-bar")).toHaveAttribute("data-tag", "production");
  });
});
