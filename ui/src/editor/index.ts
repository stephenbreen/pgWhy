import { registerTab } from "../app/registry";
import { onConnEvent } from "../lib/ipc/connections";
import { EditorTab } from "./EditorTab";
import { useEditors } from "./store";

let wired = false;

export function initEditor() {
  registerTab("editor", EditorTab);
  if (wired) return;
  wired = true;
  // a tab's connection closed by the core (Refresh swap quiesce, server kill): the tab shows
  // Disconnected with its text kept and a one-click reconnect — never silently reconnected
  void onConnEvent((ev) => {
    if (ev.event !== "closed" || ev.purpose !== "editor_tab") return;
    const { tabs, patch, apply } = useEditors.getState();
    for (const [id, t] of Object.entries(tabs)) {
      if (t.handle !== ev.handle) continue;
      const hadTxn = t.txn === "in_transaction" || t.txn === "failed_transaction";
      patch(id, { connected: false, txn: "disconnected", handle: undefined });
      const why = ev.close.reason === "quiesced" ? "Closed by a Refresh swap of this database" : ev.close.reason === "lost" ? "Connection lost" : "Disconnected";
      apply(id, { type: "notice", index: null, notice: { severity: "INFO", code: "", message: `${why}${hadTxn ? " — the open transaction was rolled back" : ""}` } }, "");
    }
  });
}

export { openEditorTab } from "./EditorTab";
