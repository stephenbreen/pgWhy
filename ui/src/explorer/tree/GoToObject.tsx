// ⌘P / Ctrl+P (Find object): find any table/view/function in the active connection's database by name,
// then expand the tree down to it and select it (R3 go-to-object).

import { useEffect, useState } from "react";
import { useConnections } from "../../connection/store";
import * as ex from "../../lib/ipc/explorer";
import { notifySelect } from "../actions";
import { dbPath, folderFor, folderPath, relPath, routinePath, schemaPath, SEP, useTrees } from "./model";

export function GoToObject({ onClose }: { onClose: () => void }) {
  const active = useConnections((s) => s.entries.find((e) => e.id === s.activeId && s.status[e.id]?.state === "connected"));
  const [q, setQ] = useState("");
  const [hits, setHits] = useState<ex.SearchHit[]>([]);
  const [cursor, setCursor] = useState(0);

  useEffect(() => {
    if (!active || !q.trim()) return setHits([]);
    let live = true;
    const t = setTimeout(() => {
      ex.searchObjects(active.id, active.database, q.trim())
        .then((h) => live && (setHits(h.slice(0, 50)), setCursor(0)))
        .catch(() => live && setHits([]));
    }, 120);
    return () => {
      live = false;
      clearTimeout(t);
    };
  }, [q, active]);

  const go = async (h: ex.SearchHit) => {
    if (!active) return;
    const db = active.database;
    const trees = useTrees.getState();
    const isRoutine = ["function", "procedure", "aggregate", "window"].includes(h.kind);
    for (const id of [dbPath(db), schemaPath(db, h.schema), folderPath(db, h.schema, folderFor(h.kind))]) trees.toggle(active.id, id, true);
    await trees.loadContents(active.id, db, h.schema);
    const id = isRoutine ? routinePath(db, h.schema, h.name, h.identity_args ?? "") : relPath(db, h.schema, h.name);
    trees.select(active.id, id);
    const contents = useTrees.getState().trees[active.id]?.contents[`${db}${SEP}${h.schema}`];
    const rel = contents?.state === "loaded" ? contents.value.relations.find((r) => r.name === h.name) : undefined;
    if (rel) notifySelect({ type: "relation", connectionId: active.id, db, rel });
    onClose();
    requestAnimationFrame(() => document.querySelector<HTMLElement>('.explorer [role="treeitem"][aria-selected="true"]')?.scrollIntoView?.({ block: "center" }));
  };

  return (
    <div className="goto" role="dialog" aria-label="Find object">
      <input
        autoFocus
        aria-label="Find object"
        placeholder={active ? `Find object in ${active.name}/${active.database}…` : "Connect first"}
        disabled={!active}
        value={q}
        onChange={(e) => setQ(e.target.value)}
        onBlur={() => setTimeout(onClose, 150)}
        onKeyDown={(e) => {
          if (e.key === "Escape") onClose();
          else if (e.key === "ArrowDown") setCursor((c) => Math.min(c + 1, hits.length - 1));
          else if (e.key === "ArrowUp") setCursor((c) => Math.max(c - 1, 0));
          else if (e.key === "Enter" && hits[cursor]) void go(hits[cursor]);
        }}
      />
      {hits.length > 0 && (
        <ul role="listbox" aria-label="Matches">
          {hits.map((h, i) => (
            <li key={`${h.schema}.${h.name}.${i}`} role="option" aria-selected={i === cursor} onMouseDown={() => void go(h)}>
              <span className="node-label">{h.name}</span>
              <span className="node-meta">
                {h.schema} · {h.kind.replace("_", " ")}
              </span>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
