import { registerTab, registerView } from "../app/registry";
import { ScriptsView } from "./ScriptsView";
import { TextTab } from "./TextTab";

export function initScripts() {
  registerView("scripts", ScriptsView);
  registerTab("text", TextTab);
}
