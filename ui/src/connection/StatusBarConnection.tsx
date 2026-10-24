// Always-visible active connection identity for the status bar.

import { serverVersionLabel } from "../lib/ipc/connections";
import { useConnections } from "./store";
import { TagPill } from "./EnvTag";

const TLS_LABEL = {
  plaintext: "not encrypted",
  encrypted_unverified: "TLS (unverified)",
  encrypted_verified: "TLS verified",
} as const;

export function StatusBarConnection() {
  const active = useConnections((s) => s.entries.find((e) => e.id === s.activeId));
  const status = useConnections((s) => (s.activeId ? s.status[s.activeId] : undefined));
  if (!active) return <span className="status-muted">No active connection</span>;
  return (
    <span className="status-conn" data-tag={active.env_tag} style={{ ["--status-tag" as string]: `var(--tag-${active.env_tag})` }}>
      <TagPill tag={active.env_tag} />
      <span className="status-name">{active.name}</span>
      <span className="status-muted">
        {active.user}@{active.host}:{active.port}/{active.database}
      </span>
      {status?.state === "connected" && (
        <>
          <span className="status-muted">{serverVersionLabel(status.info.server_version_num)}</span>
          <span className="status-muted" data-tls={status.info.tls}>{TLS_LABEL[status.info.tls]}</span>
        </>
      )}
      {status?.state === "connecting" && <span className="status-muted">connecting…</span>}
      {status?.state === "failed" && <span className="status-error">{status.error.message}</span>}
    </span>
  );
}
