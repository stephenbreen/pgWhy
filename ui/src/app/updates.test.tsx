import { act, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { mockIPC } from "@tauri-apps/api/mocks";
import { beforeEach, expect, test } from "vitest";
import { App } from "./App";
import { UpdatesSection, useUpdates } from "./updates";
import { setCloseGuard } from "./workspace";

const start = useUpdates.getState();
const release = { version: "0.2.0", current_version: "0.1.0", notes: "Save as for backups" };
const failure = (message: string) => ({ kind: "network", class: "retriable", message });

function backend(opts: { check?: unknown; restart?: unknown; download?: unknown } = {}) {
  const calls: string[] = [];
  mockIPC((cmd, args) => {
    calls.push(cmd);
    if (cmd === "update_check") {
      if (opts.check instanceof Error) throw failure(opts.check.message);
      return opts.check ?? null;
    }
    if (cmd === "update_download") {
      if (opts.download) throw opts.download;
      (args as { events: { onmessage: (p: unknown) => void } }).events.onmessage({ downloaded: 1024, total: 2048 });
      return null;
    }
    if (cmd === "update_restart") {
      if (opts.restart) throw opts.restart;
      return null;
    }
    return null;
  });
  return calls;
}

beforeEach(() => {
  useUpdates.setState(start, true);
  setCloseGuard("t1", null);
});

test("a check the user asked for says when there is nothing new", async () => {
  backend();
  render(<UpdatesSection currentVersion="0.1.0" />);
  expect(screen.getByText("You have 0.1.0.")).toBeInTheDocument();
  await userEvent.click(screen.getByRole("button", { name: /check for updates/i }));
  expect(await screen.findByText(/up to date/i)).toBeInTheDocument();
});

test("a failed check is reported when asked for, and silent at startup", async () => {
  backend({ check: new Error("offline") });
  render(<UpdatesSection />);
  await act(() => useUpdates.getState().check(true));
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  await userEvent.click(screen.getByRole("button", { name: /check for updates/i }));
  expect(await screen.findByRole("alert")).toHaveTextContent("offline");
});

test("a found update shows in the status bar, which opens Settings", async () => {
  backend({ check: release });
  render(<App />);
  expect(screen.queryByText(/is available/)).not.toBeInTheDocument();
  await act(() => useUpdates.getState().check(true));
  await userEvent.click(screen.getByRole("button", { name: "pgWhy 0.2.0 is available" }));
  expect(screen.getByRole("tab", { name: /settings/i })).toBeInTheDocument();
});

test("updating downloads, then restarts", async () => {
  const calls = backend({ check: release });
  render(<UpdatesSection />);
  await act(() => useUpdates.getState().check(true));
  expect(screen.getByText("Save as for backups")).toBeInTheDocument();
  await userEvent.click(screen.getByRole("button", { name: /update and restart/i }));
  expect(await screen.findByText(/restarting/i)).toBeInTheDocument();
  expect(calls.filter((c) => c.startsWith("update_"))).toEqual(["update_check", "update_download", "update_restart"]);
});

test("nothing restarts until the close guards agree, and the download isn't repeated", async () => {
  const calls = backend({ check: release });
  let answer = false;
  setCloseGuard("t1", async () => answer);
  render(<UpdatesSection />);
  await act(() => useUpdates.getState().check(true));

  await userEvent.click(screen.getByRole("button", { name: /update and restart/i }));
  const again = await screen.findByRole("button", { name: /restart to update/i });
  expect(calls).not.toContain("update_restart");

  answer = true;
  await userEvent.click(again);
  await screen.findByText(/restarting/i);
  expect(calls.filter((c) => c === "update_download")).toHaveLength(1);
  expect(calls).toContain("update_restart");
});

test("a failed download offers another try and never restarts", async () => {
  const calls = backend({ check: release, download: failure("Could not download the update") });
  render(<UpdatesSection />);
  await act(() => useUpdates.getState().check(true));
  await userEvent.click(screen.getByRole("button", { name: /update and restart/i }));
  expect(await screen.findByRole("alert")).toHaveTextContent("Could not download");
  expect(screen.getByRole("button", { name: /update and restart/i })).toBeEnabled();
  expect(calls).not.toContain("update_restart");
});

test("a restart refused because work is running keeps the download", async () => {
  const refused = { kind: "refused", class: "fix_then_retry", message: "pgWhy can't restart to update while a backup is running" };
  const calls = backend({ check: release, restart: refused });
  render(<UpdatesSection />);
  await act(() => useUpdates.getState().check(true));
  await userEvent.click(screen.getByRole("button", { name: /update and restart/i }));
  expect(await screen.findByRole("alert")).toHaveTextContent("a backup is running");
  await userEvent.click(screen.getByRole("button", { name: /restart to update/i }));
  expect(calls.filter((c) => c === "update_download")).toHaveLength(1);
  expect(calls.filter((c) => c === "update_restart")).toHaveLength(2);
});
