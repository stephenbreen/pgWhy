// Typed runtime parameters editor ({{param_name}}) for Workflows v2.
// Secret parameters never store a default value on disk.

import type * as w from "../lib/ipc/workflow";

const PARAM_KINDS: { value: w.ParamType; label: string }[] = [
  { value: "text", label: "Text" },
  { value: "number", label: "Number" },
  { value: "file_path", label: "File path" },
  { value: "secret", label: "Secret (never saved)" },
];

export function ParamsEditor({
  params,
  onChange,
}: {
  params: w.WorkflowParam[];
  onChange: (next: w.WorkflowParam[]) => void;
}) {
  const update = (index: number, patch: Partial<w.WorkflowParam>) => {
    const next = params.map((p, i) => {
      if (i !== index) return p;
      const merged = { ...p, ...patch };
      if (merged.kind === "secret") merged.default = null;
      return merged;
    });
    onChange(next);
  };

  const addParam = () => {
    const base = "param";
    let n = params.length + 1;
    let name = `${base}_${n}`;
    while (params.some((p) => p.name === name)) {
      n += 1;
      name = `${base}_${n}`;
    }
    onChange([
      ...params,
      { name, label: "", description: null, kind: "text", required: true, default: null },
    ]);
  };

  const remove = (index: number) => onChange(params.filter((_, i) => i !== index));

  return (
    <div className="wf-params">
      <div className="wf-section-head">
        <span className="muted">
          Reference declared parameters as <code className="mono">{"{{name}}"}</code> in visual steps or SQL. Secret values are entered at run time and never written to disk.
        </span>
        <button type="button" onClick={addParam}>
          + Add parameter
        </button>
      </div>
      {params.length === 0 ? (
        <p className="muted wf-empty-hint">No runtime parameters declared.</p>
      ) : (
        <div className="wf-param-list">
          {params.map((p, idx) => (
            <div key={idx} className="wf-param-row">
              <label className="wf-param-name">
                Name
                <input
                  aria-label={`Parameter ${idx + 1} name`}
                  className="mono"
                  value={p.name}
                  placeholder="test_password"
                  onChange={(e) => update(idx, { name: e.target.value })}
                  required
                />
              </label>
              <label className="wf-param-label">
                Label
                <input
                  aria-label={`Parameter ${idx + 1} label`}
                  value={p.label ?? ""}
                  placeholder="Optional prompt label"
                  onChange={(e) => update(idx, { label: e.target.value || null })}
                />
              </label>
              <label className="wf-param-kind">
                Type
                <select
                  aria-label={`Parameter ${idx + 1} type`}
                  value={p.kind}
                  onChange={(e) => update(idx, { kind: e.target.value as w.ParamType })}
                >
                  {PARAM_KINDS.map((k) => (
                    <option key={k.value} value={k.value}>
                      {k.label}
                    </option>
                  ))}
                </select>
              </label>
              <label className="wf-param-default">
                Default
                {p.kind === "secret" ? (
                  <input
                    aria-label={`Parameter ${idx + 1} default`}
                    value=""
                    disabled
                    placeholder="prompted at run time"
                  />
                ) : (
                  <input
                    aria-label={`Parameter ${idx + 1} default`}
                    value={p.default ?? ""}
                    placeholder={p.kind === "number" ? "0" : "optional default"}
                    onChange={(e) => update(idx, { default: e.target.value || null })}
                  />
                )}
              </label>
              <label className="check wf-param-req">
                <input
                  type="checkbox"
                  checked={p.required}
                  onChange={(e) => update(idx, { required: e.target.checked })}
                />
                Required
              </label>
              <button
                type="button"
                className="icon"
                aria-label={`Remove parameter ${p.name || idx + 1}`}
                title="Remove parameter"
                onClick={() => remove(idx)}
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
