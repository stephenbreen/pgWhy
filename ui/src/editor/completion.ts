// Completion namespace for the editor (R7): object names from the explorer's catalog cache.
// Filled in by the introspection integration; until a catalog is loaded, keywords only.

import { useEffect, useState } from "react";
import type { SQLNamespace } from "@codemirror/lang-sql";

type Loader = (connectionId: string, database: string) => Promise<SQLNamespace | undefined>;
let loader: Loader | null = null;
const listeners = new Set<() => void>();

export function setCompletionLoader(l: Loader) {
  loader = l;
  listeners.forEach((f) => f());
}

/** Tell open editors the catalog changed (refresh, post-swap invalidation). */
export function completionChanged() {
  listeners.forEach((f) => f());
}

export function useCompletionSchema(connectionId: string, database: string): SQLNamespace | undefined {
  const [schema, setSchema] = useState<SQLNamespace | undefined>(undefined);
  const [tick, setTick] = useState(0);
  useEffect(() => {
    const f = () => setTick((t) => t + 1);
    listeners.add(f);
    return () => void listeners.delete(f);
  }, []);
  useEffect(() => {
    let live = true;
    loader?.(connectionId, database).then((s) => live && setSchema(s)).catch(() => {});
    return () => {
      live = false;
    };
  }, [connectionId, database, tick]);
  return schema;
}
