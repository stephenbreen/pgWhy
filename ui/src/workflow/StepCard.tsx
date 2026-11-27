// WYSIWYG pipeline step card for Workflows v2 (delete_rows, insert_rows, update_mask, sql, verify).
// Supports Smart 4-level catalog hints, live compiled SQL preview, and one-click Convert to SQL.

import { useId, useState } from "react";
import type { ScriptInfo } from "../lib/ipc/scripts";
import * as w from "../lib/ipc/workflow";
import type { CatalogHints } from "./useCatalogHints";

const STEP_BADGE: Record<w.WorkflowStep["kind"], string> = {
  delete_rows: "Delete",
  insert_rows: "Insert",
  update_mask: "Mask / Update",
  sql: "SQL",
  verify: "Verify",
};

const FILTER_OPS: { value: w.FilterOp; label: string }[] = [
  { value: "eq", label: "=" },
  { value: "neq", label: "!=" },
  { value: "lt", label: "<" },
  { value: "lte", label: "<=" },
  { value: "gt", label: ">" },
  { value: "gte", label: ">=" },
  { value: "like", label: "LIKE" },
  { value: "ilike", label: "ILIKE" },
  { value: "not_like", label: "NOT LIKE" },
  { value: "not_ilike", label: "NOT ILIKE" },
  { value: "in", label: "IN" },
  { value: "not_in", label: "NOT IN" },
  { value: "is_null", label: "IS NULL" },
  { value: "is_not_null", label: "IS NOT NULL" },
];

const COMPARE_OPS: { value: w.CompareOp; label: string }[] = [
  { value: "eq", label: "=" },
  { value: "neq", label: "!=" },
  { value: "gt", label: ">" },
  { value: "gte", label: ">=" },
  { value: "lt", label: "<" },
  { value: "lte", label: "<=" },
];

function SchemaTablePicker({
  schema,
  table,
  catalog,
  onChange,
  idPrefix,
}: {
  schema: string;
  table: string;
  catalog: CatalogHints;
  onChange: (schema: string, table: string) => void;
  idPrefix: string;
}) {
  const schemaListId = `${idPrefix}-schemas`;
  const tableListId = `${idPrefix}-tables`;
  const tables = catalog.tablesInSchema(schema);
  return (
    <div className="row wf-table-row">
      <label className="wf-schema-field">
        Schema
        <input
          aria-label="Schema"
          className="mono"
          list={schemaListId}
          value={schema}
          placeholder="public"
          onChange={(e) => onChange(e.target.value, table)}
        />
        <datalist id={schemaListId}>
          {catalog.schemas.map((s) => (
            <option key={s} value={s} />
          ))}
        </datalist>
      </label>
      <label className="grow">
        Table
        <input
          aria-label="Table"
          className="mono"
          list={tableListId}
          value={table}
          placeholder="users"
          onChange={(e) => onChange(schema, e.target.value)}
        />
        <datalist id={tableListId}>
          {tables.map((t) => (
            <option key={`${t.schema}.${t.name}`} value={t.name} />
          ))}
        </datalist>
      </label>
    </div>
  );
}

function defaultCellForKind(kind: w.CellValue["kind"], params: w.WorkflowParam[]): w.CellValue {
  switch (kind) {
    case "literal":
      return { kind: "literal", value: "" };
    case "number":
      return { kind: "number", value: "0" };
    case "bool":
      return { kind: "bool", value: true };
    case "null":
      return { kind: "null" };
    case "default":
      return { kind: "default" };
    case "param":
      return { kind: "param", name: params[0]?.name ?? "" };
    case "expr":
      return { kind: "expr", sql: "now()" };
    case "list":
      return { kind: "list", items: [{ kind: "literal", value: "" }] };
  }
}

function listToText(val: w.CellValue): string {
  if (val.kind !== "list") return "";
  return val.items
    .map((item) => {
      if (item.kind === "literal" || item.kind === "number") return item.value;
      if (item.kind === "bool") return String(item.value);
      if (item.kind === "param") return `{{${item.name}}}`;
      if (item.kind === "expr") return item.sql;
      return "";
    })
    .join(", ");
}

function textToList(raw: string): w.CellValue {
  const parts = raw
    .split(",")
    .map((s) => s.trim())
    .filter((s) => s.length > 0);
  const items: w.CellValue[] = parts.map((p) => {
    const m = /^\{\{([A-Za-z_][A-Za-z0-9_]*)\}\}$/.exec(p);
    if (m) return { kind: "param", name: m[1] };
    return { kind: "literal", value: p };
  });
  return { kind: "list", items };
}

