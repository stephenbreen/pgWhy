// Promo capture (E2E_PROMO=1): drives the real app and records *motion*, not stills.
//
// Each beat performs the interaction a frame at a time and grabs a screenshot after every step,
// so typing, scrolling and panels opening are real. The app's own driver serves the frames
// (WebDriver screenshots of the webview): no OS screen recording, no desktop, no permissions.
// ~10-16 fps at 3200x1800 (exactly 16:9 at 2x), with a timestamp per frame so
// `scripts/promo.py` can cut on real timing instead of guessing.
//
// Output: E2E_PROMO_DIR/<beat>/0000.png … plus manifest.json (beat → frames + ms offsets).

import { $, browser, expect } from "@wdio/globals";
import { execSync } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import {
  activePanel,
  choose,
  clickMenu,
  connectTo,
  createConnection,
  expectReportDone,
  fillIn,
  form,
  showView,
  treeRow,
} from "../helpers";

const enabled = process.env.E2E_PROMO === "1";
const out = process.env.E2E_PROMO_DIR ?? "/tmp/pgwhy-promo/frames";
const localPort = process.env.E2E_PG_PORT!;
const remotePort = process.env.E2E_REMOTE_PORT!;

const local = "Local dev";
const remote = "Staging (read-only)";

const SEED = `
drop schema if exists demo cascade;
create schema demo;
create table demo.orders (
  id bigserial primary key,
  customer_id int not null,
  placed_at timestamptz not null,
  status text not null,
  total numeric(10,2) not null,
  items jsonb not null,
  note text
);
insert into demo.orders (customer_id, placed_at, status, total, items, note)
select 1 + g % 500,
       now() - (g || ' minutes')::interval,
       (array['paid','shipped','pending','refunded'])[1 + g % 4],
       round((random() * 500)::numeric, 2),
       jsonb_build_object('sku', 'SKU-' || g, 'qty', 1 + g % 5),
       case when g % 7 = 0 then 'gift wrap; leave at door' end
from generate_series(1, 20000) g;
create unique index orders_customer_placed_idx on demo.orders (customer_id, placed_at);
analyze demo.orders;
`;

// ---------------------------------------------------------------------------------- recorder

interface Beat {
  name: string;
  /** ms offset of each frame from the beat's first frame */
  offsets: number[];
}

const beats: Beat[] = [];
let current: Beat | null = null;
let started = 0;

function begin(name: string) {
  mkdirSync(`${out}/${name}`, { recursive: true });
  current = { name, offsets: [] };
  started = Date.now();
}

/** Grab one frame of the current beat. Call it after each small UI step. */
async function grab() {
  const beat = current;
  if (!beat) throw new Error("grab() outside a beat");
  const i = beat.offsets.length;
  beat.offsets.push(Date.now() - started);
  await browser.saveScreenshot(`${out}/${beat.name}/${String(i).padStart(4, "0")}.png`);
}

function end() {
  if (!current) throw new Error("end() outside a beat");
  beats.push(current);
  // eslint-disable-next-line no-console
  console.log(`beat ${current.name}: ${current.offsets.length} frames`);
  current = null;
}

/** Type text in chunks, one frame per chunk, so it reads as typing. */
async function typeInto(chunked: (chunk: string) => Promise<void>, text: string, chunks: number) {
  const size = Math.max(1, Math.ceil(text.length / chunks));
  for (let i = 0; i < text.length; i += size) {
    await chunked(text.slice(i, i + size));
    await grab();
  }
}

/**
 * Wait for `predicate`, grabbing a frame every ~120 ms while it is false: the thing appearing is
 * the motion (a dialog opening, a commit landing), so the wait becomes part of the shot.
 */
async function grabUntil(predicate: () => Promise<boolean>, timeoutMs = 60_000, every = 120) {
  const startedAt = Date.now();
  while (Date.now() - startedAt < timeoutMs) {
    if (await predicate()) {
      await grab();
      return;
    }
    await grab();
    await browser.pause(every);
  }
  throw new Error("grabUntil timed out");
}

/** Scroll an element by `dy` pixels per frame — the driver paces it, so the capture sees every step. */
async function scrollBy(selector: string, dy: number, steps: number) {
  for (let i = 0; i < steps; i++) {
    await browser.execute(
      (sel: string, delta: number) => {
        const el = document.querySelector(sel) as HTMLElement | null;
        if (!el) throw new Error(`no element ${sel}`);
        el.scrollTop += delta;
      },
      selector,
      dy,
    );
    await grab();
  }
}

// ---------------------------------------------------------------------------------- the reel

const suite = enabled ? describe : describe.skip;

