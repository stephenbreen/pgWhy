import { viewRenderer } from "./registry";
import { useWorkspace } from "./workspace";
import { SIDEBAR_W, maxPaneWidth, useLayout } from "./layout";
import { Splitter } from "./Splitter";

const TITLES = { explorer: "Explorer", scripts: "Scripts", workflows: "Workflows" } as const;

export function Sidebar({ detailsOpen = false }: { detailsOpen?: boolean }) {
  const activeView = useWorkspace((s) => s.activeView);
  const width = useLayout((s) => s.sidebarWidth);
  const setWidth = useLayout((s) => s.setSidebarWidth);
  const View = viewRenderer(activeView);
  return (
    <aside className="sidebar" aria-label={TITLES[activeView]}>
      <header className="pane-header">{TITLES[activeView]}</header>
      <div className="sidebar-body">{View ? <View /> : <p className="empty">Nothing here yet.</p>}</div>
      <Splitter
        label="Resize sidebar"
        edge="right"
        width={width}
        min={SIDEBAR_W.min}
        max={() => maxPaneWidth(SIDEBAR_W, detailsOpen ? useLayout.getState().detailsWidth : 0)}
        defaultWidth={SIDEBAR_W.default}
        onResize={setWidth}
      />
    </aside>
  );
}
