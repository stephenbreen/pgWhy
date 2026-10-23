// The shell must survive a render failure: the boundary shows the error and the rest of the app
// (activity bar, status bar) stays mounted.

import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, test, vi } from "vitest";
import { ErrorBoundary } from "./ErrorBoundary";

function Boom({ explode }: { explode: boolean }) {
  if (explode) throw new Error("renderer exploded");
  return <p>recovered</p>;
}

afterEach(() => vi.restoreAllMocks());

test("renders the fallback and can retry", async () => {
  // React logs the caught error; keep the test output clean.
  vi.spyOn(console, "error").mockImplementation(() => {});
  render(
    <ErrorBoundary>
      <Boom explode />
    </ErrorBoundary>,
  );
  expect(screen.getByRole("alert")).toBeInTheDocument();
  expect(screen.getByText(/stopped rendering/i)).toBeInTheDocument();
  expect(screen.getByText(/renderer exploded/)).toBeInTheDocument();
  await userEvent.click(screen.getByRole("button", { name: /try rendering again/i }));
  // children are re-attempted; this one still throws, so the fallback stays (the point is that
  // the boundary does not unmount the app or lose control)
  expect(screen.getByRole("alert")).toBeInTheDocument();
});

test("shows children when nothing throws", () => {
  render(
    <ErrorBoundary>
      <Boom explode={false} />
    </ErrorBoundary>,
  );
  expect(screen.getByText("recovered")).toBeInTheDocument();
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
});
