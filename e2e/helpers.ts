// Shared e2e helpers for the v1.1 flows (lifecycle, backup/restore).
//
// The embedded WebDriver cannot synthesize a right-click on a tree row in WKWebView, and it sets
// <select> values without the change event React listens for, so both go through the DOM — the
// same pragmatic route `refresh.e2e.ts` takes for selects.

import { browser, $, expect } from "@wdio/globals";

/** Fill an input found by its aria-label, anywhere on screen. */
export async function fill(label: string, value: string) {
  const input = await $(`aria/${label}`);
  await input.clearValue();
  await input.setValue(value);
}

/**
 * The visible tab panel. Every open tab stays mounted (hidden), so commands and assertions scope
 * to the active one or they hit an editor from another spec.
 */
export function activePanel() {
  return $(".tab-panel:not([hidden])");
}

/** The form or dialog with this aria-label (hidden tabs stay mounted, so always scope). */
export function form(scope: string) {
  return $(`[aria-label="${scope}"]`);
}

/** Click a button inside the form/dialog with this aria-label. */
export async function clickIn(scope: string, name: string) {
  await (await form(scope)).$(`button=${name}`).click();
}

/** Fill an input inside the form/dialog with this aria-label. */
export async function fillIn(scope: string, label: string, value: string) {
  const input = await (await form(scope)).$(`aria/${label}`);
  await input.clearValue();
  await input.setValue(value);
}

/** Pick a <select> option by its visible text, the way React sees a user's choice. */
export async function choose(label: string, text: string) {
  await browser.execute(
    (l, t) => {
      const sel = document.querySelector<HTMLSelectElement>(`select[aria-label="${l}"]`);
      if (!sel) throw new Error(`no select labelled ${l}`);
      const opt = [...sel.options].find((o) => o.text === t || o.value === t);
      if (!opt) throw new Error(`no option ${t} in ${l}`);
      Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, "value")!.set!.call(sel, opt.value);
      sel.dispatchEvent(new Event("change", { bubbles: true }));
    },
    label,
    text,
  );
}

/**
 * The tree row for `label`, optionally on one connection. `aria/` selectors match accessible
 * names and therefore also plain text (a report step saying "Rename to e2e_renamed" is not a tree
 * row), so presence checks go through this.
 */
export function treeRow(label: string, connection?: string) {
  const scope = connection ? `[data-connection="${connection}"]` : "";
  return $(`[role="treeitem"][aria-label="${label}"]${scope}`);
}

/**
 * Open a tree row's context menu (dispatching the event the row listens for). `connection` scopes
 * the lookup: two connections can show the same database name.
 */
export async function openMenu(rowLabel: string, connection?: string) {
  await browser.execute(
    (l, conn) => {
      const rows = [...document.querySelectorAll(`[aria-label="${CSS.escape(l)}"]`)].filter(
        (el) => !conn || (el as HTMLElement).dataset.connection === conn,
      );
      const row = rows[0];
      if (!row) throw new Error(`no tree row labelled ${l}${conn ? ` on ${conn}` : ""}`);
      row.dispatchEvent(
        new MouseEvent("contextmenu", { bubbles: true, cancelable: true, clientX: 320, clientY: 240 }),
      );
    },
    rowLabel,
    connection,
  );
}

/** The context-menu item with this exact text (menus use an ellipsis: "Drop database…"). */
export async function menuItem(name: string) {
  const menu = await $(".context-menu");
  await menu.waitForDisplayed();
  const item = await menu.$(`button=${name}`);
  await item.waitForDisplayed();
  return item;
}

export async function clickMenu(rowLabel: string, name: string, connection?: string) {
  await openMenu(rowLabel, connection);
  await (await menuItem(name)).click();
}

/** The reason a context-menu item shows when it is disabled, or null when it is enabled. */
export async function menuDisabledReason(
  rowLabel: string,
  name: string,
  connection?: string,
): Promise<string | null> {
  await openMenu(rowLabel, connection);
  const item = await menuItem(name);
  const disabled = await item.getAttribute("disabled");
  const reason = await item.$(".menu-reason");
  const text = (await reason.isExisting()) ? (await reason.getText()).trim() : null;
  await browser.keys("Escape");
  return disabled === null || disabled === "false" ? null : (text ?? "disabled");
}

/** Bring a connection in the sidebar into view and click it (the list scrolls). */
export async function connectTo(name: string) {
  await showView("Explorer");
  const row = await $(`aria/${name}`);
  await row.waitForExist({ timeout: 20_000 });
  await row.scrollIntoView();
  await row.click();
}

/**
 * Close any connection form left open by an earlier step or spec. The app is shared across spec
 * files and every tab stays mounted, so a stale form would swallow the next `fillIn("Connection",
 * …)` and the following Save instead of the new one.
 */
export async function dismissOpenForms() {
  const forms = await $$('[aria-label="Connection"]');
  for (const f of forms) {
    const cancel = await f.$("button=Cancel");
    if (await cancel.isExisting()) await cancel.click().catch(() => {});
  }
}

/** Create and connect a connection entry; `tag` is the environment-tag radio's label. */
export async function createConnection(opts: {
  name: string;
  port: string;
  database: string;
  tag: "Local" | "Dev" | "Staging" | "Production";
}) {
  await showView("Explorer");
  await dismissOpenForms();
  await $("aria/New connection").click();
  await fillIn("Connection", "Name", opts.name);
  await fillIn("Connection", "Host", "127.0.0.1");
  await fillIn("Connection", "Port", opts.port);
  await fillIn("Connection", "Database", opts.database);
  await fillIn("Connection", "User", "pgwhy");
  await fillIn("Connection", "Password", "pgwhy-e2e");
  await (await form("Connection")).$(`aria/${opts.tag}`).click();
  // Scoped: an editor tab left open by an earlier spec also has a "Save" button, and the app is
  // shared across spec files (the embedded driver keeps one process for the whole run).
  await clickIn("Connection", "Save");
  const row = await $(`aria/${opts.name}`);
  await row.waitForExist({ timeout: 20_000 });
  await row.scrollIntoView();
}

/** Connect a saved connection by clicking it in the connection list. */
export async function connect(name: string) {
  await $(`aria/${name}`).click();
}

/** Run `sql` in a fresh editor tab bound to `database`, via the database node's New query. */
export async function runQuery(database: string, sql: string, connection?: string) {
  await clickMenu(database, "New query", connection);
  const panel = await activePanel();
  const editor = await panel.$(".cm-content");
  await editor.waitForExist({ timeout: 20_000 });
  await editor.click();
  await editor.addValue(sql);
  await panel.$("button*=Run").click();
}

/**
 * Wait for a dialog's operation report to say the action finished. The lifecycle dialogs say
 * "Done"; the restore dialog says "Restored".
 */
export async function expectReportDone(scope: string, expected = "Done") {
  const report = await $(`[role="dialog"][aria-label="${scope}"]`).$("div.op-report");
  await report.waitForDisplayed({ timeout: 30_000 });
  await browser.waitUntil(
    async () => (await report.getText()).includes(expected),
    { timeout: 120_000, timeoutMsg: `the ${scope} report never said ${expected}` },
  );
}

/**
 * Bring an activity view to the front. Clicking the active view's icon toggles the sidebar off,
 * so only click when it is not already showing.
 */
export async function showView(label: "Explorer" | "Scripts" | "Workflows") {
  const button = await $(`aria/${label}`);
  if ((await button.getAttribute("aria-pressed")) !== "true") await button.click();
}
