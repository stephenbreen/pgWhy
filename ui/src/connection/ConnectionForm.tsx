// New/edit connection form (R1, R18). Opens as a main-area tab, not a modal, so the tree
// stays usable while it's open. Secrets use blank-with-"unchanged" semantics: the stored
// password is never sent to the UI; leaving the field blank keeps it.

import { useState, type FormEvent } from "react";
import * as ipc from "../lib/ipc/connections";
import { toAppError, type AppError } from "../lib/ipc/errors";
import { useWorkspace, type Tab } from "../app/workspace";
import { useConnections } from "./store";
import { TAG_LABEL, TAGS } from "./EnvTag";
import { ErrorNote } from "./ErrorNote";

export interface ConnectionFormData {
  /** existing entry to edit; absent = new connection */
  entryId?: string;
}

const SSL_MODES: { value: ipc.SslMode; label: string }[] = [
  { value: "disable", label: "disable — never encrypt" },
  { value: "prefer", label: "prefer — encrypt if the server offers it" },
  { value: "require", label: "require — encrypt, don't verify the certificate" },
  { value: "verify-ca", label: "verify-ca — encrypt, verify the certificate chain" },
  { value: "verify-full", label: "verify-full — verify chain and host name" },
];

function secretInput(value: string, persist: boolean, cleared: boolean): ipc.SecretInput {
  if (cleared) return { action: "clear" };
  if (value === "") return { action: "unchanged" };
  return { action: "set", value, persist };
}

