// Shell-level UI state: which activity view is showing and which main-area tabs are open.
// Feature units add their own tab kinds; the shell only knows how to host them.

import { create } from "zustand";

export type ActivityView = "explorer" | "scripts" | "workflows";

export type TabKind = "welcome" | "editor" | "grid" | "workflow-run" | "connection-form" | "details" | "workflow-editor" | "backup" | "settings" | "workflow-history" | "history" | "operation" | "text";

export interface Tab {
  id: string;
  kind: TabKind;
  title: string;
  /** connection entry the tab is bound to; absent for app-level tabs */
  connectionId?: string;
  /** env tag of the bound connection (colour bar on the tab) */
  tag?: "local" | "dev" | "staging" | "production";
  /** kind-specific payload owned by the feature that opened the tab */
  data?: unknown;
}

/** A tab can veto/postpone closing (e.g. an editor with an open transaction asks first). */
export type CloseGuard = () => Promise<boolean>;
const closeGuards = new Map<string, CloseGuard>();

export function setCloseGuard(tabId: string, guard: CloseGuard | null) {
  if (guard) closeGuards.set(tabId, guard);
  else closeGuards.delete(tabId);
}

/** Close a tab after its guard (if any) agrees. */
export async function requestCloseTab(tabId: string): Promise<boolean> {
  const guard = closeGuards.get(tabId);
  if (guard && !(await guard())) return false;
  closeGuards.delete(tabId);
  useWorkspace.getState().closeTab(tabId);
  return true;
}

/** Ask every guarded tab (quit / window close). */
export async function requestCloseAll(): Promise<boolean> {
  for (const [id, guard] of [...closeGuards]) {
    useWorkspace.getState().activateTab(id);
    if (!(await guard())) return false;
  }
  return true;
}

interface WorkspaceState {
  activeView: ActivityView;
  sidebarVisible: boolean;
  tabs: Tab[];
  activeTabId: string | null;
  setView: (view: ActivityView) => void;
  /** bring a view into the sidebar, never toggling it away (keyboard shortcuts) */
  showView: (view: ActivityView) => void;
  openTab: (tab: Tab) => void;
  closeTab: (id: string) => void;
  activateTab: (id: string) => void;
  updateTab: (id: string, patch: Partial<Omit<Tab, "id">>) => void;
}

const WELCOME: Tab = { id: "welcome", kind: "welcome", title: "Welcome" };

export const useWorkspace = create<WorkspaceState>((set) => ({
  activeView: "explorer",
  sidebarVisible: true,
  tabs: [WELCOME],
  activeTabId: WELCOME.id,

  // clicking the active view's icon toggles the sidebar, VS Code style
  setView: (view) =>
    set((s) =>
      s.activeView === view
        ? { sidebarVisible: !s.sidebarVisible }
        : { activeView: view, sidebarVisible: true },
    ),

  showView: (view) => set({ activeView: view, sidebarVisible: true }),

  // opening an already-open tab id just focuses it (workflow run views are re-entrant)
  openTab: (tab) =>
    set((s) =>
      s.tabs.some((t) => t.id === tab.id)
        ? { activeTabId: tab.id }
        : { tabs: [...s.tabs, tab], activeTabId: tab.id },
    ),

  closeTab: (id) =>
    set((s) => {
      const idx = s.tabs.findIndex((t) => t.id === id);
      if (idx < 0) return s;
      const tabs = s.tabs.filter((t) => t.id !== id);
      let activeTabId = s.activeTabId;
      if (activeTabId === id) activeTabId = (tabs[idx] ?? tabs[idx - 1])?.id ?? null;
      return { tabs, activeTabId };
    }),

  activateTab: (id) => set({ activeTabId: id }),

  updateTab: (id, patch) =>
    set((s) => ({ tabs: s.tabs.map((t) => (t.id === id ? { ...t, ...patch } : t)) })),
}));
