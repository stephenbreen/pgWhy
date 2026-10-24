// Shared progress readout for pg_dump/pg_restore steps: step, current object, objects
// done of total ("~" when the total is an estimate), bytes written, elapsed. No percentage —
// the tools don't report a reliable one.

import type { ToolProgress } from "../lib/ipc/backup";

export function fmtBytes(n?: number | null): string {
  if (n == null) return "—";
  const u = ["B", "kB", "MB", "GB", "TB"];
  let i = 0;
  let v = n;
  while (v >= 1024 && i < u.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v < 10 && i > 0 ? v.toFixed(1) : Math.round(v)} ${u[i]}`;
}

export function fmtElapsed(ms: number): string {
  const s = Math.floor(ms / 1000);
  const m = Math.floor(s / 60);
  const h = Math.floor(m / 60);
  return h ? `${h}h ${m % 60}m ${s % 60}s` : m ? `${m}m ${s % 60}s` : `${s}s`;
}

export function ProgressLine({ p }: { p: ToolProgress }) {
  const tilde = p.total_is_estimate ? "~" : "";
  return (
    <div className="progress-line">
      {p.current_object && <span className="mono">{p.current_object}</span>}
      <span>
        {tilde}
        {p.objects_done.toLocaleString()}
        {p.objects_total != null && ` of ${tilde}${p.objects_total.toLocaleString()}`} objects
      </span>
      {p.bytes_written != null && <span>{fmtBytes(p.bytes_written)} written</span>}
      <span>{fmtElapsed(p.elapsed_ms)}</span>
      {p.warnings > 0 && <span className="warn">{p.warnings} warnings</span>}
      {p.errors > 0 && <span className="error-text">{p.errors} errors</span>}
    </div>
  );
}
