import { describe, expect, it } from "vitest";
import { decodeChunk, encodeChunk } from "./protocol";
import { RowStore, WINDOW } from "./rowstore";

const rows = (n: number, tag: string) => Array.from({ length: n }, (_, i) => [`${tag}${i}`]);
const chunk = (start: number, n: number, tag: string) => decodeChunk(encodeChunk(start, rows(n, tag), 1));

function manual() {
  const requests: [number, number][] = [];
  const store = new RowStore({ request: (s, n) => void requests.push([s, n]) });
  return { store, requests };
}

describe("row store races", () => {
  it("drops a window reply that was in flight when the store was invalidated", () => {
    const { store, requests } = manual();
    store.setRowCount(100);
    store.ensureRange(0, 50);
    expect(requests).toEqual([[0, 100]]);

    store.invalidate(); // e.g. a committed edit changed the rows
    store.ensureRange(0, 50);
    expect(requests).toEqual([[0, 100], [0, 100]]);

    store.deliver(chunk(0, 100, "old")); // answer to the first request
    expect(store.cell(0, 0)).toBeUndefined();
    store.deliver(chunk(0, 100, "new")); // answer to the second
    expect(store.cell(0, 0)).toBe("new0");
  });

  it("a failed pre-invalidate request does not swallow the fresh reply", () => {
    const { store } = manual();
    store.setRowCount(10);
    store.ensureRange(0, 10);
    const gen = store.generation;
    store.invalidate();
    store.ensureRange(0, 10);
    store.fail(0, gen); // the old request errored: no stale reply is coming
    store.deliver(chunk(0, 10, "new"));
    expect(store.cell(0, 0)).toBe("new0");
  });

  it("refetches a partial chunk that was fetched at a smaller row count", () => {
    const { store, requests } = manual();
    store.setRowCount(100);
    store.ensureRange(0, 100);
    store.setRowCount(WINDOW + 50); // the result grew while the request was out
    store.deliver(chunk(0, 100, "r")); // short: asked for 100 rows
    expect(store.cell(150, 0)).toBeUndefined();

    store.ensureRange(0, WINDOW + 50);
    expect(requests).toContainEqual([0, WINDOW]);
    store.deliver(chunk(0, WINDOW, "r"));
    expect(store.cell(150, 0)).toBe("r150");

    // and does not loop once the chunk is complete
    const n = requests.length;
    store.ensureRange(0, WINDOW + 50);
    expect(requests.length).toBe(n);
  });

  it("does not refetch a short chunk again at the same row count", () => {
    const { store, requests } = manual();
    store.setRowCount(WINDOW);
    store.ensureRange(0, WINDOW);
    store.deliver(chunk(0, 10, "r")); // the core answered short
    store.ensureRange(0, WINDOW);
    store.deliver(chunk(0, 10, "r"));
    const n = requests.length;
    store.ensureRange(0, WINDOW);
    expect(requests.length).toBe(n);
  });
});
