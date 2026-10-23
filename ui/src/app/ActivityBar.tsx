import { useWorkspace, type ActivityView } from "./workspace";
import { openSettings } from "./Settings";
import { shortcut } from "./keys";

const VIEWS: { view: ActivityView; label: string; glyph: string; key: string }[] = [
  { view: "explorer", label: "Explorer", glyph: "▤", key: "1" },
  { view: "scripts", label: "Scripts", glyph: "▦", key: "2" },
  { view: "workflows", label: "Workflows", glyph: "⟳", key: "3" },
];

export function ActivityBar() {
  const activeView = useWorkspace((s) => s.activeView);
  const sidebarVisible = useWorkspace((s) => s.sidebarVisible);
  const setView = useWorkspace((s) => s.setView);
  return (
    <nav className="activity-bar" aria-label="Views">
      {VIEWS.map(({ view, label, glyph, key }) => (
        <button
          key={view}
          type="button"
          className="activity-item"
          aria-label={label}
          aria-pressed={sidebarVisible && activeView === view}
          title={`${label} (${shortcut(key)})`}
          onClick={() => setView(view)}
        >
          <span aria-hidden="true">{glyph}</span>
        </button>
      ))}
      <span className="activity-spacer" />
      <button type="button" className="activity-item" aria-label="Settings" title="Settings" onClick={openSettings}>
        <span aria-hidden="true">⚙</span>
      </button>
    </nav>
  );
}
