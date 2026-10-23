// Feature units register what renders in the sidebar for each activity view and in the main
// area for each tab kind. The shell renders a placeholder for anything not yet registered.

import type { ComponentType } from "react";
import type { ActivityView, Tab, TabKind } from "./workspace";

export type TabRenderer = ComponentType<{ tab: Tab }>;
export type ViewRenderer = ComponentType;

const tabRenderers = new Map<TabKind, TabRenderer>();
const viewRenderers = new Map<ActivityView, ViewRenderer>();

export function registerTab(kind: TabKind, renderer: TabRenderer) {
  tabRenderers.set(kind, renderer);
}

export function registerView(view: ActivityView, renderer: ViewRenderer) {
  viewRenderers.set(view, renderer);
}

export function tabRenderer(kind: TabKind): TabRenderer | undefined {
  return tabRenderers.get(kind);
}

export function viewRenderer(view: ActivityView): ViewRenderer | undefined {
  return viewRenderers.get(view);
}
