// Always-visible status bar. The connection manager supplies the active connection
// and its environment tag; until then it reports that nothing is connected.

import type { ReactNode } from "react";

export interface StatusBarProps {
  connection?: ReactNode;
  right?: ReactNode;
}

export function StatusBar({ connection, right }: StatusBarProps) {
  return (
    <footer className="status-bar" aria-label="Status">
      <div className="status-left">{connection ?? <span className="status-muted">No active connection</span>}</div>
      <div className="status-right">{right}</div>
    </footer>
  );
}
