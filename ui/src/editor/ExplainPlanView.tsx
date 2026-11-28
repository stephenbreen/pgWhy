// Visual PostgreSQL EXPLAIN / EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) plan tree viewer.
// Computes exclusive (self) timing/cost, estimation accuracy flags, buffer hit ratios,
// and lets the user toggle between the visual tree and raw JSON.

import { useMemo, useState } from "react";
import type { ExplainNode, ExplainOutput, ExplainRoot } from "../lib/ipc/query";

interface EnrichedNode {
  node: ExplainNode;
  children: EnrichedNode[];
  inclusiveTime: number | null;
  selfTime: number | null;
  totalCost: number;
  selfCost: number;
  actualRowsTotal: number | null;
  planRows: number | null;
  estWarning: string | null;
  sharePct: number;
}

function extractRoot(raw: unknown): ExplainRoot | null {
  const first = Array.isArray(raw) ? raw[0] : raw;
  if (!first || typeof first !== "object" || !("Plan" in first)) return null;
  return first as ExplainRoot;
}

function enrich(node: ExplainNode): Omit<EnrichedNode, "sharePct"> {
  const children = (node.Plans ?? []).map(enrich);
  const loops = node["Actual Loops"] ?? 1;
  const inclusiveTime =
    typeof node["Actual Total Time"] === "number" ? node["Actual Total Time"] * loops : null;
  const childInclusive = children.reduce((acc, c) => acc + (c.inclusiveTime ?? 0), 0);
  const selfTime = inclusiveTime != null ? Math.max(0, inclusiveTime - childInclusive) : null;

  const totalCost = typeof node["Total Cost"] === "number" ? node["Total Cost"] : 0;
  const childCost = children.reduce((acc, c) => acc + c.totalCost, 0);
  const selfCost = Math.max(0, totalCost - childCost);

  const actualRowsTotal =
    typeof node["Actual Rows"] === "number" ? node["Actual Rows"] * loops : null;
  const planRows = typeof node["Plan Rows"] === "number" ? node["Plan Rows"] : null;

  let estWarning: string | null = null;
  if (actualRowsTotal != null && planRows != null && (actualRowsTotal > 0 || planRows > 0)) {
    const a = Math.max(1, actualRowsTotal);
    const p = Math.max(1, planRows);
    const ratio = a >= p ? a / p : p / a;
    if (ratio >= 10) {
      estWarning = `${ratio >= 100 ? Math.round(ratio) : ratio.toFixed(1)}× ${a > p ? "underestimated" : "overestimated"}`;
    }
  }

  return {
    node,
    children: children as EnrichedNode[],
    inclusiveTime,
    selfTime,
    totalCost,
    selfCost,
    actualRowsTotal,
    planRows,
    estWarning,
  };
}

function withShares(
  n: Omit<EnrichedNode, "sharePct">,
  rootTotalTime: number | null,
  rootTotalCost: number,
): EnrichedNode {
  const sharePct =
    rootTotalTime != null && rootTotalTime > 0 && n.selfTime != null
      ? Math.min(100, (n.selfTime / rootTotalTime) * 100)
      : rootTotalCost > 0
        ? Math.min(100, (n.selfCost / rootTotalCost) * 100)
        : 0;
  return {
    ...n,
    sharePct,
    children: n.children.map((c) => withShares(c, rootTotalTime, rootTotalCost)),
  };
}

function sumBuffers(node: ExplainNode): { hit: number; read: number; dirtied: number; written: number } {
  const hit = node["Shared Hit Blocks"] ?? 0;
  const read = node["Shared Read Blocks"] ?? 0;
  const dirtied = node["Shared Dirtied Blocks"] ?? 0;
  const written = node["Shared Written Blocks"] ?? 0;
  // PostgreSQL already rolls child buffers up into the root node in EXPLAIN (ANALYZE, BUFFERS).
  return { hit, read, dirtied, written };
}

function fmtMs(ms: number): string {
  if (ms < 0.01) return "<0.01 ms";
  if (ms < 10) return `${ms.toFixed(2)} ms`;
  if (ms < 1000) return `${ms.toFixed(1)} ms`;
  return `${(ms / 1000).toFixed(2)} s`;
}

