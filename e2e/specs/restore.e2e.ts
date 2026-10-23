// Backup → history → restore, through the real UI (v1.1 Epic 2 + 3): a backup taken from the tree
// becomes a history row whose "Restore backup…" action feeds the archive straight into the restore
// dialog (no native file picker), which restores it into a new database; then a backup of that
// copy is restored over it with the replace mode, which keeps the name.
//
// Order-independent on purpose: the Refresh spec replaces the seeded `shop` database, so this
// spec seeds its own probe table in `postgres` and never assumes what `shop` contains.

import { $, browser, expect } from "@wdio/globals";
import {
  activePanel,
  clickMenu,
  connectTo,
  createConnection,
  expectReportDone,
  fill,
  form,
  runQuery,
  showView,
  treeRow,
} from "../helpers";

const port = () => process.env.E2E_PG_PORT!;
const conn = "E2E restore";
const probeRows = 25;

/** The newest backup row in the history whose text mentions `database`. */
async function backupRow(database: string) {
  const rows = await $$('tr[data-kind="backup"]');
  for (const row of rows) {
    if ((await row.getText()).includes(database)) return row;
  }
  throw new Error(`no backup row for ${database} in the history`);
}

async function openHistory() {
  await showView("Workflows");
  await $("aria/All history").click();
  // the tab stays mounted; its own Refresh makes the list current whatever happened since
  await (await activePanel()).$("button=Refresh").click();
}

async function restoreFromHistory(database: string) {
  await openHistory();
  const row = await backupRow(database);
  await row.$("button=Restore backup…").click();
  const dialog = await $('[role="dialog"][aria-label="Restore a backup"]');
  await expect(dialog).toBeDisplayed();
  // the archive is preselected and inspected: no native file picker in this flow
  await expect(dialog).toHaveText(expect.stringContaining("dumped by pg_dump"));
  return dialog;
}

async function backUp(database: string) {
  await showView("Explorer");
  await clickMenu(database, "Back up…", conn);
  await expect($("aria/Backup format")).toHaveValue("custom");
  await $(".backup-form").$("button=Back up").click();
  // scope to the active tab: other tabs stay mounted, and one of them may show an error note
  const panel = await activePanel();
  await browser.waitUntil(
    async () => (await panel.$("div.ok-note").isExisting()) || (await panel.$(".error-note").isExisting()),
    { timeout: 120_000, timeoutMsg: `the backup of ${database} neither finished nor failed` },
  );
  if (await panel.$(".error-note").isExisting()) {
    throw new Error(`backup of ${database} failed: ${await panel.$(".error-note").getText()}`);
  }
  await expect(panel.$("div.ok-note")).toHaveText(expect.stringContaining("Backed up"));
}

describe("backup and restore", () => {
  before(async () => {
    await createConnection({ name: conn, port: port(), database: "shop", tag: "Local" });
    await connectTo(conn);
    await expect(treeRow("shop", conn)).toBeDisplayed({ wait: 20_000 });
    // a probe table nothing else in the suite touches
    await runQuery(
      "postgres",
      `drop table if exists pgwhy_e2e_probe; create table pgwhy_e2e_probe (id int primary key); insert into pgwhy_e2e_probe select g from generate_series(1, ${probeRows}) g;`,
      conn,
    );
    // DDL returns no rows; the count assertion in the restore test proves the seed landed.
  });

  it("backs up a database from the tree", async () => {
    await backUp("postgres");
  });

  it("restores that backup into a new database from the history row", async () => {
    const dialog = await restoreFromHistory("postgres");
    await fill("New database name", "e2e_restored");
    await dialog.$("button=Restore").click();
    await expectReportDone("Restore a backup", "Restored");
    await dialog.$("button=Close").click();

    // the copy is a real database, with the backup's rows
    await showView("Explorer");
    await expect(treeRow("e2e_restored", conn)).toBeDisplayed({ wait: 20_000 });
    await runQuery("e2e_restored", "select count(*) as n from pgwhy_e2e_probe;", conn);
    const panel = await activePanel();
    await expect(panel.$(`[role="gridcell"]=${probeRows}`)).toBeDisplayed({ wait: 60_000 });
  });

  it("replaces a database from a backup of itself, keeping its name", async () => {
    await backUp("e2e_restored");
    const dialog = await restoreFromHistory("e2e_restored");
    await $('input[aria-label="Replace database"]').click();
    await fill("Type the database name to confirm", "e2e_restored");
    await dialog.$("button=Restore").click();
    await expectReportDone("Restore a backup", "Restored");
    await dialog.$("button=Close").click();

    // the name is kept, the staging copy and `_old` are gone
    await showView("Explorer");
    await expect(treeRow("e2e_restored", conn)).toBeDisplayed({ wait: 20_000 });
    await expect(treeRow("e2e_restored_old", conn)).not.toBeExisting();
  });
});