suite("promo capture", function () {
  this.timeout(900_000);

  before(async () => {
    mkdirSync(out, { recursive: true });
    // exactly 16:9: the capture is 2x the CSS size, so 1600x900 gives 3200x1800 with no bars
    await browser.setWindowSize(1600, 900).catch(() => {});
    execSync(
      `docker exec -i ${process.env.E2E_PG_CONTAINER} psql -h 127.0.0.1 -U pgwhy -d shop -v ON_ERROR_STOP=1 -f -`,
      { input: SEED, stdio: ["pipe", "ignore", "inherit"] },
    );
    await createConnection({ name: local, port: localPort, database: "shop", tag: "Local" });
    await connectTo(local);
    await expect(treeRow("shop", local)).toBeDisplayed({ wait: 20_000 });
  });

  after(() => {
    writeFileSync(`${out}/manifest.json`, JSON.stringify({ beats }, null, 2));
  });

  it("records the tour", async () => {
    // ---- 1. tree: expand to a table's columns, with its rows open in the middle ----------
    begin("tree");
    await grab();
    await (await treeRow("demo", local)).$(".twisty").click();
    await grab();
    await grab();
    await (await treeRow("Tables", local)).$(".twisty").click();
    await grab();
    await grab();
    const orders = await treeRow("demo.orders", local);
    await orders.click();
    await grab();
    await (await form("Details")).$("button=View top rows").click();
    const topRows = await activePanel();
    await expect(topRows.$('[role="gridcell"]')).toBeDisplayed({ wait: 30_000 });
    await grab();
    await grab();
    await (await treeRow("demo.orders", local)).$(".twisty").click();
    await expect(treeRow("note text", local)).toBeDisplayed({ wait: 20_000 });
    await grab();
    await grab();
    end();

    // ---- 2. query: type it, run it, watch it fill --------------------------------------
    begin("query");
    await clickMenu("demo", "New query", local);
    const editorPanel = await activePanel();
    const editor = await editorPanel.$(".cm-content");
    await editor.waitForExist({ timeout: 20_000 });
    await grab();
    await editor.click();
    await typeInto(
      (chunk) => editor.addValue(chunk),
      "select o.id, o.status, o.total, o.placed_at, o.items\nfrom demo.orders o\nwhere o.total > 100\norder by o.placed_at desc\nlimit 300;",
      22,
    );
    await grab();
    await editorPanel.$("button*=Run").click();
    await expect(editorPanel.$('[role="gridcell"]')).toBeDisplayed({ wait: 60_000 });
    await grab();
    await grab();
    await grab();
    end();

    // ---- 3. a million rows: the stream itself (not used in the cut) then the scroll ----
    begin("million-run");
    await clickMenu("demo", "New query", local);
    const bigPanel = await activePanel();
    const bigEditor = await bigPanel.$(".cm-content");
    await bigEditor.waitForExist({ timeout: 20_000 });
    await bigEditor.click();
    await typeInto(
      (chunk) => bigEditor.addValue(chunk),
      "select g as id, md5(g::text) as hash, now() - (g || ' seconds')::interval as seen_at from generate_series(1, 1000000) g;",
      14,
    );
    await bigEditor.isDisplayed(); // settle
    await bigPanel.$("button*=Run").click();
    // catch the stream climbing: a frame every ~120 ms until the count stops moving
    for (let i = 0; i < 26; i++) {
      await grab();
      await browser.pause(60);
    }
    await browser.waitUntil(
      async () => {
        const s = await bigPanel.$(".grid-status");
        return (await s.isExisting()) && (await s.getText()).includes("1,000,000 rows");
      },
      { timeout: 180_000, timeoutMsg: "the 1M-row result never finished streaming" },
    );
    end();

    begin("million-scroll");
    await grab();
    await grab();
    // a paced scroll: ~70 px per frame reads as smooth motion even at 10 fps
    await scrollBy(".tab-panel:not([hidden]) .grid-scroll", 70, 44);
    end();

    // ---- 4. edit a cell, watch it pend, commit it ---------------------------------------
    begin("edit");
    await showView("Explorer");
    await (await treeRow("demo.orders", local)).click();
    await grab();
    await (await form("Details")).$("button=View top rows").click();
    const grid = await activePanel();
    const cell = await grid.$('[role="gridcell"]=paid');
    await grabUntil(async () => cell.isDisplayed().catch(() => false), 30_000);
    await grab();
    await cell.click();
    await browser.keys("Enter");
    await grab();
    const input = await grid.$("aria/Cell value");
    await input.clearValue();
    await grab();
    await typeInto((chunk) => input.addValue(chunk), "refunded", 5);
    await browser.keys("Enter");
    await grab();
    await expect(grid.$("aria/Pending changes")).toHaveText(
      expect.stringContaining("1 pending change"),
    );
    await grab();
    await grab();
    end();

    begin("commit");
    await grid.$("button=Commit").click();
    const committed = grid.$("span*=Committed: 1 updated");
    await grabUntil(async () => committed.isDisplayed().catch(() => false), 30_000, 90);
    await grab();
    await grab();
    end();

    // ---- 5. the guardrail: type the name to drop a database ----------------------------
    begin("guardrail");
    await showView("Explorer");
    await clickMenu("shop", "Drop database…", local);
    const dropDialog = await form("Drop shop");
    await grabUntil(async () => dropDialog.isDisplayed().catch(() => false), 20_000, 100);
    const confirmBox = await dropDialog.$('input[aria-label="Type the database name to confirm"]');
    await confirmBox.click();
    await grab();
    await typeInto((chunk) => confirmBox.addValue(chunk), "sho", 3);
    await grab();
    await grab();
    end();
    await dropDialog.$("button=Cancel").click();

    // ---- 6. fill the history, so the timeline has something to show --------------------
    await showView("Explorer");
    const backUp = async (row: string, item: string) => {
      await clickMenu(row, item, local);
      await $(".backup-form").$("button=Back up").click();
      const panel = await activePanel();
      await expect(panel.$("div.ok-note")).toBeDisplayed({ wait: 120_000 });
    };
    await backUp("demo", "Back up this schema…");
    await backUp("demo.orders", "Back up this table…");
    await backUp("shop", "Back up…");
    await showView("Workflows");
    await $("aria/All history").click();
    const backupRow = await $('tr[data-kind="backup"]');
    await backupRow.waitForDisplayed({ timeout: 20_000 });
    await backupRow.$("button=Restore backup…").click();
    const restoreDialog = await form("Restore a backup");
    await expect(restoreDialog).toHaveText(expect.stringContaining("dumped by pg_dump"), {
      wait: 30_000,
    });
    await fillIn("Restore a backup", "New database name", "demo_restore");
    await restoreDialog.$("button=Restore").click();
    await expectReportDone("Restore a backup", "Restored");
    await restoreDialog.$("button=Close").click();
    await showView("Explorer");
    await clickMenu("shop", "New database…", local);
    await fillIn("New database", "Database name", "demo_tmp");
    await (await form("New database")).$("button=Create").click();
    await expectReportDone("New database");
    await (await form("New database")).$("button=Close").click();
    await (await treeRow("demo_tmp", local)).waitForExist({ timeout: 20_000 });
    await clickMenu("demo_tmp", "Drop database…", local);
    await fillIn("Drop demo_tmp", "Type the database name to confirm", "demo_tmp");
    await (await form("Drop demo_tmp")).$("button=Drop database").click();
    await expectReportDone("Drop demo_tmp");
    await (await form("Drop demo_tmp")).$("button=Close").click();

    // ---- 7. the marquee flow: one confirmation, and the run that follows ----------------
    await createConnection({ name: remote, port: remotePort, database: "shop", tag: "Staging" });
    await showView("Workflows");
    await $("aria/New workflow").click();
    await fillIn("Workflow", "Name", "Refresh from staging");
    await choose("Source connection", `${remote} (staging)`);
    await choose("Target connection", `${local} (local)`);
    await (await form("Workflow")).$("button=Save").click();
    const run = await $(
      '//li[.//span[text()="Refresh from staging"]]//button[normalize-space()="Run"]',
    );
    await run.waitForDisplayed({ timeout: 20_000 });

    begin("confirm");
    await run.click();
    await grabUntil(
      async () => (await $("aria/Confirmation")).isDisplayed().catch(() => false),
      90_000,
      160,
    );
    const pw = await $("aria/Database password for Staging (read-only)");
    if (await pw.isDisplayed().catch(() => false)) {
      await pw.setValue("pgwhy-e2e");
      await grab();
      await $("button=Use").click();
    }
    const replace = await $("button=Replace shop");
    await replace.waitForEnabled({ timeout: 60_000 });
    await grab();
    await grab();
    end();

    // ---- 8. history, with the finished run in it ---------------------------------------
    await replace.click();
    await expect($(".run-status=Succeeded")).toBeDisplayed({ wait: 180_000 });
    begin("history");
    await showView("Workflows");
    await $("aria/All history").click();
    await (await activePanel()).$("button=Refresh").click();
    const refreshRow = await $('tr[data-kind="refresh"]');
    await grabUntil(async () => refreshRow.isDisplayed().catch(() => false), 30_000, 120);
    await grab();
    await scrollBy(".tab-panel:not([hidden]) .history-table", 60, 6);
    end();
  });
});
