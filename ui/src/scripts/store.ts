// Lightweight version signal so ScriptsView, EditorTab, TextTab, and WorkflowEditor
// stay in sync whenever scripts are created, edited, renamed, re-tagged, or deleted.

import { create } from "zustand";

interface ScriptLibraryState {
  version: number;
  bump: () => void;
}

export const useScriptLibrary = create<ScriptLibraryState>((set) => ({
  version: 0,
  bump: () => set((s) => ({ version: s.version + 1 })),
}));