function CellValueEditor({
  value,
  params,
  allowDefault = false,
  listMode = false,
  ariaLabel,
  onChange,
}: {
  value: w.CellValue;
  params: w.WorkflowParam[];
  allowDefault?: boolean;
  listMode?: boolean;
  ariaLabel?: string;
  onChange: (next: w.CellValue) => void;
}) {
  if (listMode) {
    const listVal: w.CellValue = value.kind === "list" ? value : { kind: "list", items: [] };
    return (
      <input
        aria-label={ariaLabel ?? "Comma-separated values"}
        className="mono wf-cell-input"
        value={listToText(listVal)}
        placeholder="admin, qa, {{extra_role}}"
        onChange={(e) => onChange(textToList(e.target.value))}
      />
    );
  }

  return (
    <div className="wf-cell-editor">
      <select
        aria-label={ariaLabel ? `${ariaLabel} type` : "Value type"}
        className="wf-cell-kind"
        value={value.kind}
        onChange={(e) => onChange(defaultCellForKind(e.target.value as w.CellValue["kind"], params))}
      >
        <option value="literal">Text</option>
        <option value="number">Num</option>
        <option value="bool">Bool</option>
        <option value="null">NULL</option>
        {allowDefault && <option value="default">DEFAULT</option>}
        <option value="param">{"{{Param}}"}</option>
        <option value="expr">SQL expr</option>
      </select>
      {value.kind === "literal" && (
        <input
          aria-label={ariaLabel ?? "Value"}
          className="wf-cell-input"
          value={value.value}
          placeholder="text value"
          onChange={(e) => onChange({ kind: "literal", value: e.target.value })}
        />
      )}
      {value.kind === "number" && (
        <input
          aria-label={ariaLabel ?? "Numeric value"}
          className="mono wf-cell-input"
          value={value.value}
          placeholder="0"
          onChange={(e) => onChange({ kind: "number", value: e.target.value })}
        />
      )}
      {value.kind === "bool" && (
        <select
          aria-label={ariaLabel ?? "Boolean value"}
          className="wf-cell-input"
          value={value.value ? "true" : "false"}
          onChange={(e) => onChange({ kind: "bool", value: e.target.value === "true" })}
        >
          <option value="true">true</option>
          <option value="false">false</option>
        </select>
      )}
      {value.kind === "param" &&
        (params.length > 0 ? (
          <select
            aria-label={ariaLabel ?? "Parameter"}
            className="mono wf-cell-input"
            value={value.name}
            onChange={(e) => onChange({ kind: "param", name: e.target.value })}
          >
            <option value="">choose param…</option>
            {params.map((p) => (
              <option key={p.name} value={p.name}>
                {"{{"}
                {p.name}
                {"}}"}
                {p.kind === "secret" ? " (secret)" : ""}
              </option>
            ))}
            {value.name && !params.some((p) => p.name === value.name) && (
              <option value={value.name}>
                {"{{"}
                {value.name}
                {"}}"} (undeclared)
              </option>
            )}
          </select>
        ) : (
          <input
            aria-label={ariaLabel ?? "Parameter name"}
            className="mono wf-cell-input"
            value={value.name}
            placeholder="param_name"
            onChange={(e) => onChange({ kind: "param", name: e.target.value })}
          />
        ))}
      {value.kind === "expr" && (
        <input
          aria-label={ariaLabel ?? "SQL expression"}
          className="mono wf-cell-input"
          value={value.sql}
          placeholder="now() or crypt({{pw}}, gen_salt('bf'))"
          onChange={(e) => onChange({ kind: "expr", sql: e.target.value })}
        />
      )}
      {(value.kind === "null" || value.kind === "default") && (
        <span className="muted mono wf-cell-fixed">{value.kind.toUpperCase()}</span>
      )}
    </div>
  );
}

