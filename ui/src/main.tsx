import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./app/App";
import { initConnections } from "./connection";
import { initExplorer } from "./explorer";
import { initEditor } from "./editor";
import { initScripts } from "./scripts";
import { initGrid } from "./grid";
import { initWorkflow } from "./workflow";
import { initQuitHandler } from "./app/quit";
import { initShortcuts } from "./app/shortcuts";
import { registerTab } from "./app/registry";
import { SettingsTab } from "./app/Settings";
import "./app/theme.css";
import "./app/components.css";

// e2e builds (vite --mode e2e) load the WebdriverIO bridge; never present in release builds
if (import.meta.env.MODE === "e2e") await import("@wdio/tauri-plugin");

initConnections();
initExplorer();
initEditor();
initScripts();
initGrid();
initWorkflow();
initQuitHandler();
initShortcuts();
registerTab("settings", SettingsTab);

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
