import { invoke } from "@tauri-apps/api/core";

export interface AppInfo {
  name: string;
  version: string;
}

export function appInfo(): Promise<AppInfo> {
  return invoke<AppInfo>("app_info");
}
