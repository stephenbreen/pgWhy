// Settings tab: top-N rows, backup retention and folders, script library, pg tool search dirs.

import { useEffect, useState, type FormEvent } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useWorkspace, type Tab } from "./workspace";
import { ErrorNote } from "../connection/ErrorNote";
import { toAppError, type AppError } from "../lib/ipc/errors";

interface AppSettings {
  top_n: number;
  backup_retention: number;
  backup_dir?: string | null;
  idle_close_secs: number;
  grid_lock_timeout_ms: number;
  script_library_dir?: string | null;
  pg_tools_dirs: string[];
  /** rows one result set keeps before it is marked truncated */
  result_row_cap: number;
  /** megabytes one result set keeps before it is marked truncated */
  result_byte_cap_mb: number;
}

interface SettingsView {
  settings: AppSettings;
  defaults: { backup_dir: string; script_library_dir: string };
  tools: string[];
}

export function openSettings() {
  useWorkspace.getState().openTab({ id: "settings", kind: "settings", title: "Settings" });
}

export function SettingsTab({ tab }: { tab: Tab }) {
  const [view, setView] = useState<SettingsView | null>(null);
  const [s, setS] = useState<AppSettings | null>(null);
  const [error, setError] = useState<AppError | null>(null);
  const [saved, setSaved] = useState(false);
  const closeTab = useWorkspace((x) => x.closeTab);

  const apply = (v: SettingsView) => {
    setView(v);
    setS(v.settings);
  };

  useEffect(() => {
    void invoke<SettingsView>("settings_get", { rescanTools: false }).then(apply).catch((e) => setError(toAppError(e)));
  }, []);

  if (!s || !view) return error ? <ErrorNote error={error} /> : <p className="empty">Loading…</p>;
  const set = <K extends keyof AppSettings>(k: K, v: AppSettings[K]) => {
    setSaved(false);
    setS({ ...s, [k]: v });
  };

  // Folders come only from the backend's native picker: the backend refuses any it did not show.
  const choose = async (apply: (dir: string) => void) => {
    setError(null);
    try {
      const dir = await invoke<string | null>("pick_folder");
      if (dir) apply(dir);
    } catch (err) {
      setError(toAppError(err));
    }
  };

  const onSubmit = async (e: FormEvent) => {
    e.preventDefault();
    setError(null);
    try {
      apply(
        await invoke<SettingsView>("settings_set", {
          settings: s,
        }),
      );
      setSaved(true);
    } catch (err) {
      setError(toAppError(err));
    }
  };

  return (
    <form className="form-page" aria-label="Settings" onSubmit={onSubmit}>
      <h2>Settings</h2>
      <fieldset>
        <legend>Data</legend>
        <div className="row">
          <label className="narrow">
            Top rows (N) <input type="number" min={1} value={s.top_n} onChange={(e) => set("top_n", Number(e.target.value))} />
          </label>
          <label className="grow">
            Grid commit lock timeout (ms) <input type="number" min={0} value={s.grid_lock_timeout_ms} onChange={(e) => set("grid_lock_timeout_ms", Number(e.target.value))} />
          </label>
        </div>
      </fieldset>
      <fieldset>
        <legend>Results memory</legend>
        <div className="row">
          <label className="narrow">
            Rows kept per result <input type="number" min={1} value={s.result_row_cap} onChange={(e) => set("result_row_cap", Number(e.target.value))} />
          </label>
          <label className="narrow">
            MB kept per result <input type="number" min={1} value={s.result_byte_cap_mb} onChange={(e) => set("result_byte_cap_mb", Number(e.target.value))} />
          </label>
        </div>
        <p className="note">
          Whichever cap a result reaches first stops the stream and marks it truncated; older result sets of
          the same tab are released to stay within twice these limits. Large selections copy only up to
          32 MB — use export for more.
        </p>
      </fieldset>
      <fieldset>
        <legend>Backups</legend>
        <label className="narrow">
          Keep last <input type="number" min={1} value={s.backup_retention} onChange={(e) => set("backup_retention", Number(e.target.value))} />
        </label>
        <div className="row">
          <label className="grow">
            Backup folder <input readOnly value={s.backup_dir ?? ""} placeholder={view.defaults.backup_dir} />
          </label>
          <button type="button" onClick={() => void choose((d) => set("backup_dir", d))}>Choose…</button>
          <button type="button" disabled={!s.backup_dir} onClick={() => set("backup_dir", null)}>Default</button>
        </div>
      </fieldset>
      <fieldset>
        <legend>Scripts</legend>
        <div className="row">
          <label className="grow">
            Library folder <input readOnly value={s.script_library_dir ?? ""} placeholder={view.defaults.script_library_dir} />
          </label>
          <button type="button" onClick={() => void choose((d) => set("script_library_dir", d))}>Choose…</button>
          <button type="button" disabled={!s.script_library_dir} onClick={() => set("script_library_dir", null)}>Default</button>
        </div>
      </fieldset>
      <fieldset>
        <legend>Postgres client tools</legend>
        <div className="note">Extra folders to search first</div>
        <ul className="tool-list">
          {s.pg_tools_dirs.map((d) => (
            <li key={d} className="mono">
              {d}{" "}
              <button type="button" aria-label={`Remove ${d}`} onClick={() => set("pg_tools_dirs", s.pg_tools_dirs.filter((x) => x !== d))}>Remove</button>
            </li>
          ))}
        </ul>
        <button type="button" onClick={() => void choose((d) => !s.pg_tools_dirs.includes(d) && set("pg_tools_dirs", [...s.pg_tools_dirs, d]))}>
          Add folder…
        </button>
        <div className="note">
          Found:
          {view.tools.length === 0 ? " none — install PostgreSQL client tools" : (
            <ul className="tool-list">{view.tools.map((t) => <li key={t} className="mono">{t}</li>)}</ul>
          )}
        </div>
      </fieldset>
      {error && <ErrorNote error={error} />}
      {saved && <p className="ok-note" role="status">Saved.</p>}
      <div className="actions">
        <span className="spacer" />
        <button type="button" onClick={() => closeTab(tab.id)}>Close</button>
        <button type="submit" className="primary">Save</button>
      </div>
    </form>
  );
}
