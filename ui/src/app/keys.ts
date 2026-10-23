// Platform-aware shortcuts: ⌘ on macOS, Ctrl on Windows/Linux — never both, so macOS keeps
// Ctrl+letter for text editing (e.g. Ctrl+P = cursor up in the editor).

export const isMac = typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent);

/** The platform's primary modifier is down (and the other one isn't). */
export function primaryMod(e: { metaKey: boolean; ctrlKey: boolean }): boolean {
  return isMac ? e.metaKey && !e.ctrlKey : e.ctrlKey && !e.metaKey;
}

/** Human label for a shortcut, e.g. shortcut("P", { shift: true }) → "⇧⌘P" / "Ctrl+Shift+P". */
export function shortcut(key: string, opts: { shift?: boolean } = {}): string {
  if (isMac) return `${opts.shift ? "⇧" : ""}⌘${key}`;
  return `Ctrl+${opts.shift ? "Shift+" : ""}${key}`;
}

// Explorer commands reachable from anywhere in the app.
export const FIND_OBJECT_EVENT = "pgwhy:find-object";
export const FILTER_OBJECTS_EVENT = "pgwhy:filter-objects";
