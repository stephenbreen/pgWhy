// Context actions on explorer nodes. Features register handlers (Script-as, top-N rows,
// backup, details); the tree only knows labels and where they apply.

import type { DatabaseNode, RelationNode, RoutineNode } from "../lib/ipc/explorer";

export type NodeTarget =
  | { type: "database"; connectionId: string; db: DatabaseNode }
  | { type: "schema"; connectionId: string; db: string; schema: string }
  | { type: "relation"; connectionId: string; db: string; rel: RelationNode }
  | { type: "routine"; connectionId: string; db: string; routine: RoutineNode };

export interface NodeAction {
  id: string;
  label: string;
  /** submenu grouping, e.g. "Script as" */
  group?: string;
  applies: (t: NodeTarget) => boolean;
  /** greyed out with this reason when it applies but can't run */
  disabledReason?: (t: NodeTarget) => string | null;
  run: (t: NodeTarget) => void | Promise<void>;
  /** the node's default (double-click / Enter) action */
  primary?: boolean;
}

const actions: NodeAction[] = [];
type SelectHandler = (t: NodeTarget) => void;
let onSelect: SelectHandler | null = null;

export function registerNodeAction(a: NodeAction) {
  const i = actions.findIndex((x) => x.id === a.id);
  if (i >= 0) actions[i] = a;
  else actions.push(a);
}

export function actionsFor(t: NodeTarget): NodeAction[] {
  // the default action leads the menu
  return actions.filter((a) => a.applies(t)).sort((a, b) => Number(!!b.primary) - Number(!!a.primary));
}

/** The details panel (U10) listens to selections. */
export function setSelectHandler(h: SelectHandler | null) {
  onSelect = h;
}

export function notifySelect(t: NodeTarget) {
  onSelect?.(t);
}
