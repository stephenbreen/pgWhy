// Drag handle on a pane's inner edge. Dragging, or the arrow keys when focused, resizes the
// pane; double-click puts it back to its default width.

import { useRef, type KeyboardEvent, type PointerEvent } from "react";

export interface SplitterProps {
  label: string;
  /** which edge of its pane the handle sits on: dragging toward the pane shrinks it */
  edge: "left" | "right";
  width: number;
  min: number;
  max: () => number;
  defaultWidth: number;
  onResize: (w: number) => void;
}

const KEY_STEP = 16;

export function Splitter({ label, edge, width, min, max, defaultWidth, onResize }: SplitterProps) {
  const drag = useRef<{ x0: number; w0: number } | null>(null);
  const clamp = (w: number) => Math.max(min, Math.min(Math.max(min, max()), w));

  const onPointerDown = (e: PointerEvent<HTMLDivElement>) => {
    if (e.button !== 0) return;
    e.preventDefault();
    e.currentTarget.setPointerCapture(e.pointerId);
    // start from what's on screen: the shell's CSS may be holding the pane below its stored width
    const shown = e.currentTarget.parentElement?.getBoundingClientRect().width;
    drag.current = { x0: e.clientX, w0: shown || width };
  };
  const onPointerMove = (e: PointerEvent<HTMLDivElement>) => {
    const d = drag.current;
    if (!d) return;
    const dx = e.clientX - d.x0;
    onResize(clamp(edge === "right" ? d.w0 + dx : d.w0 - dx));
  };
  const end = (e: PointerEvent<HTMLDivElement>) => {
    drag.current = null;
    if (e.currentTarget.hasPointerCapture(e.pointerId)) e.currentTarget.releasePointerCapture(e.pointerId);
  };
  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    const grow = edge === "right" ? "ArrowRight" : "ArrowLeft";
    const shrink = edge === "right" ? "ArrowLeft" : "ArrowRight";
    if (e.key === grow || e.key === shrink) {
      e.preventDefault();
      onResize(clamp(width + (e.key === grow ? KEY_STEP : -KEY_STEP)));
    } else if (e.key === "Home" || e.key === "End") {
      e.preventDefault();
      onResize(clamp(e.key === "Home" ? min : max()));
    }
  };

  return (
    <div
      role="separator"
      aria-orientation="vertical"
      aria-label={label}
      aria-valuenow={width}
      aria-valuemin={min}
      aria-valuemax={Math.max(min, max())}
      tabIndex={0}
      className="pane-splitter"
      data-edge={edge}
      title={`${label} — double-click to reset`}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={end}
      onPointerCancel={end}
      onDoubleClick={() => onResize(clamp(defaultWidth))}
      onKeyDown={onKeyDown}
    />
  );
}
