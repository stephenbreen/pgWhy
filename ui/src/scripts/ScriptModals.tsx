// Shared modal for creating a new library script or editing an existing linked script inline,
// with live psql meta-command scanning via meta_scan.

import { useEffect, useState, type FormEvent } from "react";
import { ErrorNote } from "../connection/ErrorNote";
import { toAppError, type AppError } from "../lib/ipc/errors";
import * as ipc from "../lib/ipc/scripts";
import { useScriptLibrary } from "./store";

function normalizeNewPath(raw: string): string {
  const trimmed = raw.trim().replace(/^\/+/, "");
  if (!trimmed) return "";
  return /\.(sql|txt|md)$/i.test(trimmed) ? trimmed : `${trimmed}.sql`;
}

export function NewScriptModal({
  folders,
  defaultFolder = "",
  sqlOnly = false,
  initialContent = "-- SQL script\n",
  onCreated,
  onClose,
}: {
  folders: string[];
  defaultFolder?: string;
  sqlOnly?: boolean;
  initialContent?: string;
  onCreated: (opened: ipc.OpenedScript) => void;
  onClose: () => void;
}) {
  const [path, setPath] = useState(() => (defaultFolder ? `${defaultFolder}/` : ""));
  const [content, setContent] = useState(initialContent);
  const [metaCmds, setMetaCmds] = useState<ipc.MetaCommand[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<AppError | null>(null);

  useEffect(() => {
    const isSql = sqlOnly || !/\.(txt|md)$/i.test(path.trim());
    if (!isSql || !content.trim()) {
      setMetaCmds([]);
      return;
    }
    let cancelled = false;
    const timer = setTimeout(() => {
      void ipc
        .metaScan(content)
        .then((res) => {
          if (!cancelled) setMetaCmds(Array.isArray(res) ? res : []);
        })
        .catch(() => {
          if (!cancelled) setMetaCmds([]);
        });
    }, 150);
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, [content, path, sqlOnly]);

  const onSubmit = async (e: FormEvent) => {
    e.preventDefault();
    const target = normalizeNewPath(path);
    if (!target) return;
    if (sqlOnly && !/\.sql$/i.test(target)) {
      setError({ kind: "config", class: "fix_then_retry", message: "Workflow scripts must have a .sql extension" });
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const opened = await ipc.scriptSave(target, content);
      useScriptLibrary.getState().bump();
      onCreated(opened);
    } catch (err) {
      setError(toAppError(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="modal-backdrop" role="presentation" onClick={onClose}>
      <form
        className="modal script-modal"
        role="dialog"
        aria-modal="true"
        aria-label="New script"
        onClick={(e) => e.stopPropagation()}
        onSubmit={onSubmit}
      >
        <h2>{sqlOnly ? "New SQL script in library" : "New library script"}</h2>
        <label>
          Script path {sqlOnly ? "(.sql; folders created automatically)" : "(.sql, .md, or .txt; folders created automatically)"}
          <input
            aria-label="New script path"
            className="mono"
            value={path}
            placeholder={sqlOnly ? "post/seed_test_users.sql" : "reports/daily.sql"}
            onChange={(e) => setPath(e.target.value)}
            autoFocus
            required
          />
        </label>
        {folders.length > 0 && (
          <div className="wf-param-chips">
            <span className="muted">Folders:</span>
            {folders.map((f) => (
              <button
                key={f}
                type="button"
                className="wf-chip"
                onClick={() => {
                  const base = path.split("/").pop() ?? "";
                  setPath(`${f}/${base}`);
                }}
              >
                {f}/
              </button>
            ))}
          </div>
        )}
        <label>
          Content
          <textarea
            aria-label="New script content"
            className="mono wf-sql-textarea"
            rows={8}
            value={content}
            onChange={(e) => setContent(e.target.value)}
          />
        </label>
        {metaCmds.length > 0 && (
          <div className="warn-note" role="alert">
            <strong>psql meta-commands detected:</strong>{" "}
            {metaCmds.map((m) => `${m.command} (line ${m.line})`).join(", ")}. pgWhy runs standard SQL and cannot execute psql meta-commands in the editor or workflows.
          </div>
        )}
        {error && <ErrorNote error={error} compact />}
        <div className="modal-actions">
          <button type="button" onClick={onClose} disabled={busy}>
            Cancel
          </button>
          <button type="submit" className="primary" disabled={busy || !path.trim()}>
            Create script
          </button>
        </div>
      </form>
    </div>
  );
}

export function EditScriptModal({
  scriptId,
  onSaved,
  onClose,
}: {
  scriptId: string;
  onSaved: (info: ipc.ScriptInfo) => void;
  onClose: () => void;
}) {
  const [opened, setOpened] = useState<ipc.OpenedScript | null>(null);
  const [content, setContent] = useState("");
  const [metaCmds, setMetaCmds] = useState<ipc.MetaCommand[]>([]);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<AppError | null>(null);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    void ipc
      .scriptRead(scriptId)
      .then((res) => {
        if (!cancelled) {
          setOpened(res);
          setContent(res.file.content);
        }
      })
      .catch((err) => {
        if (!cancelled) setError(toAppError(err));
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [scriptId]);

  useEffect(() => {
    if (!content.trim()) {
      setMetaCmds([]);
      return;
    }
    let cancelled = false;
    const timer = setTimeout(() => {
      void ipc
        .metaScan(content)
        .then((res) => {
          if (!cancelled) setMetaCmds(Array.isArray(res) ? res : []);
        })
        .catch(() => {
          if (!cancelled) setMetaCmds([]);
        });
    }, 150);
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, [content]);

  const onSubmit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const info = await ipc.scriptWrite(scriptId, content);
      useScriptLibrary.getState().bump();
      onSaved(info);
    } catch (err) {
      setError(toAppError(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="modal-backdrop" role="presentation" onClick={onClose}>
      <form
        className="modal script-modal"
        role="dialog"
        aria-modal="true"
        aria-label="Edit linked script"
        onClick={(e) => e.stopPropagation()}
        onSubmit={onSubmit}
      >
        <h2>Edit {opened?.info.path ?? "script"}</h2>
        {loading ? (
          <p className="muted">Loading script…</p>
        ) : (
          <label>
            SQL content
            <textarea
              aria-label="Linked script SQL"
              className="mono wf-sql-textarea"
              rows={10}
              value={content}
              onChange={(e) => setContent(e.target.value)}
              autoFocus
            />
          </label>
        )}
        {metaCmds.length > 0 && (
          <div className="warn-note" role="alert">
            <strong>psql meta-commands detected:</strong>{" "}
            {metaCmds.map((m) => `${m.command} (line ${m.line})`).join(", ")}. Workflows cannot attach scripts with psql meta-commands.
          </div>
        )}
        {error && <ErrorNote error={error} compact />}
        <div className="modal-actions">
          <button type="button" onClick={onClose} disabled={busy}>
            Cancel
          </button>
          <button type="submit" className="primary" disabled={busy || loading}>
            Save script
          </button>
        </div>
      </form>
    </div>
  );
}
