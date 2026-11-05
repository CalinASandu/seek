import { useEffect, useRef, useState, type ReactNode } from "react";
import { Lock, Shield } from "lucide-react";
import { splitTail } from "../../lib/clean";

/** A path cut in the middle: the start gives way, the name stays. The whole path is on hover. */
export function MidPath({ path, className = "" }: { path: string; className?: string }) {
  const { head, tail } = splitTail(path);
  return (
    <span className={`mid ${className}`} title={path}>
      {head && <span className="mid-head">{head}</span>}
      <span className="mid-tail">{tail}</span>
    </span>
  );
}

interface TickProps {
  checked: boolean;
  onChange: () => void;
  label: string;
  disabled?: boolean;
  title?: string;
  indeterminate?: boolean;
  /** Inside a list that owns keyboard focus. */
  loose?: boolean;
}

export function Tick({ checked, onChange, label, disabled, title, indeterminate, loose }: TickProps) {
  const ref = useRef<HTMLInputElement>(null);
  useEffect(() => {
    if (ref.current) ref.current.indeterminate = !!indeterminate && !checked;
  }, [indeterminate, checked]);
  return (
    <input
      ref={ref}
      type="checkbox"
      className="tick"
      checked={checked}
      disabled={disabled}
      title={title}
      aria-label={label}
      tabIndex={loose ? -1 : undefined}
      onChange={onChange}
      onClick={(e) => e.stopPropagation()}
    />
  );
}

export function Shielded({ className = "" }: { className?: string }) {
  return (
    <span className={`shield ${className}`} role="img" aria-label="Asks for admin" title="Asks for admin">
      <Shield size={13} strokeWidth={2} aria-hidden="true" />
    </span>
  );
}

export function LockNote({ text, label = true }: { text: string; label?: boolean }) {
  return (
    <span className="lock-note" title={text}>
      <Lock size={12} strokeWidth={2} aria-hidden="true" />
      {label && <span>{text}</span>}
    </span>
  );
}

/** Opens by growing its height from nothing (`grid-template-rows`), and mounts its content only while it is open or closing. */
export function Collapse({ open, children }: { open: boolean; children: ReactNode }) {
  const [mounted, setMounted] = useState(open);
  const [shown, setShown] = useState(open);
  useEffect(() => {
    if (open) {
      setMounted(true);
      return;
    }
    setShown(false);
    const t = setTimeout(() => setMounted(false), 260);
    return () => clearTimeout(t);
  }, [open]);
  useEffect(() => {
    if (!mounted || !open) return;
    const id = requestAnimationFrame(() => setShown(true));
    return () => cancelAnimationFrame(id);
  }, [mounted, open]);
  return (
    <div className={`collapse${shown ? " is-open" : ""}`} aria-hidden={!open} inert={!open}>
      <div className="collapse-inner">{mounted && children}</div>
    </div>
  );
}

/** A centred message for a state with nothing to list: before the first scan, scanning, nothing found, an error. */
export function Placeholder({ icon, title, children }: { icon: ReactNode; title: string; children?: ReactNode }) {
  return (
    <div className="clean-empty">
      <div className="clean-empty-glyph" aria-hidden="true">
        {icon}
      </div>
      <h2 className="clean-empty-title">{title}</h2>
      {children && <p className="clean-empty-sub">{children}</p>}
    </div>
  );
}

/** Grey bars standing in for rows that are still on their way. */
export function SkeletonRows({ count = 4 }: { count?: number }) {
  return (
    <div className="clean-skeleton" aria-hidden="true">
      {Array.from({ length: count }, (_, i) => (
        <div key={i} className="clean-skeleton-row">
          <span className="skeleton" style={{ width: 18 }} />
          <span className="skeleton" style={{ width: `${44 + ((i * 17) % 30)}%` }} />
          <span className="skeleton" style={{ width: 54, marginLeft: "auto" }} />
        </div>
      ))}
    </div>
  );
}
