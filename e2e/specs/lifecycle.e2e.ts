// Database lifecycle through the real UI (v1.1 Epic 4): create, rename and drop from the tree,
// and the environment-tag gate that refuses a Production target before any SQL.
//
// Every destructive path ends in an operation report, which is what these assertions read — the
// database list is reloaded by the dialog, so the tree is the second witness.

import { $, expect } from "@wdio/globals";
import {
  clickMenu,
  connectTo,
  form,
  createConnection,
  expectReportDone,
  fill,
  menuDisabledReason,
  treeRow,
} from "../helpers";

const port = () => process.env.E2E_PG_PORT!;
const local = "E2E lifecycle";
const prod = "E2E prod";

describe("database lifecycle", () => {
  before(async () => {
    await createConnection({ name: local, port: port(), database: "shop", tag: "Local" });
    await connectTo(local);
    await expect(treeRow("shop", local)).toBeDisplayed({ wait: 20_000 });
  });

  it("creates a database from the tree", async () => {
    await clickMenu("shop", "New database…", local);
    const dialog = await $('[role="dialog"][aria-label="New database"]');
    await expect(dialog).toBeDisplayed();
    await fill("Database name", "e2e_created");
    await (await form("New database")).$("button=Create").click();
    await expectReportDone("New database");
    await (await form("New database")).$("button=Close").click();
    await expect(treeRow("e2e_created", local)).toBeDisplayed({ wait: 20_000 });
  });

  it("renames it", async () => {
    await clickMenu("e2e_created", "Rename database…", local);
    await expect($('[role="dialog"][aria-label="Rename e2e_created"]')).toBeDisplayed();
    await fill("Database name", "e2e_renamed");
    await (await form("Rename e2e_created")).$("button=Rename").click();
    await expectReportDone("Rename e2e_created");
    await (await form("Rename e2e_created")).$("button=Close").click();
    await expect(treeRow("e2e_renamed", local)).toBeDisplayed({ wait: 20_000 });
    await expect(treeRow("e2e_created", local)).not.toBeExisting();
  });

  it("drops it only after the database name is typed", async () => {
    await clickMenu("e2e_renamed", "Drop database…", local);
    const dialog = await $('[role="dialog"][aria-label="Drop e2e_renamed"]');
    await expect(dialog).toBeDisplayed();
    const submit = await (await form("Drop e2e_renamed")).$("button=Drop database");
    await expect(submit).toBeDisabled();
    await fill("Type the database name to confirm", "e2e_renamed");
    await expect(submit).toBeEnabled();
    await submit.click();
    await expectReportDone("Drop e2e_renamed");
    await (await form("Drop e2e_renamed")).$("button=Close").click();
    await expect(treeRow("e2e_renamed", local)).not.toBeExisting();
  });

  it("refuses destructive actions on a Production-tagged connection", async () => {
    await createConnection({ name: prod, port: port(), database: "shop", tag: "Production" });
    await connectTo(prod);
    await expect(treeRow("shop", local)).toBeDisplayed({ wait: 20_000 });
    // the menu names the tag instead of letting the click through
    const drop = await menuDisabledReason("shop", "Drop database…", prod);
    expect(drop).toContain("production");
    const rename = await menuDisabledReason("shop", "Rename database…", prod);
    expect(rename).toContain("production");
  });
});
