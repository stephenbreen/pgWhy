// App updates: the startup check, the status-bar notice, and the section at the top of Settings.
//
// Installing goes download, then the same prompts as quitting (open transactions, pending edits),
// then restart, so a failed download or a change of heart leaves every tab as it was.

import { create } from "zustand";
import { ErrorNote } from "../connection/ErrorNote";
import { toAppError, type AppError } from "../lib/ipc/errors";
import * as u from "../lib/ipc/updates";
import { fmtBytes } from "../workflow/progress";
import { requestCloseAll } from "./workspace";

type Phase = "idle" | "checking" | "up_to_date" | "downloading" | "ready" | "restarting";

interface UpdatesState {
  phase: Phase;
  available: u.UpdateInfo | null;
  progress: u.UpdateProgress | null;
  error: AppError | null;
  check: (automatic: boolean) => Promise<void>;
  install: () => Promise<void>;
}

export const useUpdates = create<UpdatesState>((set, get) => ({
  phase: "idle",
  available: null,
  progress: null,
  error: null,

  check: async (automatic) => {
    set({ phase: "checking", error: null });
    try {
      const available = await u.updateCheck(automatic);
      if (available) set({ phase: "idle", available });
      else set({ phase: automatic ? "idle" : "up_to_date", available: null });
    } catch (e) {
      // being offline at startup is nobody's problem; only a check the user asked for reports it
      set({ phase: "idle", error: automatic ? null : toAppError(e) });
    }
  },

  install: async () => {
    set({ error: null });
    try {
      if (get().phase !== "ready") {
        set({ phase: "downloading", progress: null });
        await u.updateDownload((progress) => set({ progress }));
        set({ phase: "ready" });
      }
      if (!(await requestCloseAll())) return;
      set({ phase: "restarting" });
      await u.updateRestart();
    } catch (e) {
      // a refused restart keeps the download, so trying again doesn't fetch it twice
      set((s) => ({
        phase: s.phase === "downloading" ? "idle" : "ready",
        error: toAppError(e),
      }));
    }
  },
}));

export function UpdatesSection({ currentVersion }: { currentVersion?: string }) {
  const { phase, available, progress, error, check, install } = useUpdates();
  const busy = phase === "checking" || phase === "downloading" || phase === "restarting";
  return (
    <fieldset>
      <legend>Updates</legend>
      {available ? (
        <>
          <p role="status">
            pgWhy {available.version} is available. You have {available.current_version}.
          </p>
          {available.notes && <pre className="update-notes">{available.notes}</pre>}
          <div className="row">
            <button type="button" className="primary" disabled={busy} onClick={() => void install()}>
              {phase === "ready" ? "Restart to update" : "Update and restart"}
            </button>
          </div>
        </>
      ) : (
        <div className="row">
          <button type="button" disabled={busy} onClick={() => void check(false)}>
            Check for updates
          </button>
          {currentVersion && <span className="note">You have {currentVersion}.</span>}
        </div>
      )}
      {phase === "checking" && <p className="note">Checking…</p>}
      {phase === "up_to_date" && <p className="note" role="status">pgWhy is up to date.</p>}
      {phase === "downloading" && (
        <p className="note" role="status">
          Downloading{progress && ` ${fmtBytes(progress.downloaded)}`}
          {progress?.total ? ` of ${fmtBytes(progress.total)}` : ""}…
        </p>
      )}
      {phase === "restarting" && <p className="note" role="status">Restarting…</p>}
      {error && <ErrorNote error={error} />}
    </fieldset>
  );
}
