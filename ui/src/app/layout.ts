// Per-machine view preferences: zoom and the widths of the explorer and details panes.
// They live in the webview's localStorage — a convenience, so an unreadable store just
// falls back to the defaults.

import { create } from "zustand";

export const ZOOM_STEPS = [0.67, 0.75, 0.8, 0.9, 1, 1.1, 1.25, 1.5, 1.75, 2];
export const SIDEBAR_W = { default: 300, min: 180, max: 900 };
export const DETAILS_W = { default: 340, min: 240, max: 900 };
// the main area never gets squeezed below this by a pane drag
const MAIN_MIN = 320;
const ACTIVITY_W = 44;

const KEY = "pgwhy.layout";

interface Stored {
  zoom: number;
  sidebarWidth: number;
  detailsWidth: number;
}

const DEFAULTS: Stored = { zoom: 1, sidebarWidth: SIDEBAR_W.default, detailsWidth: DETAILS_W.default };

function load(): Stored {
  try {
    const raw = JSON.parse(localStorage.getItem(KEY) ?? "{}") as Partial<Stored>;
    return {
      zoom: ZOOM_STEPS.includes(raw.zoom as number) ? (raw.zoom as number) : DEFAULTS.zoom,
      sidebarWidth: num(raw.sidebarWidth, SIDEBAR_W),
      detailsWidth: num(raw.detailsWidth, DETAILS_W),
    };
  } catch {
    return DEFAULTS;
  }
}

function num(v: unknown, range: { default: number; min: number; max: number }): number {
  return typeof v === "number" && Number.isFinite(v) ? Math.min(range.max, Math.max(range.min, v)) : range.default;
}

function save(s: Stored) {
  try {
    localStorage.setItem(KEY, JSON.stringify(s));
  } catch {
    // private storage or a full quota: the preference just doesn't survive a restart
  }
}

interface LayoutState extends Stored {
  setZoom: (zoom: number) => void;
  /** one step along ZOOM_STEPS; 0 resets to 100% */
  stepZoom: (dir: -1 | 0 | 1) => void;
  setSidebarWidth: (w: number) => void;
  setDetailsWidth: (w: number) => void;
}

export const useLayout = create<LayoutState>((set, get) => {
  const update = (patch: Partial<Stored>) => {
    set(patch);
    const { zoom, sidebarWidth, detailsWidth } = get();
    save({ zoom, sidebarWidth, detailsWidth });
  };
  return {
    ...load(),
    setZoom: (zoom) => update({ zoom }),
    stepZoom: (dir) => {
      if (dir === 0) return update({ zoom: 1 });
      const i = ZOOM_STEPS.indexOf(get().zoom);
      const next = ZOOM_STEPS[Math.min(ZOOM_STEPS.length - 1, Math.max(0, (i < 0 ? ZOOM_STEPS.indexOf(1) : i) + dir))];
      update({ zoom: next });
    },
    setSidebarWidth: (w) => update({ sidebarWidth: Math.round(w) }),
    setDetailsWidth: (w) => update({ detailsWidth: Math.round(w) }),
  };
});

/** The widest a pane may be dragged, leaving the main area and the other pane their room. */
export function maxPaneWidth(range: { max: number }, otherPane: number): number {
  const viewport = typeof window === "undefined" ? Infinity : window.innerWidth;
  return Math.max(0, Math.min(range.max, viewport - ACTIVITY_W - otherPane - MAIN_MIN));
}

/** Apply the zoom to the webview (the whole UI scales, so grid row maths stays in CSS px). */
export async function applyZoom(zoom: number) {
  try {
    const { getCurrentWebview } = await import("@tauri-apps/api/webview");
    await getCurrentWebview().setZoom(zoom);
  } catch {
    // outside Tauri (tests, the browser demo) there is no webview to zoom
  }
}

export function initZoom() {
  void applyZoom(useLayout.getState().zoom);
  useLayout.subscribe((s, prev) => {
    if (s.zoom !== prev.zoom) void applyZoom(s.zoom);
  });
}
