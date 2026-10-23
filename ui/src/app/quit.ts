// Quit flow (R19): the native close is intercepted; every tab with an open transaction asks
// (commit / roll back / keep open) before the app exits.

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { requestCloseAll } from "./workspace";

export function initQuitHandler() {
  void listen("close-requested", async () => {
    // acknowledge at once: a second close request with this one unacknowledged makes the
    // backend conclude the page is gone and quit by itself (asking natively about open
    // transactions)
    await invoke("close_ack").catch(() => {});
    if (await requestCloseAll()) await invoke("app_exit");
    else await invoke("close_abort").catch(() => {});
  });
}
