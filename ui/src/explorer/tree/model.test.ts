import { mockIPC } from "@tauri-apps/api/mocks";
import { beforeEach, expect, it } from "vitest";
import { useTrees } from "./model";

const trees0 = useTrees.getState();
beforeEach(() => useTrees.setState({ ...trees0, trees: {} }, true));

it("a catalog load that was in flight when the tree was invalidated does not land over the fresh one", async () => {
  const pending: ((v: { name: string; system: boolean }[]) => void)[] = [];
  mockIPC((cmd) => {
    if (cmd === "explorer_schemas") return new Promise((r) => pending.push(r));
    return null;
  });
  const first = useTrees.getState().loadSchemas("c1", "app");
  useTrees.getState().invalidate("c1", "app");
  const second = useTrees.getState().loadSchemas("c1", "app");
  expect(pending).toHaveLength(2);

  pending[1]([{ name: "fresh", system: false }]);
  await second;
  pending[0]([{ name: "stale", system: false }]);
  await first;

  const cur = useTrees.getState().trees.c1.schemas.app;
  expect(cur).toMatchObject({ state: "loaded", value: [{ name: "fresh" }] });
});

it("invalidating one database leaves another database's load alone", async () => {
  let release!: (v: { name: string; system: boolean }[]) => void;
  mockIPC((cmd) => (cmd === "explorer_schemas" ? new Promise((r) => (release = r)) : null));
  const load = useTrees.getState().loadSchemas("c1", "other");
  useTrees.getState().invalidate("c1", "app");
  release([{ name: "public", system: false }]);
  await load;
  expect(useTrees.getState().trees.c1.schemas.other).toMatchObject({ state: "loaded" });
});
