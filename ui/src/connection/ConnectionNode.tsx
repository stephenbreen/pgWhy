// One connection entry as a tree root row: tag colour bar, name, live status badge, and an
// inline (non-modal) password prompt when the server asks for credentials. The object tree
// (U9) renders these as its root nodes and hangs databases under connected ones.

import { useState, type ReactNode } from "react";
import { trustHostKey, type ConnectionEntry, type HostKeyPrompt, type TunnelStatus } from "../lib/ipc/connections";
import { toAppError } from "../lib/ipc/errors";
import { useWorkspace } from "../app/workspace";
import { useConnections, type ConnStatus } from "./store";
import { TagBar } from "./EnvTag";
import { ErrorNote } from "./ErrorNote";

export function openConnectionForm(entryId?: string, name?: string) {
  useWorkspace.getState().openTab({
    id: entryId ? `connection-form:${entryId}` : `connection-form:new:${crypto.randomUUID()}`,
    kind: "connection-form",
    title: name ? `Edit ${name}` : "New connection",
    data: { entryId },
  });
}

function badge(status: ConnStatus | undefined): { label: string; state: string } {
  switch (status?.state) {
    case "connecting":
      return { label: "connecting…", state: "connecting" };
    case "connected":
      return { label: status.info.tls === "plaintext" ? "connected · not encrypted" : "connected", state: "connected" };
    case "failed":
      return { label: "failed", state: "failed" };
    default:
      return { label: "", state: "disconnected" };
  }
}

type SecretNeed = "db" | "ssh_key_passphrase" | "ssh_password" | null;

function secretNeeded(status: ConnStatus | undefined): SecretNeed {
  if (status?.state !== "failed") return null;
  const data = status.error.data as { secret?: string } | undefined;
  if (data?.secret === "ssh_key_passphrase" || data?.secret === "ssh_password") return data.secret;
  return status.error.kind === "credentials" || status.error.kind === "auth" ? "db" : null;
}

function hostKeyPrompt(status: ConnStatus | undefined): HostKeyPrompt | null {
  if (status?.state !== "failed" || status.error.kind !== "host_key") return null;
  const d = status.error.data as HostKeyPrompt | undefined;
  return d?.fingerprint ? d : null;
}

function tunnelBadge(t: TunnelStatus | undefined): string | null {
  if (!t) return null;
  if (t.state === "reconnecting") return `ssh reconnecting${t.attempt ? ` (${t.attempt})` : ""}…`;
  if (t.state === "starting") return "ssh starting…";
  if (t.state === "down" && t.error) return "ssh down";
  return null;
}

