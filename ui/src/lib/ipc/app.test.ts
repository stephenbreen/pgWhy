import { mockIPC } from "@tauri-apps/api/mocks";
import { expect, it } from "vitest";
import { appInfo } from "./app";

it("app_info goes over IPC (mockIPC smoke)", async () => {
  mockIPC((cmd) => (cmd === "app_info" ? { name: "pgWhy", version: "0.1.0" } : undefined));
  await expect(appInfo()).resolves.toEqual({ name: "pgWhy", version: "0.1.0" });
});
