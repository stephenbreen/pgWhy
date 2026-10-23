import { act, fireEvent, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeAll, describe, expect, it, vi } from "vitest";
import { decodeChunk, encodeChunk, ROW_UPDATED } from "./protocol";
import { RowStore, WINDOW, type WindowSource } from "./rowstore";
import { ResultGrid, toTsv } from "./ResultGrid";

function fakeSource(rows: (string | null)[][], ncols: number) {
  const requests: [number, number][] = [];
  let store: RowStore | null = null;
  const source: WindowSource = {
    request(start, count) {
      requests.push([start, count]);
      queueMicrotask(() => store!.deliver(decodeChunk(encodeChunk(start, rows.slice(start, start + count), ncols))));
    },
  };
  store = new RowStore(source);
  store.setRowCount(rows.length);
  return { store, requests };
}

// jsdom does no layout: give every element a viewport-sized box so the virtualizer renders
beforeAll(() => {
  Object.defineProperty(HTMLElement.prototype, "offsetHeight", { configurable: true, get: () => 600 });
  Object.defineProperty(HTMLElement.prototype, "offsetWidth", { configurable: true, get: () => 1000 });
});

describe("window protocol", () => {
  it("round-trips text, NULL vs empty string, and multibyte values", () => {
    const rows = [["1", "héllo 漢字", null], ["2", "", "x"]];
    const c = decodeChunk(encodeChunk(512, rows, 3, [0, ROW_UPDATED], [[1, 2]]));
    expect(c.start).toBe(512);
    expect(c.cells[1]).toEqual(["héllo 漢字", ""]);
    expect(c.cells[2]).toEqual([null, "x"]);
    expect(c.rowFlags[1]).toBe(ROW_UPDATED);
    expect(!!(c.pending[2][0] & 2)).toBe(true);
  });
});

describe("row store", () => {
  it("requests aligned windows clamped to the row count and caps in-flight requests", () => {
    const requests: [number, number][] = [];
    const store = new RowStore({ request: (s, n) => requests.push([s, n]) });
    store.setRowCount(1000);
    store.ensureRange(0, 5000);
    expect(requests.length).toBeLessThanOrEqual(store.inflightCap);
    expect(requests[0]).toEqual([0, WINDOW]);
    expect(requests.every(([s, n]) => s % WINDOW === 0 && s + n <= 1000)).toBe(true);
    expect(requests.at(-1)).toEqual([768, 232]);
  });
});

describe("row store re-render granularity", () => {
  it("bumps only the delivered window, so other rows keep their snapshot", () => {
    const store = new RowStore({ request: () => {} });
    store.setRowCount(WINDOW * 2);
    const untouchedBefore = store.versionOf(WINDOW);
    let notified = 0;
    const off = store.subscribe(() => notified++);

    store.deliver(decodeChunk(encodeChunk(0, [["a"]], 1)));
    expect(store.versionOf(0)).toBeGreaterThan(0);
    expect(store.versionOf(WINDOW)).toBe(untouchedBefore);
    expect(notified).toBe(1);

    // a second delivery to the same window bumps it again (streaming grows a window)
    const first = store.versionOf(0);
    store.deliver(decodeChunk(encodeChunk(0, [["a"], ["b"]], 1)));
    expect(store.versionOf(0)).toBeGreaterThan(first);

    off();
    store.deliver(decodeChunk(encodeChunk(WINDOW, [["c"]], 1)));
    expect(notified).toBe(2);
  });
});

describe("result grid", () => {
  const cols = [{ name: "id", type: "integer" }, { name: "name", type: "text" }, { name: "note", type: "text" }];
  const rows = Array.from({ length: 300 }, (_, i) => [String(i + 1), `name ${i + 1}`, i === 0 ? null : i === 1 ? "" : "n"]);

  it("renders windowed rows with NULL distinct from empty", async () => {
    const { store } = fakeSource(rows, 3);
    render(<ResultGrid columns={cols} store={store} rowCount={rows.length} />);
    const grid = screen.getByRole("grid", { name: "Results" });
    const first = await within(grid).findByText("name 1");
    const row1 = first.closest('[role="row"]')!;
    const cells = within(row1 as HTMLElement).getAllByRole("gridcell");
    expect(cells[2]).toHaveTextContent("NULL");
    expect(cells[2]).toHaveAttribute("data-null", "true");
    const row2 = within(grid).getByText("name 2").closest('[role="row"]') as HTMLElement;
    const empty = within(row2).getAllByRole("gridcell")[2];
    expect(empty).toHaveTextContent("");
    expect(empty).not.toHaveAttribute("data-null");
    // only the window is resident
    expect(store.residentRows()).toBeLessThan(rows.length + 1);
  });

  it("copies a selected range as TSV", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.assign(navigator, { clipboard: { writeText } });
    const { store } = fakeSource(rows, 3);
    render(<ResultGrid columns={cols} store={store} rowCount={rows.length} />);
    const grid = screen.getByRole("grid");
    await within(grid).findByText("name 1");
    const firstRow = within(grid).getByText("name 1").closest('[role="row"]') as HTMLElement;
    fireEvent.mouseDown(within(firstRow).getAllByRole("gridcell")[0]);
    fireEvent.mouseDown(within(grid).getByText("name 2"), { shiftKey: true });
    fireEvent.keyDown(grid.querySelector(".grid-scroll")!, { key: "c", ctrlKey: true });
    await act(async () => {});
    expect(writeText).toHaveBeenCalledWith("1\tname 1\n2\tname 2");
  });

  it("double-click edits a cell when editing is enabled", async () => {
    const user = userEvent.setup();
    const onEdit = vi.fn();
    const { store } = fakeSource(rows, 3);
    render(<ResultGrid columns={cols} store={store} rowCount={rows.length} onEditCell={onEdit} />);
    await user.dblClick(await screen.findByText("name 3"));
    const input = screen.getByLabelText("Cell value");
    await user.clear(input);
    await user.type(input, "renamed{Enter}");
    expect(onEdit).toHaveBeenCalledWith(2, 1, "renamed");
  });

  it("sort headers report clicks and show direction", async () => {
    const user = userEvent.setup();
    const onSort = vi.fn();
    const { store } = fakeSource(rows, 3);
    render(<ResultGrid columns={cols} store={store} rowCount={rows.length} onSort={onSort} sort={{ column: 1, dir: "desc" }} />);
    expect(screen.getByRole("columnheader", { name: /name/ })).toHaveAttribute("aria-sort", "descending");
    await user.click(screen.getByRole("columnheader", { name: /^id/ }));
    expect(onSort).toHaveBeenCalledWith(0);
  });

  it("tsv quoting", () => {
    expect(toTsv([["a\tb", 'say "hi"', null]])).toBe('"a\tb"\t"say ""hi"""\t');
  });
});
