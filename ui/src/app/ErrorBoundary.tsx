// Shell-level error boundary: a throw while rendering a tab must not blank the whole window.
//
// The sidebar, activity bar and status bar stay mounted outside the boundary, so the user can
// still switch tabs, reconnect or quit after a render failure.

import { Component, type ReactNode } from "react";
import { toAppError } from "../lib/ipc/errors";

interface Props {
  children: ReactNode;
}

interface State {
  error: Error | null;
}

export class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  render() {
    const { error } = this.state;
    if (!error) return this.props.children;
    const app = toAppError(error);
    return (
      <div className="error-boundary" role="alert">
        <h2>This view stopped rendering</h2>
        <p>{app.message}</p>
        {app.detail && <pre>{app.detail}</pre>}
        <p className="note">
          The rest of the app is still running — close or switch the tab, and reopen it. Your saved
          connections, scripts and run history are unaffected.
        </p>
        <button type="button" onClick={() => this.setState({ error: null })}>
          Try rendering again
        </button>
      </div>
    );
  }
}
