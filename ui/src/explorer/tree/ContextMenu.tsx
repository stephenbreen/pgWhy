import { useEffect, useRef } from "react";
import { actionsFor, type NodeTarget } from "../actions";

export function ContextMenu({ target, x, y, onClose }: { target: NodeTarget; x: number; y: number; onClose: () => void }) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const away = (e: MouseEvent) => !ref.current?.contains(e.target as Node) && onClose();
    const esc = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("mousedown", away);
    window.addEventListener("keydown", esc);
    ref.current?.querySelector<HTMLButtonElement>("button:not(:disabled)")?.focus();
    return () => {
      window.removeEventListener("mousedown", away);
      window.removeEventListener("keydown", esc);
    };
  }, [onClose]);
  const items = actionsFor(target);
  if (items.length === 0) return null;
  let lastGroup: string | undefined;
  return (
    <div ref={ref} className="context-menu" role="menu" style={{ left: x, top: y }}>
      {items.map((a) => {
        const reason = a.disabledReason?.(target) ?? null;
        const header = a.group && a.group !== lastGroup ? <div className="menu-group">{a.group}</div> : null;
        lastGroup = a.group;
        return (
          <div key={a.id}>
            {header}
            <button
              type="button"
              role="menuitem"
              disabled={!!reason}
              title={reason ?? undefined}
              className={a.group ? "in-group" : undefined}
              onClick={() => {
                onClose();
                void a.run(target);
              }}
            >
              {a.label}
              {reason && <span className="menu-reason">{reason}</span>}
            </button>
          </div>
        );
      })}
    </div>
  );
}