export function ConnectionForm({ tab }: { tab: Tab }) {
  const data = (tab.data ?? {}) as ConnectionFormData;
  const existing = useConnections((s) => s.entries.find((e) => e.id === data.entryId));
  const keyringAvailable = useConnections((s) => s.keyringAvailable);
  const save = useConnections((s) => s.save);
  const closeTab = useWorkspace((s) => s.closeTab);
  const updateTab = useWorkspace((s) => s.updateTab);

  const [entry, setEntry] = useState<ipc.ConnectionEntry>(() => existing ?? ipc.newConnectionEntry());
  const [password, setPassword] = useState("");
  const [savePassword, setSavePassword] = useState(true);
  const [clearPassword, setClearPassword] = useState(false);
  const [sshSecret, setSshSecret] = useState("");
  const [useTunnel, setUseTunnel] = useState(!!entry.tunnel);
  const [error, setError] = useState<AppError | null>(null);
  const [warnings, setWarnings] = useState<string[]>([]);
  const [testResult, setTestResult] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const set = <K extends keyof ipc.ConnectionEntry>(k: K, v: ipc.ConnectionEntry[K]) =>
    setEntry((e) => ({ ...e, [k]: v }));
  const tunnel: ipc.TunnelConfig = entry.tunnel ?? { host: "", port: 22, user: "", auth: { method: "key", key_path: "" } };
  const setTunnel = (t: Partial<ipc.TunnelConfig>) => setEntry((e) => ({ ...e, tunnel: { ...tunnel, ...t } }));

  const effective = (): ipc.ConnectionEntry => ({ ...entry, tunnel: useTunnel ? tunnel : undefined });

  const onTest = async () => {
    setBusy(true);
    setError(null);
    setTestResult(null);
    try {
      const info = await ipc.testConnection(effective(), password || undefined);
      const tls = { plaintext: "not encrypted", encrypted_unverified: "encrypted (certificate not verified)", encrypted_verified: "encrypted and verified" }[info.tls];
      setTestResult(`Connected: ${ipc.serverVersionLabel(info.server_version_num)}, ${tls}.`);
    } catch (e) {
      setError(toAppError(e));
    } finally {
      setBusy(false);
    }
  };

  const onSubmit = async (ev: FormEvent) => {
    ev.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const pw = secretInput(password, savePassword && keyringAvailable, clearPassword);
      const ssh = sshSecret ? ({ action: "set", value: sshSecret, persist: savePassword && keyringAvailable } as const) : ({ action: "unchanged" } as const);
      const res = await save(effective(), pw, ssh);
      setEntry(res.entry);
      setPassword("");
      setSshSecret("");
      setClearPassword(false);
      updateTab(tab.id, { title: res.entry.name, connectionId: res.entry.id, data: { entryId: res.entry.id } });
      if (res.warnings.length) setWarnings(res.warnings);
      else closeTab(tab.id);
    } catch (e) {
      setError(toAppError(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <form className="form-page" onSubmit={onSubmit} aria-label="Connection">
      <h2>{existing ? `Edit ${existing.name}` : "New connection"}</h2>

      <fieldset>
        <legend>Connection</legend>
        <label>
          Name <input value={entry.name} onChange={(e) => set("name", e.target.value)} required autoFocus />
        </label>
        <label>
          Group <input value={entry.group ?? ""} placeholder="e.g. Clients/Acme" onChange={(e) => set("group", e.target.value || undefined)} />
        </label>
        <div className="row">
          <label className="grow">
            Host <input value={entry.host} onChange={(e) => set("host", e.target.value)} required />
          </label>
          <label className="narrow">
            Port <input type="number" value={entry.port} min={1} max={65535} onChange={(e) => set("port", Number(e.target.value))} />
          </label>
        </div>
        <div className="row">
          <label className="grow">
            Database <input value={entry.database} onChange={(e) => set("database", e.target.value)} required />
          </label>
          <label className="grow">
            Maintenance database <input value={entry.maintenance_db} onChange={(e) => set("maintenance_db", e.target.value)} />
          </label>
        </div>
        <label>
          User <input value={entry.user} onChange={(e) => set("user", e.target.value)} required autoComplete="off" />
        </label>
        <label>
          Password
          <input
            type="password"
            value={password}
            autoComplete="new-password"
            placeholder={entry.password_saved && !clearPassword ? "saved — leave blank to keep" : ""}
            onChange={(e) => {
              setPassword(e.target.value);
              setClearPassword(false);
            }}
          />
        </label>
        <div className="checks">
          <label className="check">
            <input type="checkbox" checked={savePassword && keyringAvailable} disabled={!keyringAvailable} onChange={(e) => setSavePassword(e.target.checked)} />
            Save in the system keychain
          </label>
          {entry.password_saved && (
            <label className="check">
              <input type="checkbox" checked={clearPassword} onChange={(e) => setClearPassword(e.target.checked)} /> Forget the saved password
            </label>
          )}
        </div>
        {!keyringAvailable && <p className="note">No system credential store is available; passwords are kept for this session only.</p>}
      </fieldset>

      <fieldset>
        <legend>Environment</legend>
        <div className="tag-choice" role="radiogroup" aria-label="Environment tag">
          {TAGS.map((t) => (
            <label key={t} className="tag-option" data-tag={t}>
              <input type="radio" name="env_tag" value={t} checked={entry.env_tag === t} onChange={() => set("env_tag", t)} />
              {TAG_LABEL[t]}
            </label>
          ))}
        </div>
        <p className="note">Refresh workflows can only replace databases on Local or Dev connections.</p>
      </fieldset>

      <fieldset>
        <legend>Security</legend>
        <label>
          SSL mode
          <select value={entry.sslmode} onChange={(e) => set("sslmode", e.target.value as ipc.SslMode)}>
            {SSL_MODES.map((m) => (
              <option key={m.value} value={m.value}>
                {m.label}
              </option>
            ))}
          </select>
        </label>
        {(entry.sslmode === "verify-ca" || entry.sslmode === "verify-full") && (
          <label>
            Root certificate file
            <input value={entry.ssl_root_cert ?? ""} placeholder="system roots" onChange={(e) => set("ssl_root_cert", e.target.value || undefined)} />
          </label>
        )}
        <label className="check">
          <input type="checkbox" checked={useTunnel} onChange={(e) => setUseTunnel(e.target.checked)} /> Connect through an SSH tunnel
        </label>
        {useTunnel && (
          <div className="sub">
            <div className="row">
              <label className="grow">
                SSH host <input value={tunnel.host} onChange={(e) => setTunnel({ host: e.target.value })} required />
              </label>
              <label className="narrow">
                Port <input type="number" value={tunnel.port} onChange={(e) => setTunnel({ port: Number(e.target.value) })} />
              </label>
            </div>
            <label>
              SSH user <input value={tunnel.user} onChange={(e) => setTunnel({ user: e.target.value })} required />
            </label>
            <label>
              Authentication
              <select
                value={tunnel.auth.method}
                onChange={(e) =>
                  setTunnel({ auth: e.target.value === "key" ? { method: "key", key_path: "" } : { method: "password" } })
                }
              >
                <option value="key">Private key</option>
                <option value="password">Password</option>
              </select>
            </label>
            {tunnel.auth.method === "key" && (
              <label>
                Key file
                <input
                  value={tunnel.auth.key_path}
                  placeholder="~/.ssh/id_ed25519"
                  onChange={(e) => setTunnel({ auth: { method: "key", key_path: e.target.value } })}
                  required
                />
              </label>
            )}
            <label>
              {tunnel.auth.method === "key" ? "Key passphrase" : "SSH password"}
              <input type="password" value={sshSecret} autoComplete="new-password" placeholder="leave blank to keep / none" onChange={(e) => setSshSecret(e.target.value)} />
            </label>
            <p className="note">The database host above is the server's real name as seen from the SSH host.</p>
          </div>
        )}
      </fieldset>

      {error && <ErrorNote error={error} />}
      {testResult && <p className="ok-note" role="status">{testResult}</p>}
      {warnings.length > 0 && (
        <div className="warn-note" role="alert">
          <p>Saved, with a warning:</p>
          <ul>
            {warnings.map((w) => (
              <li key={w}>{w}</li>
            ))}
          </ul>
          <button type="button" onClick={() => closeTab(tab.id)}>
            OK
          </button>
        </div>
      )}

      <div className="actions">
        <button type="button" onClick={onTest} disabled={busy}>
          Test
        </button>
        <span className="spacer" />
        <button type="button" onClick={() => closeTab(tab.id)} disabled={busy}>
          Cancel
        </button>
        <button type="submit" className="primary" disabled={busy}>
          Save
        </button>
      </div>
    </form>
  );
}
