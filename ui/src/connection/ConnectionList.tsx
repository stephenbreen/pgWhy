// The explorer's root level: saved connections by group, recents one action away (R1).

import { useState, type ReactNode } from "react";
import type { ConnectionEntry } from "../lib/ipc/connections";
import { groupEntries, useConnections } from "./store";
import { ConnectionNode, openConnectionForm } from "./ConnectionNode";

export function ConnectionList({ renderChildren }: { renderChildren?: (entry: ConnectionEntry) => ReactNode }) {
  const entries = useConnections((s) => s.entries);
  const recents = useConnections((s) => s.recents);
  const loaded = useConnections((s) => s.loaded);
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});
  const toggle = (id: string, open?: boolean) => setExpanded((x) => ({ ...x, [id]: open ?? !x[id] }));

  if (!loaded) return <p className="empty">Loading connections…</p>;
  if (entries.length === 0) {
    return (
      <div className="empty">
        <p>No saved connections.</p>
        <button type="button" className="primary" onClick={() => openConnectionForm()}>
          New connection
        </button>
      </div>
    );
  }
  const recentEntries = recents.map((id) => entries.find((e) => e.id === id)).filter((e): e is ConnectionEntry => !!e).slice(0, 5);

  const node = (e: ConnectionEntry) => (
    <ConnectionNode key={e.id} entry={e} expanded={!!expanded[e.id]} onToggle={(open) => toggle(e.id, open)}>
      {renderChildren?.(e)}
    </ConnectionNode>
  );

  return (
    <div className="conn-list" role="tree" aria-label="Connections">
      <div className="list-toolbar">
        <button type="button" className="icon" aria-label="New connection" title="New connection" onClick={() => openConnectionForm()}>
          ＋
        </button>
      </div>
      {recentEntries.length > 0 && (
        <section aria-label="Recent connections" className="recents">
          <header className="group-header">Recent</header>
          {recentEntries.map((e) => (
            <button key={e.id} type="button" className="recent-chip" data-tag={e.env_tag} onClick={() => void useConnections.getState().connect(e.id)}>
              {e.name}
            </button>
          ))}
        </section>
      )}
      {groupEntries(entries).map(({ group, entries: es }) =>
        group === null ? (
          es.map(node)
        ) : (
          <section key={group} aria-label={group}>
            <header className="group-header">{group}</header>
            {es.map(node)}
          </section>
        ),
      )}
    </div>
  );
}
