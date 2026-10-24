// CSV/JSON export, run core-side from the buffer.

import { useState } from "react";
import { pickSavePath, resultExport, type ExportFormat } from "../lib/ipc/query";
import { toAppError } from "../lib/ipc/errors";

export function ExportButton({ resultId, disabled }: { resultId: string; disabled?: boolean }) {
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<string | null>(null);
  // on by default: exported values can be attacker-controlled (CSV formula injection)
  const [excelSafe, setExcelSafe] = useState(true);
  const run = async (format: ExportFormat) => {
    const path = await pickSavePath(`result.${format}`, format);
    if (!path) return;
    setBusy(true);
    setNote(null);
    try {
      const r = await resultExport(resultId, {
        format,
        path,
        header: true,
        escape_formulas: format === "csv" ? excelSafe : undefined,
      });
      const extra = [r.partial && "partial result", r.pending_edits_included > 0 && `includes ${r.pending_edits_included} pending edits`].filter(Boolean).join(", ");
      setNote(`Exported ${r.rows.toLocaleString()} rows${extra ? ` (${extra})` : ""}`);
    } catch (e) {
      setNote(toAppError(e).message);
    } finally {
      setBusy(false);
    }
  };
  return (
    <span className="export">
      {note && <span className="export-note">{note}</span>}
      <label className="check" title="Prefix cells starting with = + - @ with an apostrophe so a spreadsheet shows rather than evaluates them">
        <input type="checkbox" checked={excelSafe} disabled={busy} onChange={(e) => setExcelSafe(e.target.checked)} />
        Excel-safe
      </label>
      <button type="button" disabled={disabled || busy} onClick={() => void run("csv")}>
        Export CSV
      </button>
      <button type="button" disabled={disabled || busy} onClick={() => void run("json")}>
        JSON
      </button>
    </span>
  );
}
