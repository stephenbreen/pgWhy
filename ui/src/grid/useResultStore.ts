// Binds a RowStore to a core result set over IPC: subscribes to its binary channel, routes
// window requests, and releases the core buffer when the view goes away.
//
// Window requests wait for the subscription: a request that reached the core before the
// channel was registered would be answered nowhere and its chunk would stay "in flight".

import { useEffect, useMemo } from "react";
import { resultRelease, resultSubscribe, resultWindow } from "../lib/ipc/query";
import { decodeChunk } from "./protocol";
import { RowStore } from "./rowstore";

export function useResultStore(resultId: string, release = false): RowStore {
  const { store, ready } = useMemo(() => {
    let markReady: () => void = () => {};
    const ready = new Promise<void>((r) => (markReady = r));
    const store: RowStore = new RowStore({
      request: (start, count) => {
        const generation = store.generation;
        void ready
          .then(() => resultWindow(resultId, start, count))
          .catch(() => store.fail(start, generation));
      },
    });
    return { store, ready: { promise: ready, resolve: () => markReady() } };
  }, [resultId]);
  useEffect(() => {
    let live = true;
    void resultSubscribe(resultId, (buf) => {
      if (live) store.deliver(decodeChunk(buf));
    })
      .then(ready.resolve)
      // never subscribed (result already released): leave requests parked rather than fail loudly
      .catch(() => {});
    return () => {
      live = false;
      if (release) void resultRelease(resultId);
    };
  }, [resultId, store, ready, release]);
  return store;
}
