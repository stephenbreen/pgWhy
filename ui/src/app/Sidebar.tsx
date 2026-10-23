import { viewRenderer } from "./registry";
import { useWorkspace } from "./workspace";

const TITLES = { explorer: "Explorer", scripts: "Scripts", workflows: "Workflows" } as const;

export function Sidebar() {
  const activeView = useWorkspace((s) => s.activeView);
  const View = viewRenderer(activeView);
  return (
    <aside className="sidebar" aria-label={TITLES[activeView]}>
      <header className="pane-header">{TITLES[activeView]}</header>
      <div className="sidebar-body">{View ? <View /> : <p className="empty">Nothing here yet.</p>}</div>
    </aside>
  );
}
