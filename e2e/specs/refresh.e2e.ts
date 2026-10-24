// A Refresh run through the real UI: a Production-tagged remote, a Local target, a saved Refresh
// workflow, the preflight secret prompt (session-only credentials don't survive a restart),
// one confirmation, and a successful staging-then-swap run.

import { browser, $, expect } from "@wdio/globals";
import { execSync } from "node:child_process";

// hidden tabs stay mounted (other specs' editors have their own "Save"), so scope to the form
async function fill(form: string, label: string, value: string) {
  const input = await $(`form[aria-label="${form}"]`).$(`aria/${label}`);
  await input.clearValue();
  await input.setValue(value);
}

// The embedded driver sets <select> values without the change event React listens for;
// pick the option the way a user's choice reaches React.
async function choose(label: string, text: string) {
  await browser.execute(
    (l, t) => {
      const sel = document.querySelector<HTMLSelectElement>(`select[aria-label="${l}"]`)!;
      const opt = [...sel.options].find((o) => o.text === t)!;
      Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, "value")!.set!.call(sel, opt.value);
      sel.dispatchEvent(new Event("change", { bubbles: true }));
    },
    label,
    text,
  );
}

describe("Refresh local from remote", () => {
  it("creates the Production remote connection", async () => {
    await $("aria/New connection").click();
    const f = "Connection";
    await fill(f, "Name", "E2E remote");
    await fill(f, "Host", "127.0.0.1");
    await fill(f, "Port", process.env.E2E_REMOTE_PORT!);
    await fill(f, "Database", "shop");
    await fill(f, "User", "pgwhy");
    await fill(f, "Password", "pgwhy-e2e");
    await expect($('form[aria-label="Connection"]').$("aria/Production")).toBeSelected();
    await $('form[aria-label="Connection"]').$("button=Save").click();
    await expect($("aria/E2E remote")).toBeDisplayed();
  });

  it("saves a workflow; Production can't be the target", async () => {
    await $("aria/Workflows").click();
    await $("aria/New workflow").click();
    await fill("Workflow", "Name", "Refresh e2e");
    await choose("Source connection", "E2E remote (production)");
    await expect($("option*=E2E remote (production) — only Local or Dev")).toBeDisabled();
    await choose("Target connection", "E2E local (local)");
    await $('form[aria-label="Workflow"]').$("button=Save").click();
    await expect($("span=Refresh e2e")).toBeDisplayed();
  });

  it("runs after one confirmation and replaces the local database", async () => {
    await $(".workflow-item").$("button=Run").click();
    // session-only credentials: if the app restarted since the Local connection was created,
    // preflight asks for its password before the confirmation can run
    const confirm = await $("button=Replace shop");
    await browser.waitUntil(async () => (await confirm.isDisplayed()) || (await $("aria/Database password for E2E local").isDisplayed()), { timeout: 30_000 });
    const prompt = await $("aria/Database password for E2E local");
    if (await prompt.isDisplayed()) {
      await prompt.setValue("pgwhy-e2e");
      await $("button=Use").click();
    }
    await confirm.waitForEnabled({ timeout: 30_000 });
    await expect($("aria/Confirmation")).toHaveText(expect.stringContaining("Replace shop on"));
    await confirm.click();
    await expect($(".run-status=Succeeded")).toBeDisplayed({ wait: 120_000 });
    if (process.env.E2E_SHOTS) await browser.saveScreenshot(`${process.env.E2E_SHOTS}/refresh-succeeded.png`);

    const port = process.env.E2E_PG_PORT!;
    void port;
    const count = execSync(`docker exec ${process.env.E2E_PG_CONTAINER} psql -h 127.0.0.1 -U pgwhy -d shop -tAc "select count(*) from orders"`)
      .toString()
      .trim();
    expect(count).toBe("2000");
    void browser;
  });
});
