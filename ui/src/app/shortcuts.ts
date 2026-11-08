// App-wide shortcuts (SSMS muscle memory): Ctrl/Cmd+N new query on the active connection,
// Ctrl/Cmd+W close the active tab (asking first when it holds a transaction).
// Explorer: ⌘P / Ctrl+P finds any object; ⇧⌘P / Ctrl+Shift+P focuses the tree filter.
// Both bring the Explorer into view first, from wherever focus is.
// ⌘1 / ⌘2 / ⌘3 (Ctrl+1/2/3) open the Explorer, Scripts and Workflows views.
// ⌘= / ⌘- / ⌘0 (Ctrl+= / Ctrl+- / Ctrl+0) zoom in, out and back to 100%.

import { useConnections } from "../connection/store";
import { openEditorTab } from "../editor/EditorTab";
import { requestCloseTab, useWorkspace, type ActivityView } from "./workspace";
import { useLayout } from "./layout";
import { FILTER_OBJECTS_EVENT, FIND_OBJECT_EVENT, primaryMod } from "./keys";

const VIEW_SHORTCUTS: Record<string, ActivityView> = { "1": "explorer", "2": "scripts", "3": "workflows" };
// by physical key, so "=" works with or without Shift and on layouts that move "+" and "-"
const ZOOM_KEYS: Record<string, -1 | 0 | 1> = { Equal: 1, NumpadAdd: 1, Minus: -1, NumpadSubtract: -1, Digit0: 0, Numpad0: 0 };

export function initShortcuts() {
  window.addEventListener("keydown", (e) => {
    // match the physical digit key so layouts that shift digits (AZERTY) still work
    const digit = e.code.startsWith("Digit") ? e.code.slice(5) : e.key;
    if (primaryMod(e) && !e.altKey && !e.shiftKey && VIEW_SHORTCUTS[digit]) {
      e.preventDefault();
      useWorkspace.getState().showView(VIEW_SHORTCUTS[digit]);
      return;
    }
    if (primaryMod(e) && !e.altKey && e.code in ZOOM_KEYS) {
      e.preventDefault();
      useLayout.getState().stepZoom(ZOOM_KEYS[e.code]);
      return;
    }
    if (primaryMod(e) && !e.altKey && e.key.toLowerCase() === "p") {
      e.preventDefault();
      const ws = useWorkspace.getState();
      if (ws.activeView !== "explorer" || !ws.sidebarVisible) useWorkspace.setState({ activeView: "explorer", sidebarVisible: true });
      // let the Explorer mount before it handles the command
      setTimeout(() => window.dispatchEvent(new Event(e.shiftKey ? FILTER_OBJECTS_EVENT : FIND_OBJECT_EVENT)), 0);
      return;
    }
    const mod = e.metaKey || e.ctrlKey;
    if (!mod || e.altKey) return;
    const key = e.key.toLowerCase();
    if (key === "n" && !e.shiftKey) {
      const { activeId, entries } = useConnections.getState();
      const entry = entries.find((x) => x.id === activeId);
      if (entry) {
        e.preventDefault();
        openEditorTab(entry.id, entry.database);
      }
    } else if (key === "w") {
      const id = useWorkspace.getState().activeTabId;
      if (id) {
        e.preventDefault();
        void requestCloseTab(id);
      }
    }
  });
}
