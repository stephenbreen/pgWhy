// e2e golden flows: the debug app built with `--features e2e` (embedded WebDriver), an
// isolated config/data dir, in-memory credentials, and a throwaway Postgres container.

import { execSync } from "node:child_process";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { browser } from "@wdio/globals";

const root = resolve(import.meta.dirname, "..");
const binary = join(root, "target", "debug", process.platform === "win32" ? "pgwhy.exe" : "pgwhy");
const sandbox = mkdtempSync(join(tmpdir(), "pgwhy-e2e-"));
let container = "";
let remote = "";

function startPg(db: string, seed: string): { id: string; port: string } {
  const id = execSync(
    `docker run -d --rm -e POSTGRES_USER=pgwhy -e POSTGRES_PASSWORD=pgwhy-e2e -e POSTGRES_DB=${db} -p 127.0.0.1::5432 postgres:16-alpine`,
  )
    .toString()
    .trim();
  const port = execSync(`docker port ${id} 5432/tcp`).toString().trim().split(":").pop()!;
  // the entrypoint's init server answers and then restarts: retry the seed over TCP until
  // the real server takes it
  for (let i = 0; ; i++) {
    try {
      execSync(`docker exec ${id} psql -h 127.0.0.1 -U pgwhy -d ${db} -v ON_ERROR_STOP=1 -c "${seed}"`, { stdio: "ignore" });
      break;
    } catch (e) {
      if (i > 60) throw e;
      execSync("sleep 1");
    }
  }
  return { id, port };
}

process.env.PGWHY_CONFIG_DIR = join(sandbox, "config");
process.env.PGWHY_DATA_DIR = join(sandbox, "data");
process.env.PGWHY_SESSION_SECRETS = "1";

export const config: WebdriverIO.Config = {
  runner: "local",
  specs: ["./specs/**/*.e2e.ts"],
  maxInstances: 1,
  framework: "mocha",
  reporters: ["spec"],
  mochaOpts: { timeout: 120_000 },
  logLevel: "warn",
  waitforTimeout: 20_000,
  services: [["@wdio/tauri-service", {}]],
  capabilities: [
    {
      browserName: "tauri",
      "tauri:options": { application: binary },
    } as WebdriverIO.Capabilities,
  ],
  onPrepare() {
    const local = startPg(
      "shop",
      "create table customers (id int primary key, email text not null, name text); " +
        "insert into customers select g, 'c' || g || '@shop.test', 'Customer ' || g from generate_series(1, 500) g;",
    );
    container = local.id;
    process.env.E2E_PG_PORT = local.port;
    process.env.E2E_PG_CONTAINER = local.id;
    const rem = startPg(
      "shop",
      "create table orders (id int primary key, total numeric(10,2)); insert into orders select g, g * 1.5 from generate_series(1, 2000) g;",
    );
    remote = rem.id;
    process.env.E2E_REMOTE_PORT = rem.port;
  },
  async afterTest(test, _ctx, { passed }) {
    if (!passed && process.env.E2E_SHOTS) {
      await browser.saveScreenshot(join(process.env.E2E_SHOTS, `${test.title.replace(/\W+/g, "_")}.png`));
    }
  },
  onComplete() {
    for (const c of [container, remote]) if (c) execSync(`docker rm -f ${c}`, { stdio: "ignore" });
  },
};
