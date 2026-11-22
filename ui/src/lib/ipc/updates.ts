// Typed wrappers over src-tauri/src/commands/updates.rs.

import { Channel, invoke } from "@tauri-apps/api/core";

export interface UpdateInfo {
  version: string;
  current_version: string;
  notes?: string | null;
}

export interface UpdateProgress {
  downloaded: number;
  total?: number | null;
}

export const updateCheck = (automatic: boolean) => invoke<UpdateInfo | null>("update_check", { automatic });

export function updateDownload(onProgress: (p: UpdateProgress) => void) {
  const events = new Channel<UpdateProgress>();
  events.onmessage = onProgress;
  return invoke<void>("update_download", { events });
}

export const updateRestart = () => invoke<void>("update_restart");
