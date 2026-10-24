// Golden flow: create a connection, browse to a table's columns, run a query,
// view and edit top rows — all through the real UI against a real Postgres.

import { browser, $, expect } from "@wdio/globals";

const port = () => process.env.E2E_PG_PORT!;

async function fill(label: string, value: string) {
  const input = await $(`aria/${label}`);
  await input.clearValue();
  await input.setValue(value);
}

describe("pgWhy golden flow", () => {
  it("creates a Local connection and connects", async () => {
    await $("aria/New connection").click();
    await fill("Name", "E2E local");
    await fill("Host", "127.0.0.1");
    await fill("Port", port());
    await fill("Database", "shop");
    await fill("User", "pgwhy");
    await fill("Password", "pgwhy-e2e");
    await $("aria/Local").click();
    await $("button=Save").click();
    await $("aria/E2E local").click();
    await expect($("aria/public.customers")).toBeDisplayed({ wait: 20_000 });
  });

  it("shows a table's columns and types in one click", async () => {
    const node = await $("aria/public.customers");
    await node.$(".twisty").click();
    await expect($("aria/email text")).toBeDisplayed();
  });

  it("runs a query and shows the results grid", async () => {
    await browser.keys([process.platform === "darwin" ? "Meta" : "Control", "n"]);
    const editor = await $(".cm-content");
    await editor.click();
    await editor.addValue("select count(*) as n from customers;");
    await $("button*=Run").click();
    await expect($('[role="gridcell"]*=500')).toBeDisplayed({ wait: 20_000 });
  });

  it("views top rows and commits an edit", async () => {
    // the details dock offers the table's actions (the tree double-click does the same)
    await $("button=View top rows").click();
    const cell = await $('[role="gridcell"]=Customer 1');
    await cell.waitForDisplayed({ timeout: 20_000 });
    // select the cell and press Enter (SSMS-style F2/Enter editing; the embedded WebDriver
    // doesn't synthesize dblclick in WKWebView)
    await cell.click();
    await browser.keys("Enter");
    const input = await $("aria/Cell value");
    await input.clearValue();
    await input.setValue("Renamed");
    await browser.keys("Enter");
    await expect($("aria/Pending changes")).toHaveText(expect.stringContaining("1 pending change"));
    await $("button=Commit").click();
    await expect($("span*=Committed: 1 updated")).toBeDisplayed({ wait: 20_000 });
    // the committed value is what the re-queried grid shows (rows stay in key order)
    await expect($('[role="gridcell"]=Renamed')).toBeDisplayed({ wait: 20_000 });
  });
});
