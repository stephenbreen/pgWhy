// Details dock (R4, AE1): columns with types/nullability/defaults, keys, indexes, size and
// stats for the selected table — never a dialog. Always serves cached data with an explicit
// refresh; tells you when the object no longer exists.

import { useConnections } from "../../connection/store";
import { ErrorNote } from "../../connection/ErrorNote";
import type { ConstraintInfo, RelationDetails } from "../../lib/ipc/explorer";
import { actionsFor } from "../actions";
import { useDetails } from "./store";

function bytes(n?: number | null): string {
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

function when(ts?: string | null): string {
  if (!ts) return "never";
  const d = new Date(ts);
  return isNaN(+d) ? ts : d.toLocaleString();
}

const KIND_LABEL: Record<string, string> = {
  table: "Table", partitioned_table: "Partitioned table", view: "View", matview: "Materialized view",
  foreign_table: "Foreign table", sequence: "Sequence",
};

const CONSTRAINT_LABEL: Record<ConstraintInfo["kind"], string> = {
  primary_key: "PK", foreign_key: "FK", unique: "UNIQUE", check: "CHECK", exclusion: "EXCLUDE",
};

function Body({ d }: { d: RelationDetails }) {
  const r = d.relation;
  const pk = new Set(d.constraints.find((c) => c.kind === "primary_key")?.columns ?? []);
  const fkCols = new Set(d.constraints.filter((c) => c.kind === "foreign_key").flatMap((c) => c.columns));
  const isTable = r.kind === "table" || r.kind === "partitioned_table" || r.kind === "matview";
  return (
    <>
      <dl className="facts">
        <dt>Kind</dt>
        <dd>{KIND_LABEL[r.kind] ?? r.kind}</dd>
        {isTable && (
          <>
            <dt>Rows</dt>
            <dd>{d.stats.row_estimate != null ? `~${d.stats.row_estimate.toLocaleString()}` : "unknown"}</dd>
            <dt>Size</dt>
            <dd>
              {bytes(d.stats.total_bytes)} <span className="muted">(table {bytes(d.stats.table_bytes)}, indexes {bytes(d.stats.indexes_bytes)})</span>
            </dd>
            <dt>Analyzed</dt>
            <dd>{when(d.stats.last_analyze ?? d.stats.last_autoanalyze)}</dd>
            <dt>Vacuumed</dt>
            <dd>{when(d.stats.last_vacuum ?? d.stats.last_autovacuum)}</dd>
          </>
        )}
        <dt>Editable</dt>
        <dd>{d.edit_key.length ? `yes, keyed by ${d.edit_key.join(", ")}` : (d.read_only_reason ?? "no")}</dd>
        {d.raw.relation.partition_key && (
          <>
            <dt>Partitioned by</dt>
            <dd>{d.raw.relation.partition_key}</dd>
          </>
        )}
        {r.comment && (
          <>
            <dt>Comment</dt>
            <dd>{r.comment}</dd>
          </>
        )}
      </dl>
      {r.columns.length > 0 && (
        <section aria-label="Columns">
          <h4>Columns</h4>
          <table className="details-table">
            <thead>
              <tr><th>Name</th><th>Type</th><th>Null</th><th>Default</th></tr>
            </thead>
            <tbody>
              {r.columns.map((c) => (
                <tr key={c.name}>
                  <td>
                    {pk.has(c.name) && <span className="key-mark" title="primary key">PK</span>}
                    {fkCols.has(c.name) && <span className="key-mark fk" title="foreign key">FK</span>}
                    {c.name}
                  </td>
                  <td className="mono">{c.type}</td>
                  <td>{c.not_null ? "no" : "yes"}</td>
                  <td className="mono">{c.identity ? `identity (${c.identity})` : c.generated ? `generated (${c.generated})` : (c.default ?? "")}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </section>
      )}
      {d.constraints.length > 0 && (
        <section aria-label="Constraints">
          <h4>Keys &amp; constraints</h4>
          <ul className="details-list">
            {d.constraints.map((c) => (
              <li key={c.name}>
                <span className="key-mark">{CONSTRAINT_LABEL[c.kind]}</span> <b>{c.name}</b>
                <div className="mono muted">{c.definition}</div>
              </li>
            ))}
          </ul>
        </section>
      )}
      {d.indexes.length > 0 && (
        <section aria-label="Indexes">
          <h4>Indexes</h4>
          <ul className="details-list">
            {d.indexes.map((i) => (
              <li key={i.name}>
                <b>{i.name}</b> {i.primary ? <span className="muted">primary</span> : i.unique ? <span className="muted">unique</span> : null}
                {!i.valid && <span className="warn"> invalid</span>}
                <span className="muted"> {bytes(i.size_bytes)}</span>
                <div className="mono muted">{i.definition}</div>
              </li>
            ))}
          </ul>
        </section>
      )}
      {d.raw.sequence && (
        <section aria-label="Sequence">
          <h4>Sequence</h4>
          <dl className="facts">
            <dt>Type</dt><dd>{d.raw.sequence.data_type}</dd>
            <dt>Last value</dt><dd>{d.raw.sequence.last_value ?? "not used yet"}</dd>
            <dt>Increment</dt><dd>{d.raw.sequence.increment}</dd>
            {d.raw.sequence.owned_by && (<><dt>Owned by</dt><dd>{d.raw.sequence.owned_by.table.name}.{d.raw.sequence.owned_by.column}</dd></>)}
          </dl>
        </section>
      )}
      {d.view_definition && (
        <section aria-label="Definition">
          <h4>Definition</h4>
          <pre className="mono definition">{d.view_definition}</pre>
        </section>
      )}
    </>
  );
}

export function DetailsPanel() {
  const { target, details, missing, loading, error, refresh, show } = useDetails();
  const entry = useConnections((s) => s.entries.find((e) => e.id === target?.connectionId));
  const title =
    target?.type === "relation" ? `${target.rel.schema}.${target.rel.name}` : target?.type === "routine" ? `${target.routine.name}(${target.routine.identity_args})` : target?.type === "database" ? target.db.name : target?.type === "schema" ? target.schema : "Details";
  const actions = target ? actionsFor(target).filter((a) => !a.group) : [];
  return (
    <aside className="details-dock" aria-label="Details">
      <header className="pane-header">
        <span className="details-title" title={title}>{title}</span>
        <span className="spacer" />
        {target && (
          <button type="button" className="icon" aria-label="Refresh details" title="Refresh" onClick={() => void refresh()}>
            ↻
          </button>
        )}
        <button type="button" className="icon" aria-label="Hide details" onClick={() => show(false)}>
          ✕
        </button>
      </header>
      <div className="details-body">
        {!target && <p className="empty">Select a table or view in the Explorer.</p>}
        {target && entry && <p className="details-where">{entry.name} / {"db" in target ? (typeof target.db === "string" ? target.db : target.db.name) : ""}</p>}
        {actions.length > 0 && (
          <div className="details-actions">
            {actions.map((a) => {
              const reason = a.disabledReason?.(target!) ?? null;
              return (
                <button key={a.id} type="button" disabled={!!reason} title={reason ?? undefined} onClick={() => void a.run(target!)}>
                  {a.label}
                </button>
              );
            })}
          </div>
        )}
        {error && <ErrorNote error={error} />}
        {loading && !details && <p className="empty">Loading…</p>}
        {missing && (
          <div className="warn-note">
            <p>This object no longer exists.</p>
          </div>
        )}
        {target?.type === "routine" && (
          <dl className="facts">
            <dt>Kind</dt><dd>{target.routine.kind}</dd>
            <dt>Arguments</dt><dd className="mono">({target.routine.identity_args})</dd>
            {target.routine.result && (<><dt>Returns</dt><dd className="mono">{target.routine.result}</dd></>)}
          </dl>
        )}
        {details && <Body d={details} />}
      </div>
    </aside>
  );
}