function PlanNodeRow({ item, depth, analyzed }: { item: EnrichedNode; depth: number; analyzed: boolean }) {
  const [collapsed, setCollapsed] = useState(false);
  const n = item.node;
  const target = [n.Schema, n["Relation Name"]].filter(Boolean).join(".");
  const alias = n.Alias && n.Alias !== n["Relation Name"] ? ` as ${n.Alias}` : "";
  const heat = item.sharePct >= 50 ? "hot" : item.sharePct >= 20 ? "warm" : "cool";
  const isSeqScan = n["Node Type"] === "Seq Scan";

  const details: [string, string][] = [];
  if (n["Index Name"]) details.push(["Index", n["Index Name"]]);
  if (n["Index Cond"]) details.push(["Index Cond", n["Index Cond"]]);
  if (n["Hash Cond"]) details.push(["Hash Cond", n["Hash Cond"]]);
  if (n["Merge Cond"]) details.push(["Merge Cond", n["Merge Cond"]]);
  if (n["Join Filter"]) details.push(["Join Filter", n["Join Filter"]]);
  if (n["Recheck Cond"]) details.push(["Recheck Cond", n["Recheck Cond"]]);
  if (n.Filter) {
    const removed =
      typeof n["Rows Removed by Filter"] === "number"
        ? ` (removed ${n["Rows Removed by Filter"].toLocaleString()})`
        : "";
    details.push(["Filter", `${n.Filter}${removed}`]);
  }
  if (Array.isArray(n["Sort Key"]) && n["Sort Key"].length > 0) {
    details.push(["Sort", `${n["Sort Key"].join(", ")}${n["Sort Method"] ? ` (${n["Sort Method"]})` : ""}`]);
  }
  if (Array.isArray(n["Group Key"]) && n["Group Key"].length > 0) {
    details.push(["Group", n["Group Key"].join(", ")]);
  }

  const bufParts: string[] = [];
  if (n["Shared Hit Blocks"]) bufParts.push(`hit=${n["Shared Hit Blocks"].toLocaleString()}`);
  if (n["Shared Read Blocks"]) bufParts.push(`read=${n["Shared Read Blocks"].toLocaleString()}`);
  if (n["Shared Dirtied Blocks"]) bufParts.push(`dirtied=${n["Shared Dirtied Blocks"].toLocaleString()}`);
  if (n["Shared Written Blocks"]) bufParts.push(`written=${n["Shared Written Blocks"].toLocaleString()}`);
  if (n["Temp Read Blocks"] || n["Temp Written Blocks"]) {
    bufParts.push(
      `temp=${(n["Temp Read Blocks"] ?? 0).toLocaleString()}/${(n["Temp Written Blocks"] ?? 0).toLocaleString()}`,
    );
  }
  if (bufParts.length > 0) details.push(["Buffers", bufParts.join(" ")]);

  return (
    <li className="plan-node" style={{ "--depth": depth } as React.CSSProperties}>
      <div className="plan-node-card" data-heat={heat}>
        <div className="plan-node-main">
          {item.children.length > 0 ? (
            <button
              type="button"
              className="icon plan-toggle"
              aria-expanded={!collapsed}
              aria-label={`${collapsed ? "Expand" : "Collapse"} ${n["Node Type"]}`}
              onClick={() => setCollapsed((c) => !c)}
            >
              {collapsed ? "▸" : "▾"}
            </button>
          ) : (
            <span className="plan-toggle-spacer" aria-hidden="true">•</span>
          )}
          <span className="plan-node-type">
            {n["Node Type"]}
            {n["Join Type"] ? ` (${n["Join Type"]})` : ""}
          </span>
          {target && (
            <span className="mono plan-node-rel">
              on {target}
              {alias}
            </span>
          )}
          {n["Subplan Name"] && <span className="plan-chip">{n["Subplan Name"]}</span>}
          {isSeqScan && (item.actualRowsTotal ?? item.planRows ?? 0) >= 1000 && (
            <span className="plan-chip warn">Seq Scan</span>
          )}
          {item.estWarning && <span className="plan-chip warn">{item.estWarning}</span>}
          <span className="spacer" />
          <div className="plan-node-metrics mono">
            {analyzed && item.selfTime != null && item.inclusiveTime != null ? (
              <span title={`Self: ${fmtMs(item.selfTime)} · Total: ${fmtMs(item.inclusiveTime)}`}>
                {fmtMs(item.selfTime)}
                {item.children.length > 0 && <span className="muted"> / {fmtMs(item.inclusiveTime)}</span>}
              </span>
            ) : (
              <span title="Estimated startup..total cost">
                cost {(n["Startup Cost"] ?? 0).toFixed(1)}..{item.totalCost.toFixed(1)}
              </span>
            )}
            <span className="muted">·</span>
            <span>
              {analyzed && item.actualRowsTotal != null
                ? `${item.actualRowsTotal.toLocaleString()} rows`
                : `~${(item.planRows ?? 0).toLocaleString()} rows`}
              {(n["Actual Loops"] ?? 1) > 1 ? ` ×${n["Actual Loops"]}` : ""}
            </span>
          </div>
        </div>
        <div className="plan-bar-track" aria-hidden="true">
          <div className="plan-bar-fill" data-heat={heat} style={{ width: `${Math.max(2, Math.round(item.sharePct))}%` }} />
        </div>
        {details.length > 0 && (
          <dl className="plan-node-details mono">
            {details.map(([k, v]) => (
              <div key={k} className="plan-detail-item">
                <dt>{k}:</dt>
                <dd>{v}</dd>
              </div>
            ))}
          </dl>
        )}
      </div>
      {!collapsed && item.children.length > 0 && (
        <ul className="plan-children">
          {item.children.map((child, idx) => (
            <PlanNodeRow key={`${child.node["Node Type"]}-${idx}`} item={child} depth={depth + 1} analyzed={analyzed} />
          ))}
        </ul>
      )}
    </li>
  );
}

