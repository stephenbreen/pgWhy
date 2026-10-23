// Binary window protocol between the core result buffer (crates/pgwhy-core/src/query/results.rs)
// and the grid. Rows live in the core; the grid asks for [start, start+count) windows and the
// core answers on the result set's Channel with one chunk per window.
//
// Chunk layout (little-endian):
//   u32 start, u32 count, u16 ncols
//   u8  row_flags[count]                      0 = clean, 1 = updated, 2 = inserted, 3 = deleted
//   per column:
//     u8  null_bits[ceil(count/8)]            bit i set = row i is NULL
//     u8  pending_bits[ceil(count/8)]         bit i set = cell holds a pending (uncommitted) value
//     u32 blob_len, u8 blob[blob_len]         UTF-8 text of all non-null values, concatenated
//     u32 ends[count]                         end offset of row i's value in blob (bytes)
//
// Every value arrives as text (the executor uses the simple-query protocol), so the grid
// formats nothing — it shows exactly what Postgres printed.

export const ROW_CLEAN = 0;
export const ROW_UPDATED = 1;
export const ROW_INSERTED = 2;
export const ROW_DELETED = 3;

export interface Chunk {
  start: number;
  count: number;
  rowFlags: Uint8Array;
  /** cells[col][row] — null for SQL NULL */
  cells: (string | null)[][];
  /** pending[col] bitset, same layout as the wire */
  pending: Uint8Array[];
  bytes: number;
}

const td = new TextDecoder("utf-8");

export function decodeChunk(buf: ArrayBuffer): Chunk {
  const dv = new DataView(buf);
  let p = 0;
  const start = dv.getUint32(p, true); p += 4;
  const count = dv.getUint32(p, true); p += 4;
  const ncols = dv.getUint16(p, true); p += 2;
  const rowFlags = new Uint8Array(buf, p, count).slice(); p += count;
  const bitsLen = Math.ceil(count / 8);
  const cells: (string | null)[][] = [];
  const pending: Uint8Array[] = [];
  for (let c = 0; c < ncols; c++) {
    const nulls = new Uint8Array(buf, p, bitsLen); p += bitsLen;
    pending.push(new Uint8Array(buf, p, bitsLen).slice()); p += bitsLen;
    const blobLen = dv.getUint32(p, true); p += 4;
    const blob = td.decode(new Uint8Array(buf, p, blobLen));
    const blobBytes = new Uint8Array(buf, p, blobLen);
    p += blobLen;
    const col: (string | null)[] = new Array(count);
    const ascii = blob.length === blobLen; // byte offsets == string offsets
    let prevEnd = 0;
    for (let i = 0; i < count; i++) {
      const end = dv.getUint32(p, true); p += 4;
      if (nulls[i >> 3] & (1 << (i & 7))) col[i] = null;
      else col[i] = ascii ? blob.substring(prevEnd, end) : td.decode(blobBytes.subarray(prevEnd, end));
      prevEnd = end;
    }
    cells.push(col);
  }
  return { start, count, rowFlags, cells, pending, bytes: buf.byteLength };
}

/** Test/fixture helper: encode a chunk exactly as the core does. */
export function encodeChunk(start: number, rows: (string | null)[][], ncols: number, rowFlags?: number[], pendingCells?: [number, number][]): ArrayBuffer {
  const te = new TextEncoder();
  const count = rows.length;
  const bitsLen = Math.ceil(count / 8);
  const parts: Uint8Array[] = [];
  const head = new DataView(new ArrayBuffer(10));
  head.setUint32(0, start, true);
  head.setUint32(4, count, true);
  head.setUint16(8, ncols, true);
  parts.push(new Uint8Array(head.buffer));
  parts.push(Uint8Array.from(rowFlags ?? new Array(count).fill(0)));
  for (let c = 0; c < ncols; c++) {
    const nulls = new Uint8Array(bitsLen);
    const pend = new Uint8Array(bitsLen);
    const blobs: Uint8Array[] = [];
    const ends = new DataView(new ArrayBuffer(4 * count));
    let off = 0;
    for (let i = 0; i < count; i++) {
      const v = rows[i][c];
      if (v === null) nulls[i >> 3] |= 1 << (i & 7);
      else {
        const b = te.encode(v);
        blobs.push(b);
        off += b.length;
      }
      if (pendingCells?.some(([r, cc]) => r === i && cc === c)) pend[i >> 3] |= 1 << (i & 7);
      ends.setUint32(4 * i, off, true);
    }
    const len = new DataView(new ArrayBuffer(4));
    len.setUint32(0, off, true);
    parts.push(nulls, pend, new Uint8Array(len.buffer), ...blobs, new Uint8Array(ends.buffer));
  }
  const total = parts.reduce((n, x) => n + x.length, 0);
  const out = new Uint8Array(total);
  let p = 0;
  for (const x of parts) {
    out.set(x, p);
    p += x.length;
  }
  return out.buffer;
}

export function isPending(chunk: Chunk, col: number, row: number): boolean {
  return !!(chunk.pending[col][row >> 3] & (1 << (row & 7)));
}