function PredicateListEditor({
  conditions,
  combinator,
  columnNames,
  params,
  idPrefix,
  emptyHint,
  onChange,
}: {
  conditions: w.Predicate[];
  combinator: w.LogicalOp;
  columnNames: string[];
  params: w.WorkflowParam[];
  idPrefix: string;
  emptyHint?: string;
  onChange: (conditions: w.Predicate[], combinator: w.LogicalOp) => void;
}) {
  const colListId = `${idPrefix}-pred-cols`;

  const addCondition = () => {
    onChange(
      [...conditions, { column: columnNames[0] ?? "", op: "eq", value: { kind: "literal", value: "" } }],
      combinator,
    );
  };

  const updateCond = (idx: number, patch: Partial<w.Predicate>) => {
    const next = conditions.map((c, i) => {
      if (i !== idx) return c;
      const merged = { ...c, ...patch };
      if (w.isUnaryOp(merged.op)) {
        merged.value = null;
      } else if (w.isListOp(merged.op)) {
        if (!merged.value || merged.value.kind !== "list") {
          merged.value = { kind: "list", items: [] };
        }
      } else if (!merged.value || merged.value.kind === "list") {
        merged.value = { kind: "literal", value: "" };
      }
      return merged;
    });
    onChange(next, combinator);
  };

  const removeCond = (idx: number) => onChange(conditions.filter((_, i) => i !== idx), combinator);

  return (
    <div className="wf-predicates">
      <div className="wf-subhead">
        <span className="muted">WHERE conditions</span>
        {conditions.length > 1 && (
          <select
            aria-label="Condition combinator"
            className="wf-combinator"
            value={combinator}
            onChange={(e) => onChange(conditions, e.target.value as w.LogicalOp)}
          >
            <option value="and">Match ALL (AND)</option>
            <option value="or">Match ANY (OR)</option>
          </select>
        )}
        <span className="spacer" />
        <button type="button" onClick={addCondition}>
          + Condition
        </button>
      </div>
      <datalist id={colListId}>
        {columnNames.map((col) => (
          <option key={col} value={col} />
        ))}
      </datalist>
      {conditions.length === 0 ? (
        emptyHint ? <p className="muted wf-empty-hint">{emptyHint}</p> : null
      ) : (
        <div className="wf-predicate-rows">
          {conditions.map((cond, idx) => (
            <div key={idx} className="wf-predicate-row">
              <input
                aria-label={`Condition ${idx + 1} column`}
                className="mono wf-pred-col"
                list={colListId}
                value={cond.column}
                placeholder="column"
                onChange={(e) => updateCond(idx, { column: e.target.value })}
              />
              <select
                aria-label={`Condition ${idx + 1} operator`}
                className="wf-pred-op"
                value={cond.op}
                onChange={(e) => updateCond(idx, { op: e.target.value as w.FilterOp })}
              >
                {FILTER_OPS.map((o) => (
                  <option key={o.value} value={o.value}>
                    {o.label}
                  </option>
                ))}
              </select>
              {!w.isUnaryOp(cond.op) && (
                <div className="grow">
                  <CellValueEditor
                    ariaLabel={`Condition ${idx + 1} value`}
                    value={cond.value ?? { kind: "literal", value: "" }}
                    params={params}
                    listMode={w.isListOp(cond.op)}
                    onChange={(val) => updateCond(idx, { value: val })}
                  />
                </div>
              )}
              <button
                type="button"
                className="icon"
                aria-label={`Remove condition ${idx + 1}`}
                title="Remove condition"
                onClick={() => removeCond(idx)}
              >
                ✕
              </button>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

function DeleteRowsBody({
  step,
  catalog,
  params,
  idPrefix,
  onChange,
}: {
  step: w.DeleteRowsStep;
  catalog: CatalogHints;
  params: w.WorkflowParam[];
  idPrefix: string;
  onChange: (next: w.DeleteRowsStep) => void;
}) {
  const cols = catalog.columnsFor(step.schema, step.table).map((c) => c.name);

  const switchMode = (mode: "delete_where" | "truncate") => {
    if (mode === step.mode) return;
    if (mode === "truncate") {
      onChange({
        id: step.id,
        name: step.name,
        enabled: step.enabled,
        no_transaction: step.no_transaction,
        kind: "delete_rows",
        schema: step.schema,
        table: step.table,
        mode: "truncate",
        restart_identity: false,
        cascade: false,
      });
    } else {
      onChange({
        id: step.id,
        name: step.name,
        enabled: step.enabled,
        no_transaction: step.no_transaction,
        kind: "delete_rows",
        schema: step.schema,
        table: step.table,
        mode: "delete_where",
        conditions: [],
        combinator: "and",
      });
    }
  };

  return (
    <div className="wf-step-fields">
      <SchemaTablePicker
        schema={step.schema}
        table={step.table}
        catalog={catalog}
        idPrefix={idPrefix}
        onChange={(schema, table) => onChange({ ...step, schema, table })}
      />
      <div className="wf-inline-radios" role="radiogroup" aria-label="Delete mode">
        <label className="check">
          <input
            type="radio"
            name={`${idPrefix}-delmode`}
            checked={step.mode === "delete_where"}
            onChange={() => switchMode("delete_where")}
          />
          DELETE WHERE
        </label>
        <label className="check">
          <input
            type="radio"
            name={`${idPrefix}-delmode`}
            checked={step.mode === "truncate"}
            onChange={() => switchMode("truncate")}
          />
          TRUNCATE table
        </label>
      </div>
      {step.mode === "delete_where" ? (
        <PredicateListEditor
          conditions={step.conditions}
          combinator={step.combinator}
          columnNames={cols}
          params={params}
          idPrefix={idPrefix}
          emptyHint="No WHERE filter — deletes all rows in the table."
          onChange={(conditions, combinator) => onChange({ ...step, conditions, combinator })}
        />
      ) : (
        <div className="checks">
          <label className="check">
            <input
              type="checkbox"
              checked={step.restart_identity}
              onChange={(e) => onChange({ ...step, restart_identity: e.target.checked })}
            />
            RESTART IDENTITY
          </label>
          <label className="check">
            <input
              type="checkbox"
              checked={step.cascade}
              onChange={(e) => onChange({ ...step, cascade: e.target.checked })}
            />
            CASCADE (also truncates referencing tables)
          </label>
        </div>
      )}
    </div>
  );
}

function InsertRowsBody({
  step,
  catalog,
  params,
  idPrefix,
  onChange,
}: {
  step: w.InsertRowsStep;
  catalog: CatalogHints;
  params: w.WorkflowParam[];
  idPrefix: string;
  onChange: (next: w.InsertRowsStep) => void;
}) {
  const tableCols = catalog.columnsFor(step.schema, step.table);
  const colListId = `${idPrefix}-ins-cols`;

  const loadFromCatalog = () => {
    const editable = tableCols.filter((c) => !c.generated);
    if (editable.length === 0) return;
    const names = editable.map((c) => c.name);
    const sampleRow: w.CellValue[] = editable.map((c) => {
      if (c.default || c.identity) return { kind: "default" };
      if (!c.not_null) return { kind: "null" };
      if (/bool/i.test(c.type)) return { kind: "bool", value: false };
      if (/int|numeric|decimal|real|double|float/i.test(c.type)) return { kind: "number", value: "0" };
      return { kind: "literal", value: "" };
    });
    onChange({ ...step, columns: names, rows: [sampleRow] });
  };

  const updateColName = (colIdx: number, name: string) => {
    const columns = step.columns.map((c, i) => (i === colIdx ? name : c));
    onChange({ ...step, columns });
  };

  const addColumn = () => {
    const nextCol = tableCols.find((c) => !step.columns.includes(c.name))?.name ?? `col_${step.columns.length + 1}`;
    const columns = [...step.columns, nextCol];
    const rows = step.rows.map((r) => [...r, { kind: "literal", value: "" } as w.CellValue]);
    onChange({ ...step, columns, rows });
  };

  const removeColumn = (colIdx: number) => {
    if (step.columns.length <= 1) return;
    const columns = step.columns.filter((_, i) => i !== colIdx);
    const rows = step.rows.map((r) => r.filter((_, i) => i !== colIdx));
    onChange({ ...step, columns, rows });
  };

  const addRow = () => {
    const emptyRow: w.CellValue[] = step.columns.map(() => ({ kind: "literal", value: "" }));
    onChange({ ...step, rows: [...step.rows, emptyRow] });
  };

  const removeRow = (rowIdx: number) => {
    onChange({ ...step, rows: step.rows.filter((_, i) => i !== rowIdx) });
  };

  const updateCell = (rowIdx: number, colIdx: number, val: w.CellValue) => {
    const rows = step.rows.map((r, ri) =>
      ri === rowIdx ? r.map((c, ci) => (ci === colIdx ? val : c)) : r,
    );
    onChange({ ...step, rows });
  };

  const conflictColsText =
    step.on_conflict.mode === "do_nothing"
      ? (step.on_conflict.columns ?? []).join(", ")
      : step.on_conflict.mode === "do_update"
        ? step.on_conflict.columns.join(", ")
        : "";

  const updateColsText =
    step.on_conflict.mode === "do_update" ? (step.on_conflict.update_columns ?? []).join(", ") : "";

  const parseCols = (raw: string) =>
    raw
      .split(",")
      .map((s) => s.trim())
      .filter(Boolean);

  return (
    <div className="wf-step-fields">
      <SchemaTablePicker
        schema={step.schema}
        table={step.table}
        catalog={catalog}
        idPrefix={idPrefix}
        onChange={(schema, table) => onChange({ ...step, schema, table })}
      />
      <div className="wf-subhead">
        <span className="muted">Rows ({step.rows.length})</span>
        {tableCols.length > 0 && (
          <button type="button" onClick={loadFromCatalog}>
            Load table columns ({tableCols.length})
          </button>
        )}
        <span className="spacer" />
        <button type="button" onClick={addColumn}>
          + Column
        </button>
        <button type="button" onClick={addRow}>
          + Add row
        </button>
      </div>
      <datalist id={colListId}>
        {tableCols.map((c) => (
          <option key={c.name} value={c.name} />
        ))}
      </datalist>
      <div className="wf-grid-wrap">
        <table className="wf-grid-table">
          <thead>
            <tr>
              {step.columns.map((col, ci) => {
                const meta = tableCols.find((c) => c.name === col);
                return (
                  <th key={ci}>
                    <div className="wf-col-head">
                      <input
                        aria-label={`Column ${ci + 1} name`}
                        className="mono"
                        list={colListId}
                        value={col}
                        placeholder="column"
                        onChange={(e) => updateColName(ci, e.target.value)}
                      />
                      {step.columns.length > 1 && (
                        <button
                          type="button"
                          className="icon"
                          aria-label={`Remove column ${col || ci + 1}`}
                          title="Remove column"
                          onClick={() => removeColumn(ci)}
                        >
                          ✕
                        </button>
                      )}
                    </div>
                    {meta && (
                      <div className="muted wf-col-meta">
                        {meta.type}
                        {meta.not_null && !meta.default ? " · NOT NULL" : ""}
                      </div>
                    )}
                  </th>
                );
              })}
              <th className="wf-row-act-col" />
            </tr>
          </thead>
          <tbody>
            {step.rows.map((row, ri) => (
              <tr key={ri}>
                {step.columns.map((col, ci) => (
                  <td key={ci}>
                    <CellValueEditor
                      ariaLabel={`Row ${ri + 1} ${col || `col ${ci + 1}`}`}
                      value={row[ci] ?? { kind: "literal", value: "" }}
                      params={params}
                      allowDefault
                      onChange={(val) => updateCell(ri, ci, val)}
                    />
                  </td>
                ))}
                <td className="wf-row-act-col">
                  <button
                    type="button"
                    className="icon"
                    aria-label={`Remove row ${ri + 1}`}
                    title="Remove row"
                    onClick={() => removeRow(ri)}
                  >
                    ✕
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <div className="row wf-conflict-row">
        <label>
          On conflict
          <select
            aria-label="On conflict policy"
            value={step.on_conflict.mode}
            onChange={(e) => {
              const mode = e.target.value as w.ConflictPolicy["mode"];
              if (mode === "error") onChange({ ...step, on_conflict: { mode: "error" } });
              else if (mode === "do_nothing") onChange({ ...step, on_conflict: { mode: "do_nothing", columns: [] } });
              else onChange({ ...step, on_conflict: { mode: "do_update", columns: step.columns.slice(0, 1), update_columns: [] } });
            }}
          >
            <option value="error">Fail on conflict</option>
            <option value="do_nothing">ON CONFLICT DO NOTHING</option>
            <option value="do_update">ON CONFLICT DO UPDATE</option>
          </select>
        </label>
        {step.on_conflict.mode !== "error" && (
          <label className="grow">
            Conflict column(s){step.on_conflict.mode === "do_nothing" ? " (optional)" : ""}
            <input
              aria-label="Conflict columns"
              className="mono"
              value={conflictColsText}
              placeholder="email"
              onChange={(e) => {
                const cols = parseCols(e.target.value);
                if (step.on_conflict.mode === "do_nothing") {
                  onChange({ ...step, on_conflict: { mode: "do_nothing", columns: cols } });
                } else if (step.on_conflict.mode === "do_update") {
                  onChange({ ...step, on_conflict: { ...step.on_conflict, columns: cols } });
                }
              }}
            />
          </label>
        )}
        {step.on_conflict.mode === "do_update" && (
          <label className="grow">
            Update column(s) (blank = all other)
            <input
              aria-label="Update columns"
              className="mono"
              value={updateColsText}
              placeholder="role, name"
              onChange={(e) => {
                if (step.on_conflict.mode === "do_update") {
                  onChange({
                    ...step,
                    on_conflict: { ...step.on_conflict, update_columns: parseCols(e.target.value) },
                  });
                }
              }}
            />
          </label>
        )}
      </div>
    </div>
  );
}

function UpdateMaskBody({
  step,
  catalog,
  params,
  idPrefix,
  onChange,
}: {
  step: w.UpdateMaskStep;
  catalog: CatalogHints;
  params: w.WorkflowParam[];
  idPrefix: string;
  onChange: (next: w.UpdateMaskStep) => void;
}) {
  const cols = catalog.columnsFor(step.schema, step.table).map((c) => c.name);
  const colListId = `${idPrefix}-mask-cols`;

  const addAssignment = () => {
    const nextCol = cols.find((c) => !step.assignments.some((a) => a.column === c)) ?? "";
    onChange({
      ...step,
      assignments: [...step.assignments, { column: nextCol, transform: { kind: "set_null" } }],
    });
  };

  const updateAssignment = (idx: number, nextItem: w.MaskAssignment) => {
    onChange({ ...step, assignments: step.assignments.map((a, i) => (i === idx ? nextItem : a)) });
  };

  const removeAssignment = (idx: number) => {
    onChange({ ...step, assignments: step.assignments.filter((_, i) => i !== idx) });
  };

  const defaultTransform = (kind: w.MaskTransform["kind"]): w.MaskTransform => {
    switch (kind) {
      case "synthetic_email":
        return { kind: "synthetic_email", domain: "example.test", id_column: null };
      case "fixed_value":
        return { kind: "fixed_value", value: { kind: "literal", value: "" } };
      case "set_null":
        return { kind: "set_null" };
      case "sha256_hash":
        return { kind: "sha256_hash", salt: null };
      case "custom_expr":
        return { kind: "custom_expr", sql: "" };
    }
  };

  return (
    <div className="wf-step-fields">
      <SchemaTablePicker
        schema={step.schema}
        table={step.table}
        catalog={catalog}
        idPrefix={idPrefix}
        onChange={(schema, table) => onChange({ ...step, schema, table })}
      />
      <div className="wf-subhead">
        <span className="muted">Column transforms</span>
        <span className="spacer" />
        <button type="button" onClick={addAssignment}>
          + Column transform
        </button>
      </div>
      <datalist id={colListId}>
        {cols.map((c) => (
          <option key={c} value={c} />
        ))}
      </datalist>
      <div className="wf-mask-rows">
        {step.assignments.map((a, idx) => (
          <div key={idx} className="wf-mask-row">
            <input
              aria-label={`Transform ${idx + 1} column`}
              className="mono wf-pred-col"
              list={colListId}
              value={a.column}
              placeholder="column"
              onChange={(e) => updateAssignment(idx, { ...a, column: e.target.value })}
            />
            <select
              aria-label={`Transform ${idx + 1} kind`}
              value={a.transform.kind}
              onChange={(e) =>
                updateAssignment(idx, {
                  ...a,
                  transform: defaultTransform(e.target.value as w.MaskTransform["kind"]),
                })
              }
            >
              <option value="synthetic_email">Synthetic email</option>
              <option value="fixed_value">Fixed value</option>
              <option value="set_null">Set NULL</option>
              <option value="sha256_hash">SHA-256 hash</option>
              <option value="custom_expr">Custom SQL expr</option>
            </select>
            <div className="grow wf-mask-detail">
              {a.transform.kind === "synthetic_email" && (
                <div className="row">
                  <input
                    aria-label={`Transform ${idx + 1} domain`}
                    className="mono grow"
                    value={a.transform.domain}
                    placeholder="example.test"
                    onChange={(e) =>
                      updateAssignment(idx, {
                        ...a,
                        transform: {
                          kind: "synthetic_email",
                          domain: e.target.value,
                          id_column: a.transform.kind === "synthetic_email" ? a.transform.id_column : null,
                        },
                      })
                    }
                  />
                  <input
                    aria-label={`Transform ${idx + 1} id column`}
                    className="mono wf-narrow-input"
                    list={colListId}
                    value={a.transform.id_column ?? ""}
                    placeholder="id col (opt)"
                    onChange={(e) =>
                      updateAssignment(idx, {
                        ...a,
                        transform: {
                          kind: "synthetic_email",
                          domain: a.transform.kind === "synthetic_email" ? a.transform.domain : "example.test",
                          id_column: e.target.value || null,
                        },
                      })
                    }
                  />
                </div>
              )}
              {a.transform.kind === "fixed_value" && (
                <CellValueEditor
                  ariaLabel={`Transform ${idx + 1} value`}
                  value={a.transform.value}
                  params={params}
                  allowDefault
                  onChange={(value) => updateAssignment(idx, { ...a, transform: { kind: "fixed_value", value } })}
                />
              )}
              {a.transform.kind === "set_null" && <span className="muted mono">SET = NULL</span>}
              {a.transform.kind === "sha256_hash" && (
                <CellValueEditor
                  ariaLabel={`Transform ${idx + 1} salt`}
                  value={a.transform.salt ?? { kind: "literal", value: "" }}
                  params={params}
                  onChange={(salt) =>
                    updateAssignment(idx, {
                      ...a,
                      transform: {
                        kind: "sha256_hash",
                        salt: salt.kind === "literal" && salt.value === "" ? null : salt,
                      },
                    })
                  }
                />
              )}
              {a.transform.kind === "custom_expr" && (
                <input
                  aria-label={`Transform ${idx + 1} SQL expression`}
                  className="mono grow"
                  value={a.transform.sql}
                  placeholder="'anon_' || id::text"
                  onChange={(e) => updateAssignment(idx, { ...a, transform: { kind: "custom_expr", sql: e.target.value } })}
                />
              )}
            </div>
            <button
              type="button"
              className="icon"
              aria-label={`Remove transform ${idx + 1}`}
              title="Remove transform"
              onClick={() => removeAssignment(idx)}
            >
              ✕
            </button>
          </div>
        ))}
      </div>
      <PredicateListEditor
        conditions={step.conditions}
        combinator={step.combinator}
        columnNames={cols}
        params={params}
        idPrefix={idPrefix}
        emptyHint="No WHERE filter — updates every row in the table."
        onChange={(conditions, combinator) => onChange({ ...step, conditions, combinator })}
      />
    </div>
  );
}

function SqlBody({
  step,
  scripts,
  params,
  idPrefix,
  onChange,
}: {
  step: w.SqlStep;
  scripts: ScriptInfo[];
  params: w.WorkflowParam[];
  idPrefix: string;
  onChange: (next: w.SqlStep) => void;
}) {
  const switchSource = (source: "inline" | "library") => {
    if (source === step.source) return;
    if (source === "inline") {
      onChange({
        id: step.id,
        name: step.name,
        enabled: step.enabled,
        no_transaction: step.no_transaction,
        kind: "sql",
        source: "inline",
        sql: "",
      });
    } else {
      onChange({
        id: step.id,
        name: step.name,
        enabled: step.enabled,
        no_transaction: step.no_transaction,
        kind: "sql",
        source: "library",
        script_id: scripts.find((s) => !s.missing && /\.sql$/i.test(s.path))?.id ?? "",
      });
    }
  };

  return (
    <div className="wf-step-fields">
      <div className="wf-inline-radios" role="radiogroup" aria-label="SQL source">
        <label className="check">
          <input
            type="radio"
            name={`${idPrefix}-sqlsrc`}
            checked={step.source === "inline"}
            onChange={() => switchSource("inline")}
          />
          Inline SQL
        </label>
        <label className="check">
          <input
            type="radio"
            name={`${idPrefix}-sqlsrc`}
            checked={step.source === "library"}
            onChange={() => switchSource("library")}
          />
          Linked script from Library
        </label>
      </div>
      {step.source === "inline" ? (
        <>
          <textarea
            aria-label="Inline SQL editor"
            className="mono wf-sql-textarea"
            rows={5}
            value={step.sql}
            placeholder="DELETE FROM public.sessions WHERE expires_at < now();"
            onChange={(e) => onChange({ ...step, sql: e.target.value })}
          />
          {params.length > 0 && (
            <div className="wf-param-chips">
              <span className="muted">Insert param:</span>
              {params.map((p) => (
                <button
                  key={p.name}
                  type="button"
                  className="wf-chip"
                  onClick={() => onChange({ ...step, sql: `${step.sql}{{${p.name}}}` })}
                >
                  {"{{"}
                  {p.name}
                  {"}}"}
                </button>
              ))}
            </div>
          )}
        </>
      ) : (
        <label>
          Library script
          <select
            aria-label="Library script"
            value={step.script_id}
            onChange={(e) => onChange({ ...step, script_id: e.target.value })}
          >
            <option value="">choose script…</option>
            {scripts
              .filter((s) => /\.sql$/i.test(s.path))
              .map((s) => (
                <option key={s.id} value={s.id} disabled={s.missing}>
                  {s.path}
                </option>
              ))}
          </select>
        </label>
      )}
      <label className="check">
        <input
          type="checkbox"
          checked={step.no_transaction}
          onChange={(e) => onChange({ ...step, no_transaction: e.target.checked })}
        />
        Run outside transaction (for VACUUM, CREATE INDEX CONCURRENTLY)
      </label>
    </div>
  );
}

function VerifyBody({
  step,
  catalog,
  params,
  idPrefix,
  onChange,
}: {
  step: w.VerifyStep;
  catalog: CatalogHints;
  params: w.WorkflowParam[];
  idPrefix: string;
  onChange: (next: w.VerifyStep) => void;
}) {
  const a = step.assertion;
  const setAssertion = (assertion: w.VerifyAssertion) => onChange({ ...step, assertion });

  const switchKind = (kind: w.VerifyAssertion["kind"]) => {
    if (kind === a.kind) return;
    const schema = a.kind === "custom_sql" ? "public" : a.schema;
    const table = a.kind === "custom_sql" ? "" : a.table;
    if (kind === "row_count") {
      setAssertion({ kind: "row_count", schema, table, op: "gte", expected: 1, conditions: [], combinator: "and" });
    } else if (kind === "zero_rows") {
      setAssertion({ kind: "zero_rows", schema, table, conditions: [], combinator: "and" });
    } else {
      setAssertion({ kind: "custom_sql", sql: "SELECT true;", expect: "truthy_scalar" });
    }
  };

  return (
    <div className="wf-step-fields">
      <label>
        Assertion type
        <select
          aria-label="Verify assertion type"
          value={a.kind}
          onChange={(e) => switchKind(e.target.value as w.VerifyAssertion["kind"])}
        >
          <option value="row_count">Row count assertion</option>
          <option value="zero_rows">Zero matching rows</option>
          <option value="custom_sql">Custom SQL assertion</option>
        </select>
      </label>
      {a.kind === "row_count" && (
        <>
          <SchemaTablePicker
            schema={a.schema}
            table={a.table}
            catalog={catalog}
            idPrefix={idPrefix}
            onChange={(schema, table) => setAssertion({ ...a, schema, table })}
          />
          <div className="row">
            <label>
              Expect COUNT(*)
              <select
                aria-label="Row count comparison"
                value={a.op}
                onChange={(e) => setAssertion({ ...a, op: e.target.value as w.CompareOp })}
              >
                {COMPARE_OPS.map((o) => (
                  <option key={o.value} value={o.value}>
                    {o.label}
                  </option>
                ))}
              </select>
            </label>
            <label className="narrow">
              Expected rows
              <input
                aria-label="Expected row count"
                type="number"
                value={a.expected}
                onChange={(e) => setAssertion({ ...a, expected: Number(e.target.value || 0) })}
              />
            </label>
          </div>
          <PredicateListEditor
            conditions={a.conditions}
            combinator={a.combinator}
            columnNames={catalog.columnsFor(a.schema, a.table).map((c) => c.name)}
            params={params}
            idPrefix={idPrefix}
            onChange={(conditions, combinator) => setAssertion({ ...a, conditions, combinator })}
          />
        </>
      )}
      {a.kind === "zero_rows" && (
        <>
          <SchemaTablePicker
            schema={a.schema}
            table={a.table}
            catalog={catalog}
            idPrefix={idPrefix}
            onChange={(schema, table) => setAssertion({ ...a, schema, table })}
          />
          <PredicateListEditor
            conditions={a.conditions}
            combinator={a.combinator}
            columnNames={catalog.columnsFor(a.schema, a.table).map((c) => c.name)}
            params={params}
            idPrefix={idPrefix}
            emptyHint="Passes only if zero rows match."
            onChange={(conditions, combinator) => setAssertion({ ...a, conditions, combinator })}
          />
        </>
      )}
      {a.kind === "custom_sql" && (
        <>
          <label>
            Pass condition
            <select
              aria-label="Custom SQL pass condition"
              value={a.expect}
              onChange={(e) => setAssertion({ ...a, expect: e.target.value as w.CustomVerifyExpect })}
            >
              <option value="succeeds">Query succeeds without error</option>
              <option value="truthy_scalar">First column of single row is true</option>
              <option value="zero_rows">Query returns zero rows</option>
            </select>
          </label>
          <textarea
            aria-label="Verify SQL"
            className="mono wf-sql-textarea"
            rows={4}
            value={a.sql}
            placeholder="SELECT count(*) > 0 FROM public.users;"
            onChange={(e) => setAssertion({ ...a, sql: e.target.value })}
          />
        </>
      )}
    </div>
  );
}

export function StepCard({
  step,
  index,
  total,
  catalog,
  params,
  scripts,
  compiled,
  previewError,
  onChange,
  onMove,
  onDuplicate,
  onRemove,
  onConvertToSql,
  onRequestPreview,
}: {
  step: w.WorkflowStep;
  index: number;
  total: number;
  catalog: CatalogHints;
  params: w.WorkflowParam[];
  scripts: ScriptInfo[];
  compiled?: w.CompiledStep;
  previewError?: string | null;
  onChange: (next: w.WorkflowStep) => void;
  onMove: (delta: -1 | 1) => void;
  onDuplicate: () => void;
  onRemove: () => void;
  onConvertToSql?: () => Promise<void>;
  onRequestPreview: () => void;
}) {
  const idPrefix = useId();
  const [showPreview, setShowPreview] = useState(false);
  const [converting, setConverting] = useState(false);

  const togglePreview = () => {
    const next = !showPreview;
    setShowPreview(next);
    if (next) onRequestPreview();
  };

  return (
    <article
      className="wf-step-card"
      data-kind={step.kind}
      data-disabled={!step.enabled}
      aria-label={`Step ${index + 1}: ${step.name || STEP_BADGE[step.kind]}`}
    >
      <header className="wf-step-head">
        <input
          type="checkbox"
          aria-label={`Enable step ${step.name || index + 1}`}
          checked={step.enabled}
          onChange={(e) => onChange({ ...step, enabled: e.target.checked })}
        />
        <span className="wf-step-badge" data-kind={step.kind}>
          {STEP_BADGE[step.kind]}
        </span>
        <input
          aria-label={`Step ${index + 1} name`}
          className="wf-step-name"
          value={step.name}
          placeholder="Step name"
          onChange={(e) => onChange({ ...step, name: e.target.value })}
        />
        <div className="wf-step-controls">
          <button type="button" onClick={togglePreview} aria-pressed={showPreview}>
            {showPreview ? "Hide SQL" : "Preview SQL"}
          </button>
          {step.kind !== "sql" && onConvertToSql && (
            <button
              type="button"
              disabled={converting}
              onClick={async () => {
                setConverting(true);
                try {
                  await onConvertToSql();
                } finally {
                  setConverting(false);
                }
              }}
            >
              Convert to SQL
            </button>
          )}
          <button
            type="button"
            className="icon"
            aria-label={`Move step ${index + 1} up`}
            title="Move up"
            disabled={index === 0}
            onClick={() => onMove(-1)}
          >
            ↑
          </button>
          <button
            type="button"
            className="icon"
            aria-label={`Move step ${index + 1} down`}
            title="Move down"
            disabled={index === total - 1}
            onClick={() => onMove(1)}
          >
            ↓
          </button>
          <button
            type="button"
            className="icon"
            aria-label={`Duplicate step ${index + 1}`}
            title="Duplicate step"
            onClick={onDuplicate}
          >
            ⧉
          </button>
          <button
            type="button"
            className="icon"
            aria-label={`Remove step ${index + 1}`}
            title="Remove step"
            onClick={onRemove}
          >
            ✕
          </button>
        </div>
      </header>

      <div className="wf-step-body">
        {step.kind === "delete_rows" && (
          <DeleteRowsBody step={step} catalog={catalog} params={params} idPrefix={idPrefix} onChange={onChange} />
        )}
        {step.kind === "insert_rows" && (
          <InsertRowsBody step={step} catalog={catalog} params={params} idPrefix={idPrefix} onChange={onChange} />
        )}
        {step.kind === "update_mask" && (
          <UpdateMaskBody step={step} catalog={catalog} params={params} idPrefix={idPrefix} onChange={onChange} />
        )}
        {step.kind === "sql" && (
          <SqlBody step={step} scripts={scripts} params={params} idPrefix={idPrefix} onChange={onChange} />
        )}
        {step.kind === "verify" && (
          <VerifyBody step={step} catalog={catalog} params={params} idPrefix={idPrefix} onChange={onChange} />
        )}
      </div>

      {compiled && compiled.problems.length > 0 && (
        <div className="warn-note wf-step-alert" role="alert">
          <ul>
            {compiled.problems.map((p) => (
              <li key={p}>{p}</li>
            ))}
          </ul>
        </div>
      )}

      {compiled && compiled.warnings.length > 0 && (
        <div className="wf-step-warnings">
          {compiled.warnings.map((warn) => (
            <p key={warn} className="warn">
              ⚠ {warn}
            </p>
          ))}
        </div>
      )}

      {showPreview && (
        <div className="wf-sql-preview" aria-label={`SQL preview for ${step.name || `step ${index + 1}`}`}>
          {!step.enabled ? (
            <span className="muted">Step is disabled and will not run.</span>
          ) : compiled ? (
            <pre className="mono">{compiled.sql}</pre>
          ) : previewError ? (
            <span className="error-text">{previewError}</span>
          ) : (
            <span className="muted">Compiling SQL preview…</span>
          )}
        </div>
      )}
    </article>
  );
}
