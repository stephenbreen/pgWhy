import { tabRenderer } from "./registry";
import { requestCloseTab, useWorkspace, type Tab } from "./workspace";
import { Welcome } from "./Welcome";

function TabContent({ tab }: { tab: Tab }) {
  if (tab.kind === "welcome") return <Welcome />;
  const Renderer = tabRenderer(tab.kind);
  return Renderer ? <Renderer tab={tab} /> : <p className="empty">No view for {tab.kind} tabs yet.</p>;
}

export function MainArea() {
  const tabs = useWorkspace((s) => s.tabs);
  const activeTabId = useWorkspace((s) => s.activeTabId);
  const activateTab = useWorkspace((s) => s.activateTab);
  const closeTab = (id: string) => void requestCloseTab(id);
  const active = tabs.find((t) => t.id === activeTabId) ?? null;

  return (
    <main className="main-area">
      <div className="tab-strip" role="tablist" aria-label="Open tabs">
        {tabs.map((t) => (
          <div
            key={t.id}
            role="tab"
            aria-selected={t.id === activeTabId}
            className="tab"
            data-kind={t.kind}
            data-tag={t.tag}
            tabIndex={t.id === activeTabId ? 0 : -1}
            onClick={() => activateTab(t.id)}
            onAuxClick={(e) => e.button === 1 && closeTab(t.id)}
          >
            {t.tag && <span className="tab-tag" aria-hidden="true" />}
            <span className="tab-title">{t.title}</span>
            <button
              type="button"
              className="tab-close"
              aria-label={`Close ${t.title}`}
              onClick={(e) => {
                e.stopPropagation();
                closeTab(t.id);
              }}
            >
              ×
            </button>
          </div>
        ))}
      </div>
      {/* Every open tab stays mounted so editor buffers, grid windows and live workflow
          run views keep their state while another tab is in front. */}
      {tabs.map((t) => (
        <section key={t.id} role="tabpanel" className="tab-panel" hidden={t.id !== active?.id} aria-label={t.title}>
          <TabContent tab={t} />
        </section>
      ))}
      {tabs.length === 0 && <p className="empty">No open tabs.</p>}
    </main>
  );
}