export function ExplainPlanView({ output }: { output: ExplainOutput }) {
  const [mode, setMode] = useState<"tree" | "json">("tree");
  const [copied, setCopied] = useState(false);

  const parsed = useMemo(() => {
    const root = extractRoot(output.plan);
    if (!root) return null;
    const base = enrich(root.Plan);
    const rootTime =
      typeof root["Execution Time"] === "number" ? root["Execution Time"] : base.inclusiveTime;
    const tree = withShares(base, rootTime, base.totalCost);
    const buffers = sumBuffers(root.Plan);
    return { root, tree, buffers };
  }, [output.plan]);

  const rawJson = useMemo(() => JSON.stringify(output.plan, null, 2), [output.plan]);

  if (!parsed) {
    return (
      <div className="explain-pane" aria-label="Execution plan">
        <pre className="mono explain-json">{rawJson}</pre>
      </div>
    );
  }

  const { root, tree, buffers } = parsed;
  const totalBuf = buffers.hit + buffers.read;
  const hitPct = totalBuf > 0 ? Math.round((buffers.hit / totalBuf) * 100) : null;

  return (
    <div className="explain-pane" aria-label="Execution plan">
      <div className="explain-summary">
        <span className="plan-badge" data-analyze={output.analyze || undefined}>
          {output.analyze ? (output.buffers ? "EXPLAIN ANALYZE · BUFFERS" : "EXPLAIN ANALYZE") : "EXPLAIN (estimate)"}
        </span>
        {output.rolled_back && <span className="plan-badge safe">rolled back safely</span>}
        {typeof root["Planning Time"] === "number" && (
          <span>
            Planning: <b className="mono">{fmtMs(root["Planning Time"])}</b>
          </span>
        )}
        {typeof root["Execution Time"] === "number" && (
          <span>
            Execution: <b className="mono">{fmtMs(root["Execution Time"])}</b>
          </span>
        )}
        <span>
          Cost: <b className="mono">{tree.totalCost.toFixed(2)}</b>
        </span>
        {totalBuf > 0 && (
          <span>
            Buffers: <b className="mono">{buffers.hit.toLocaleString()} hit / {buffers.read.toLocaleString()} read</b>
            {hitPct != null && <span className="muted"> ({hitPct}% cache)</span>}
          </span>
        )}
        <span className="spacer" />
        <div className="explain-view-toggle" role="group" aria-label="Plan format">
          <button type="button" aria-pressed={mode === "tree"} onClick={() => setMode("tree")}>
            Tree
          </button>
          <button type="button" aria-pressed={mode === "json"} onClick={() => setMode("json")}>
            JSON
          </button>
          <button
            type="button"
            onClick={() => {
              void navigator.clipboard?.writeText(rawJson).then(() => setCopied(true)).catch(() => {});
            }}
          >
            {copied ? "Copied" : "Copy JSON"}
          </button>
        </div>
      </div>
      {mode === "tree" ? (
        <ul className="plan-tree" aria-label="Plan tree">
          <PlanNodeRow item={tree} depth={0} analyzed={output.analyze} />
        </ul>
      ) : (
        <pre className="mono explain-json">{rawJson}</pre>
      )}
    </div>
  );
}
