import { beforeEach, expect, it, vi } from "vitest";
import { requestCloseAll, requestCloseTab, setCloseGuard, useWorkspace } from "./workspace";

const ws0 = useWorkspace.getState();
beforeEach(() => {
  useWorkspace.setState(ws0, true);
  for (const id of ["a", "b"]) setCloseGuard(id, null);
});

const open = (id: string) => useWorkspace.getState().openTab({ id, kind: "editor", title: id });

it("closing all only asks: no teardown runs, even when a later tab refuses", async () => {
  const teardownA = vi.fn();
  open("a");
  open("b");
  setCloseGuard("a", async () => true, teardownA);
  setCloseGuard("b", async () => false);
  expect(await requestCloseAll()).toBe(false);
  expect(teardownA).not.toHaveBeenCalled();

  setCloseGuard("b", async () => true);
  expect(await requestCloseAll()).toBe(true);
  expect(teardownA).not.toHaveBeenCalled();
  expect(useWorkspace.getState().tabs.map((t) => t.id)).toEqual(["welcome", "a", "b"]);
});

it("closing one tab asks, then tears down, then removes it; a refusal does neither", async () => {
  const order: string[] = [];
  open("a");
  setCloseGuard("a", async () => (order.push("ask"), false), () => void order.push("teardown"));
  expect(await requestCloseTab("a")).toBe(false);
  expect(order).toEqual(["ask"]);
  expect(useWorkspace.getState().tabs.some((t) => t.id === "a")).toBe(true);

  setCloseGuard("a", async () => (order.push("ask"), true), () => void order.push("teardown"));
  expect(await requestCloseTab("a")).toBe(true);
  expect(order).toEqual(["ask", "ask", "teardown"]);
  expect(useWorkspace.getState().tabs.some((t) => t.id === "a")).toBe(false);
});

it("closing a background tab brings it forward so its prompt can be seen", async () => {
  open("a");
  open("b");
  let activeWhenAsked: string | null = null;
  setCloseGuard("a", async () => ((activeWhenAsked = useWorkspace.getState().activeTabId), false));
  await requestCloseTab("a");
  expect(activeWhenAsked).toBe("a");
});
