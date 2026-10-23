// Registers the connection UI with the shell and wires core connection events.

import { registerTab } from "../app/registry";
import { onConnEvent, onTunnelEvent } from "../lib/ipc/connections";
import { ConnectionForm } from "./ConnectionForm";
import { useConnections } from "./store";

let initialized = false;

export function initConnections() {
  if (initialized) return;
  initialized = true;
  registerTab("connection-form", ConnectionForm);
  void useConnections.getState().load();
  void onConnEvent((ev) => useConnections.getState().applyEvent(ev));
  void onTunnelEvent((t) => useConnections.getState().applyTunnel(t));
}

export { ConnectionList } from "./ConnectionList";
export { StatusBarConnection } from "./StatusBarConnection";
