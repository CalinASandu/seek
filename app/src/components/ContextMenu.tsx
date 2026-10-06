import { createPortal } from "react-dom";
import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";

export interface MenuItem {
  id: string;
  label: string;
  icon?: ReactNode;
  shortcut?: string;
  disabled?: boolean;
  danger?: boolean;
  title?: string;
  separatorBefore?: boolean;
  onSelect: () => void;
}

interface Props {
  x: number;
  y: number;
  items: MenuItem[];
  label: string;
  onClose: () => void;
}

export function ContextMenu({ x, y, items, label, onClose }: Props) {
  const ref = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState({ left: x, top: y });
  const [active, setActive] = useState(() => Math.max(0, items.findIndex((i) => !i.disabled)));

  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const r = el.getBoundingClientRect();
    setPos({
      left: Math.max(4, Math.min(x, window.innerWidth - r.width - 4)),
      top: Math.max(4, Math.min(y, window.innerHeight - r.height - 4)),
    });
    el.focus();
  }, [x, y]);

  useEffect(() => {
    const away = (e: Event) => {
      if (!ref.current?.contains(e.target as Node)) onClose();
    };
    window.addEventListener("pointerdown", away, true);
    window.addEventListener("blur", onClose);
    window.addEventListener("resize", onClose);
    return () => {
      window.removeEventListener("pointerdown", away, true);
      window.removeEventListener("blur", onClose);
      window.removeEventListener("resize", onClose);
    };
  }, [onClose]);

  const step = (dir: 1 | -1) => {
    let i = active;
    for (let n = 0; n < items.length; n++) {
      i = (i + dir + items.length) % items.length;
      if (!items[i]?.disabled) break;
    }
    setActive(i);
  };

  const choose = (item: MenuItem | undefined) => {
    if (!item || item.disabled) return;
    onClose();
    item.onSelect();
  };

  return createPortal(
    <div
      ref={ref}
      className="menu"
      role="menu"
      aria-label={label}
      tabIndex={-1}
      style={pos}
      onContextMenu={(e) => e.preventDefault()}
      onKeyDown={(e) => {
        if (e.key === "ArrowDown") step(1);
        else if (e.key === "ArrowUp") step(-1);
        else if (e.key === "Home") setActive(items.findIndex((i) => !i.disabled));
        else if (e.key === "Enter" || e.key === " ") choose(items[active]);
        else if (e.key === "Escape" || e.key === "Tab") onClose();
        else return;
        e.preventDefault();
        e.stopPropagation();
      }}
    >
      {items.map((item, i) => (
        <div key={item.id}>
          {item.separatorBefore && <div className="menu-sep" role="separator" />}
          <button
            role="menuitem"
            tabIndex={-1}
            className={`menu-item${i === active ? " is-active" : ""}${item.danger ? " is-danger" : ""}`}
            disabled={item.disabled}
            title={item.title}
            onMouseMove={() => !item.disabled && setActive(i)}
            onClick={() => choose(item)}
          >
            <span className="menu-icon" aria-hidden="true">
              {item.icon}
            </span>
            <span className="menu-label">{item.label}</span>
            {item.shortcut && <kbd className="menu-kbd">{item.shortcut}</kbd>}
          </button>
        </div>
      ))}
    </div>,
    document.body,
  );
}
