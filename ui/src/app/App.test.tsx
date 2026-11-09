import { fireEvent, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeAll, beforeEach, describe, expect, it } from "vitest";
import { App } from "./App";
import { registerView } from "./registry";
import { useWorkspace } from "./workspace";
import { initShortcuts } from "./shortcuts";
import { SIDEBAR_W, useLayout } from "./layout";

const initial = useWorkspace.getState();
const initialLayout = useLayout.getState();

beforeAll(() => {
  initShortcuts();
  // jsdom has no pointer capture; the splitters take it on pointerdown
  Object.assign(HTMLElement.prototype, { setPointerCapture() {}, releasePointerCapture() {}, hasPointerCapture: () => false });
});

beforeEach(() => {
  useWorkspace.setState(initial, true);
  useLayout.setState(initialLayout, true);
});

describe("app shell", () => {
  it("renders the three activity views and the always-visible status bar", () => {
    render(<App />);
    const views = screen.getByRole("navigation", { name: "Views" });
    for (const name of ["Explorer", "Scripts", "Workflows"]) {
      expect(within(views).getByRole("button", { name })).toBeInTheDocument();
    }
    expect(screen.getByRole("contentinfo", { name: "Status" })).toHaveTextContent("No active connection");
    // launch lands on the Explorer view
    expect(screen.getByRole("complementary", { name: "Explorer" })).toBeInTheDocument();
  });

  it("switches sidebar views and toggles the sidebar on re-click", async () => {
    const user = userEvent.setup();
    render(<App />);
    await user.click(screen.getByRole("button", { name: "Scripts" }));
    expect(screen.getByRole("complementary", { name: "Scripts" })).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Scripts" }));
    expect(screen.queryByRole("complementary")).not.toBeInTheDocument();
  });

  it("Ctrl/⌘ 1, 2, 3 open Explorer, Scripts and Workflows without toggling the sidebar away", async () => {
    const user = userEvent.setup();
    render(<App />);
    await user.keyboard("{Control>}2{/Control}");
    expect(screen.getByRole("complementary", { name: "Scripts" })).toBeInTheDocument();
    await user.keyboard("{Control>}2{/Control}");
    expect(screen.getByRole("complementary", { name: "Scripts" })).toBeInTheDocument();
    await user.keyboard("{Control>}3{/Control}");
    expect(screen.getByRole("complementary", { name: "Workflows" })).toBeInTheDocument();
    // a hidden sidebar comes back
    await user.click(screen.getByRole("button", { name: "Workflows" }));
    expect(screen.queryByRole("complementary")).not.toBeInTheDocument();
    await user.keyboard("{Control>}1{/Control}");
    expect(screen.getByRole("complementary", { name: "Explorer" })).toBeInTheDocument();
  });

  it("Ctrl/⌘ =, - and 0 zoom in, out and back; the status bar shows a zoom away from 100%", async () => {
    const user = userEvent.setup();
    render(<App />);
    const zoomKey = (code: string, key: string) => fireEvent.keyDown(window, { code, key, ctrlKey: true });
    zoomKey("Equal", "=");
    zoomKey("Equal", "+");
    expect(useLayout.getState().zoom).toBe(1.25);
    const status = screen.getByRole("contentinfo", { name: "Status" });
    expect(status).toHaveTextContent("Zoom 125%");
    zoomKey("Minus", "-");
    expect(useLayout.getState().zoom).toBe(1.1);
    zoomKey("Digit0", "0");
    expect(useLayout.getState().zoom).toBe(1);
    expect(status).not.toHaveTextContent("Zoom");
    // clicking the readout resets too
    zoomKey("Minus", "-");
    await user.click(within(status).getByRole("button", { name: "Zoom 90%" }));
    expect(useLayout.getState().zoom).toBe(1);
  });

  it("the sidebar resizes from its splitter, remembers the width, and double-click resets it", async () => {
    const user = userEvent.setup();
    const { container } = render(<App />);
    const shell = container.querySelector(".shell") as HTMLElement;
    const splitter = screen.getByRole("separator", { name: "Resize sidebar" });
    expect(shell.style.getPropertyValue("--sidebar-w")).toBe(`${SIDEBAR_W.default}px`);
    splitter.focus();
    await user.keyboard("{ArrowRight}{ArrowRight}");
    expect(shell.style.getPropertyValue("--sidebar-w")).toBe(`${SIDEBAR_W.default + 32}px`);
    expect(JSON.parse(localStorage.getItem("pgwhy.layout")!).sidebarWidth).toBe(SIDEBAR_W.default + 32);
    await user.keyboard("{Home}");
    expect(splitter).toHaveAttribute("aria-valuenow", String(SIDEBAR_W.min));
    await user.dblClick(splitter);
    expect(shell.style.getPropertyValue("--sidebar-w")).toBe(`${SIDEBAR_W.default}px`);
  });

  it("opens a workflow run view as a docked tab while the explorer stays interactive", async () => {
    const user = userEvent.setup();
    let treeClicks = 0;
    registerView("explorer", () => (
      <button type="button" onClick={() => treeClicks++}>
        tree node
      </button>
    ));
    render(<App />);
    useWorkspace.getState().openTab({ id: "run-1", kind: "workflow-run", title: "Refresh local" });
    expect(await screen.findByRole("tab", { name: /Refresh local/ })).toHaveAttribute("aria-selected", "true");
    // not modal: the tree is still there and clickable mid-run
    await user.click(screen.getByRole("button", { name: "tree node" }));
    expect(treeClicks).toBe(1);
    // navigating away and back keeps the run tab open
    await user.click(screen.getByRole("tab", { name: /Welcome/ }));
    await user.click(screen.getByRole("tab", { name: /Refresh local/ }));
    expect(screen.getByRole("tab", { name: /Refresh local/ })).toHaveAttribute("aria-selected", "true");
  });

  it("closing the active tab activates its neighbour", async () => {
    const user = userEvent.setup();
    render(<App />);
    useWorkspace.getState().openTab({ id: "a", kind: "editor", title: "Query 1" });
    useWorkspace.getState().openTab({ id: "b", kind: "editor", title: "Query 2" });
    await user.click(await screen.findByRole("button", { name: "Close Query 2" }));
    expect(screen.getByRole("tab", { name: /Query 1/ })).toHaveAttribute("aria-selected", "true");
  });
});
