// Local perf suite — runs only with E2E_PERF=1. Measures, in the
// real WKWebView: tree expand/collapse on cached data, and a 1M-row result's first rows and
// fling-scroll frame times through the windowed grid.

import { browser, $, expect } from "@wdio/globals";
import { writeFileSync } from "node:fs";

const enabled = process.env.E2E_PERF === "1";
const out: Record<string, unknown> = {};

(enabled ? describe : describe.skip)("perf budgets", () => {
  after(() => {
    if (process.env.E2E_PERF_OUT) writeFileSync(process.env.E2E_PERF_OUT, JSON.stringify(out, null, 2));
  });

  it("tree expand/select on cached data < 100 ms", async () => {
    // the Explorer button toggles the sidebar when it's already the active view
    if (!(await $('[aria-label="Database objects"]').isDisplayed())) await $("aria/Explorer").click();
    // a fresh app instance (new session) has to reconnect; session-only passwords are re-asked
    const customersRow = '[role="treeitem"][aria-label="public.customers"][data-connection="E2E local"]';
    if (!(await $(customersRow).isDisplayed())) {
      await $("aria/E2E local").click();
      const pw = await $("aria/Password for E2E local");
      if (await pw.waitForDisplayed({ timeout: 5_000 }).catch(() => false)) {
        await pw.setValue("pgwhy-e2e");
        await $("button=Connect").click();
      }
    }
    await expect($(customersRow)).toBeDisplayed({ wait: 20_000 });
    const samples = (await browser.executeAsync(`
      const done = arguments[arguments.length - 1];
      (async () => {
        const res = [];
        for (let i = 0; i < 30; i++) {
          // scoped to one connection: other specs' connections are in the same flat tree list
          const find = () => document.querySelector('[role="treeitem"][aria-label="public.customers"][data-connection="E2E local"]');
          const node = find();
          const twisty = node && node.querySelector(".twisty");
          if (!twisty) break;
          // click → DOM committed (rAF is throttled while the window is in the background)
          const before = node.getAttribute("aria-expanded");
          const t0 = performance.now();
          await new Promise((r) => {
            // bounded: a collapsing row can scroll out of the virtualized list, and the sample
            // should still be recorded rather than hanging the run
            const timer = setTimeout(() => { mo.disconnect(); r(); }, 1000);
            const mo = new MutationObserver(() => {
              const now = find();
              if (now && now.getAttribute("aria-expanded") !== before) { clearTimeout(timer); mo.disconnect(); r(); }
            });
            mo.observe(document.body, { subtree: true, attributes: true, childList: true });
            twisty.click();
          });
          res.push(performance.now() - t0);
          await new Promise((r) => setTimeout(r, 30));
        }
        done(res);
      })();
    `)) as number[];
    const sorted = [...samples].sort((a, b) => a - b);
    out.tree = { samples: samples.length, p50: sorted[Math.floor(sorted.length / 2)], p95: sorted[Math.floor(sorted.length * 0.95)], max: sorted.at(-1) };
    expect(sorted[Math.floor(sorted.length * 0.95)]).toBeLessThan(100);
  });

  it("1M-row result: first rows and fling scroll", async () => {
    await browser.keys([process.platform === "darwin" ? "Meta" : "Control", "n"]);
    const tab = () => $("section[role=tabpanel]:not([hidden])");
    const editor = await tab().$(".cm-content");
    await editor.click();
    await editor.addValue("select g as id, md5(g::text) as hash, g * 1.5 as amount, now() - g * interval '1 minute' as at, (g % 7 = 0) as flag from generate_series(1, 1000000) g;");
    const t0 = Date.now();
    await tab().$("button*=Run").click();
    await tab().$('[role="gridcell"]=1').waitForDisplayed({ timeout: 60_000 });
    out.first_rows_ms = Date.now() - t0;
    await tab().$("span*=1,000,000 rows").waitForDisplayed({ timeout: 120_000 });
    out.complete_ms = Date.now() - t0;

    out.scroll = await browser.executeAsync(`
      const done = arguments[arguments.length - 1];
      (async () => {
        try { await window.__TAURI__.window.getCurrentWindow().setFocus(); } catch (e) {}
        const el = document.querySelector("section[role=tabpanel]:not([hidden]) .grid-scroll");
        const res = {};
        // frame timing needs a visible, foreground window: WebKit throttles rAF otherwise
        const probe = await Promise.race([
          new Promise((r) => requestAnimationFrame(() => r(true))),
          new Promise((r) => setTimeout(() => r(false), 1000)),
        ]);
        if (!probe) { done({ skipped: "window not visible: requestAnimationFrame is throttled; run with the app in front" }); return; }
        for (const speed of [2000, 8000]) {
          el.scrollTop = 0;
          await new Promise((r) => setTimeout(r, 300));
          const deltas = [];
          let prev = performance.now();
          let frames = 0;
          await new Promise((resolve) => {
            function step(t) {
              deltas.push(t - prev);
              prev = t;
              el.scrollTop += speed;
              if (++frames >= 300 || el.scrollTop + el.clientHeight >= el.scrollHeight - 1) resolve();
              else requestAnimationFrame(step);
            }
            requestAnimationFrame(step);
          });
          const sorted = [...deltas].sort((a, b) => a - b);
          const median = sorted[Math.floor(sorted.length / 2)];
          const total = deltas.reduce((a, b) => a + b, 0);
          res["px" + speed] = {
            frames: deltas.length,
            fps: +(deltas.length / (total / 1000)).toFixed(1),
            dropped_pct: +((100 * deltas.filter((d) => d > median * 1.5).length) / deltas.length).toFixed(1),
            worst_gap_ms: +sorted[sorted.length - 1].toFixed(1),
          };
        }
        done(res);
      })();
    `);
  });
});