export function ConnectionNode({
  entry,
  expanded,
  onToggle,
  children,
}: {
  entry: ConnectionEntry;
  expanded: boolean;
  /** `open` forces a state; omitted toggles */
  onToggle: (open?: boolean) => void;
  children?: ReactNode;
}) {
  const status = useConnections((s) => s.status[entry.id]);
  const tunnel = useConnections((s) => s.tunnels[entry.id]);
  const [trustError, setTrustError] = useState<string | null>(null);
  const activeId = useConnections((s) => s.activeId);
  const connect = useConnections((s) => s.connect);
  const disconnect = useConnections((s) => s.disconnect);
  const setActive = useConnections((s) => s.setActive);
  const keyringAvailable = useConnections((s) => s.keyringAvailable);
  const [password, setPassword] = useState("");
  const [remember, setRemember] = useState(true);
  const b = badge(status);
  const connected = status?.state === "connected";

  const activate = async () => {
    setActive(entry.id);
    if (!connected && status?.state !== "connecting") {
      if (await connect(entry.id)) onToggle(true);
    } else {
      onToggle();
    }
  };

  return (
    <div className="conn-node" data-active={activeId === entry.id}>
      <div
        className="tree-row conn-row"
        role="treeitem"
        aria-expanded={connected ? expanded : undefined}
        aria-selected={activeId === entry.id}
        aria-label={entry.name}
        tabIndex={0}
        onClick={activate}
        onKeyDown={(e) => e.key === "Enter" && activate()}
      >
        <TagBar tag={entry.env_tag} />
        <span className="twisty" aria-hidden="true">{connected ? (expanded ? "▾" : "▸") : "·"}</span>
        <span className="node-label">{entry.name}</span>
        <span className="node-meta">{entry.tunnel ? "ssh · " : ""}{entry.host}</span>
        {tunnelBadge(tunnel) && <span className="status-badge" data-state="connecting">{tunnelBadge(tunnel)}</span>}
        {b.label && <span className="status-badge" data-state={b.state}>{b.label}</span>}
        <span className="row-actions">
          {connected && (
            <button type="button" className="icon" aria-label={`Disconnect ${entry.name}`} onClick={(e) => { e.stopPropagation(); void disconnect(entry.id); }}>
              ⏏
            </button>
          )}
          <button type="button" className="icon" aria-label={`Edit ${entry.name}`} onClick={(e) => { e.stopPropagation(); openConnectionForm(entry.id, entry.name); }}>
            ✎
          </button>
        </span>
      </div>
      {status?.state === "failed" && (
        <div className="node-inline">
          <ErrorNote error={status.error} compact />
          {hostKeyPrompt(status) && (() => {
            const p = hostKeyPrompt(status)!;
            return (
              <div className="hostkey-prompt" role="group" aria-label="Host key">
                {p.status === "changed" ? (
                  <p className="warn">
                    The SSH host key for {p.host}:{p.port} changed — possible man-in-the-middle. Previously {p.previous_key_type}{" "}
                    <code>{p.previous_fingerprint}</code>, now {p.key_type} <code>{p.fingerprint}</code>. Only trust the new key if
                    you know the server was rebuilt or rekeyed.
                  </p>
                ) : (
                  <p>
                    First connection to {p.host}:{p.port}. Verify the host key before trusting it: {p.key_type} <code>{p.fingerprint}</code>
                  </p>
                )}
                {trustError && <p className="warn">{trustError}</p>}
                <button
                  type="button"
                  onClick={() =>
                    void trustHostKey(p)
                      .then(() => connect(entry.id))
                      .catch((e) => setTrustError(toAppError(e).message))
                  }
                >
                  {p.status === "changed" ? "Trust the new key" : "Trust and connect"}
                </button>
              </div>
            );
          })()}
          {secretNeeded(status) && (
            <form
              className="inline-prompt"
              onSubmit={(e) => {
                e.preventDefault();
                const input = { action: "set" as const, value: password, persist: remember && keyringAvailable };
                const need = secretNeeded(status);
                void (need === "db" ? connect(entry.id, input) : connect(entry.id, undefined, input)).then(() => setPassword(""));
              }}
            >
              <input
                type="password"
                aria-label={`${secretNeeded(status) === "ssh_key_passphrase" ? "Key passphrase" : secretNeeded(status) === "ssh_password" ? "SSH password" : "Password"} for ${entry.name}`}
                placeholder={secretNeeded(status) === "ssh_key_passphrase" ? "key passphrase" : secretNeeded(status) === "ssh_password" ? "SSH password" : "password"}
                value={password}
                autoFocus
                onChange={(e) => setPassword(e.target.value)}
              />
              {keyringAvailable && (
                <label className="check">
                  <input type="checkbox" checked={remember} onChange={(e) => setRemember(e.target.checked)} /> save
                </label>
              )}
              <button type="submit">Connect</button>
            </form>
          )}
          {!secretNeeded(status) && !hostKeyPrompt(status) && status.error.class === "retriable" && (
            <button type="button" onClick={() => void connect(entry.id)}>Retry</button>
          )}
        </div>
      )}
      {connected && expanded && children}
    </div>
  );
}
