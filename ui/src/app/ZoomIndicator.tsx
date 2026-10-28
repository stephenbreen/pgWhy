// Status-bar zoom readout: only shown away from 100%, and clicking it resets.

import { shortcut } from "./keys";
import { useLayout } from "./layout";

export function ZoomIndicator() {
  const zoom = useLayout((s) => s.zoom);
  const stepZoom = useLayout((s) => s.stepZoom);
  if (zoom === 1) return null;
  return (
    <button type="button" className="link" title={`Reset zoom (${shortcut("0")})`} onClick={() => stepZoom(0)}>
      Zoom {Math.round(zoom * 100)}%
    </button>
  );
}
