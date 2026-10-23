import { registerTab } from "../app/registry";
import { registerNodeAction } from "../explorer/actions";
import { openTopN, TopNTab } from "./topn/TopNTab";

export function initGrid() {
  registerTab("grid", TopNTab);
  registerNodeAction({
    id: "top-n",
    label: "View top rows",
    primary: true,
    applies: (t) => t.type === "relation" && t.rel.kind !== "sequence",
    run: (t) => {
      if (t.type === "relation") openTopN({ connectionId: t.connectionId, database: t.db, schema: t.rel.schema, name: t.rel.name });
    },
  });
}
