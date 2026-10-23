import type { AppError } from "../lib/ipc/errors";

/** Inline (never modal) rendering of a classified error with its hint and details. */
export function ErrorNote({ error, compact }: { error: AppError; compact?: boolean }) {
  return (
    <div className="error-note" role="alert" data-class={error.class}>
      <span className="error-message">{error.message}</span>
      {error.hint && <span className="error-hint">{error.hint}</span>}
      {!compact && error.detail && (
        <details>
          <summary>Details</summary>
          <pre>{error.detail}</pre>
        </details>
      )}
    </div>
  );
}
