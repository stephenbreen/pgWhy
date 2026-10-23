// Windowed row cache: holds only the visible window plus a prefetch margin (U2 decision),
// chunk-aligned, with a bounded number of in-flight window requests.

import type { Chunk } from "./protocol";

export const WINDOW = 256;

export interface WindowSource {
  /** ask the core for rows [start, start+count); the answer arrives via `deliver` */
  request(start: number, count: number): void;
}

export class RowStore {
  private chunks = new Map<number, Chunk>();
  private inflight = new Set<number>();
  /**
   * Bumped by `invalidate()`. Replies carry no request id, so a reply to a request made before
   * an invalidate is recognised by count instead: `stale` is how many such replies are still
   * owed for a window start, and `deliver` drops that many before accepting one.
   */
  generation = 0;
  private stale = new Map<number, number>();
  /** short chunks already refetched at this row count (stops a refetch loop) */
  private refetched = new Map<number, number>();
  private wanted: number[] = [];
  private listeners = new Set<() => void>();
  /** Bumped on every change. The grid subscribes to re-ensure the visible window. */
  revision = 0;
  /** Per-window revisions: a row only re-renders when its own window arrives. */
  private chunkRevs = new Map<number, number>();
  /** known row count; requests are clamped to it */
  rowCount = 0;
  inflightCap = 8;
  behind = 1;
  ahead = 3;

  constructor(private source: WindowSource) {}

  /** Subscribe to any change (React's useSyncExternalStore contract). */
  subscribe = (fn: () => void): (() => void) => {
    this.listeners.add(fn);
    return () => {
      this.listeners.delete(fn);
    };
  };

  /** The snapshot one row's content depends on: its window's revision. */
  versionOf = (row: number): number => this.chunkRevs.get(Math.floor(row / WINDOW) * WINDOW) ?? 0;

  setRowCount(n: number) {
    if (n === this.rowCount) return;
    // the last partial chunk may have grown while streaming: refetch it
    const lastChunk = Math.floor(Math.max(0, this.rowCount - 1) / WINDOW) * WINDOW;
    const c = this.chunks.get(lastChunk);
    if (c && c.count < WINDOW) {
      this.chunks.delete(lastChunk);
      // bump so the rows that lost their window show the loading state again
      this.bumpChunk(lastChunk);
    }
    this.rowCount = n;
  }

  /** Drop everything (re-query, committed edits, pending edits changed). */
  invalidate() {
    this.generation++;
    // every request still out will be answered with data from before this call
    for (const start of this.inflight) this.stale.set(start, (this.stale.get(start) ?? 0) + 1);
    this.chunks.clear();
    this.refetched.clear();
    this.inflight.clear();
    this.wanted = [];
    this.chunkRevs.clear();
    this.bump();
  }

  ensureRange(startRow: number, endRow: number) {
    startRow = Math.max(0, startRow);
    endRow = Math.min(endRow, this.rowCount);
    if (endRow <= startRow) return;
    const first = Math.floor(startRow / WINDOW);
    const last = Math.floor((endRow - 1) / WINDOW);
    this.evict(Math.floor((first + last) / 2));
    for (let c = first; c <= last; c++) {
      const cs = c * WINDOW;
      if (this.complete(cs) || this.inflight.has(cs) || this.wanted.includes(cs)) continue;
      // a chunk fetched when the result had fewer rows stays short after the count grows
      if (this.chunks.has(cs)) {
        if (this.refetched.get(cs) === this.rowCount) continue;
        this.refetched.set(cs, this.rowCount);
      }
      this.wanted.push(cs);
    }
    this.flush();
  }

  private flush() {
    while (this.inflight.size < this.inflightCap && this.wanted.length) {
      const cs = this.wanted.shift()!;
      this.inflight.add(cs);
      this.source.request(cs, Math.min(WINDOW, this.rowCount - cs));
    }
  }

  /** Whether the cached chunk at `cs` holds every row it should at the current row count. */
  private complete(cs: number): boolean {
    const c = this.chunks.get(cs);
    return !!c && c.count >= Math.min(WINDOW, this.rowCount - cs);
  }

  /**
   * A window request failed: forget it so the next ensureRange asks again. `generation` is the
   * store's generation when the request was made; a failure from before an invalidate only
   * cancels the stale reply it would have been.
   */
  fail(start: number, generation = this.generation) {
    if (generation !== this.generation) {
      this.dropStale(start);
    } else {
      this.inflight.delete(start);
    }
    this.flush();
  }

  private dropStale(start: number): boolean {
    const n = this.stale.get(start) ?? 0;
    if (n === 0) return false;
    if (n === 1) this.stale.delete(start);
    else this.stale.set(start, n - 1);
    return true;
  }

  deliver(chunk: Chunk) {
    // an answer to a request made before invalidate(): its rows may predate the change
    if (this.dropStale(chunk.start)) return;
    this.inflight.delete(chunk.start);
    this.chunks.set(chunk.start, chunk);
    this.bumpChunk(chunk.start);
    this.flush();
    this.bump();
  }

  private bumpChunk(start: number) {
    this.chunkRevs.set(start, (this.chunkRevs.get(start) ?? 0) + 1);
  }

  private bump() {
    this.revision++;
    for (const l of [...this.listeners]) l();
  }

  private evict(center: number) {
    if (this.chunks.size <= this.behind + this.ahead + 4) return;
    for (const cs of this.chunks.keys()) {
      const idx = cs / WINDOW;
      if (idx < center - this.behind - 2 || idx > center + this.ahead + 2) this.chunks.delete(cs);
    }
  }

  chunkFor(row: number): Chunk | undefined {
    return this.chunks.get(Math.floor(row / WINDOW) * WINDOW);
  }

  /** undefined = not loaded yet; null = SQL NULL */
  cell(row: number, col: number): string | null | undefined {
    const c = this.chunkFor(row);
    if (!c || row - c.start >= c.count) return undefined;
    return c.cells[col][row - c.start];
  }

  rowFlag(row: number): number {
    const c = this.chunkFor(row);
    return c && row - c.start < c.count ? c.rowFlags[row - c.start] : 0;
  }

  pending(row: number, col: number): boolean {
    const c = this.chunkFor(row);
    if (!c) return false;
    const i = row - c.start;
    return !!(c.pending[col][i >> 3] & (1 << (i & 7)));
  }

  residentRows(): number {
    let n = 0;
    for (const c of this.chunks.values()) n += c.count;
    return n;
  }
}
