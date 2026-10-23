import { ActivityBar } from "./ActivityBar";
import { ErrorBoundary } from "./ErrorBoundary";
import { MainArea } from "./MainArea";
import { Sidebar } from "./Sidebar";
import { StatusBar } from "./StatusBar";
import { useWorkspace } from "./workspace";
import { StatusBarConnection } from "../connection/StatusBarConnection";
import { DetailsPanel } from "../explorer/details/DetailsPanel";
import { useDetails } from "../explorer/details/store";
import { DatabaseOps } from "../explorer/DatabaseOps";
import { RestoreDialog } from "../explorer/RestoreDialog";

export function App() {
  const sidebarVisible = useWorkspace((s) => s.sidebarVisible);
  const activeView = useWorkspace((s) => s.activeView);
  // subscribe to both halves: the status-bar affordance must appear even when the dock is
  // hidden and only the selection changed
  const detailsVisible = useDetails((s) => s.visible);
  const hasTarget = useDetails((s) => s.target !== null);
  // the details dock belongs to the explorer; it shows once something is selected
  const details = detailsVisible && hasTarget && sidebarVisible && activeView === "explorer";
  return (
    <div className="shell" data-sidebar={sidebarVisible ? "open" : "closed"} data-details={details ? "open" : "closed"}>
      <ActivityBar />
      {sidebarVisible && <Sidebar />}
      <ErrorBoundary>
        <MainArea />
      </ErrorBoundary>
      {details && <DetailsPanel />}
      {/* Shell-level dialogs: they work from every view, and render nothing when closed. */}
      <DatabaseOps />
      <RestoreDialog />
      <StatusBar
        connection={<StatusBarConnection />}
        right={
          !details && hasTarget ? (
            <button type="button" className="link" onClick={() => useDetails.getState().show(true)}>
              Show details
            </button>
          ) : null
        }
      />
    </div>
  );
}
