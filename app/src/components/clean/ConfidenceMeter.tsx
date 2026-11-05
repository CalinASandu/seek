import { useRef, useState } from "react";
import { createPortal } from "react-dom";
import type { Level, Reason } from "../../api/types";
import { LEVEL_BARS, LEVEL_TEXT } from "../../lib/clean";

const sign = (n: number) => (n > 0 ? `+${n}` : `−${Math.abs(n)}`);

/** Why Seek thinks so: each piece of evidence in a plain sentence, with its direction. */
export function ReasonList({ reasons, label = "Why Seek thinks so" }: { reasons: Reason[]; label?: string }) {
  if (reasons.length === 0) return <p className="reasons-none">No evidence either way.</p>;
  return (
    <ul className="reasons" aria-label={label}>
      {reasons.map((r) => (
        <li key={r.text} className={r.points > 0 ? "is-for" : "is-against"}>
          <span className="reason-points tnum">{sign(r.points)}</span>
          <span>{r.text}</span>
        </li>
      ))}
    </ul>
  );
}

/** Four bars and the level written out. Hovering it lists the reasons; it never ticks anything. */
export function ConfidenceMeter({ level, reasons, quiet = false }: { level: Level; reasons: Reason[]; quiet?: boolean }) {
  const ref = useRef<HTMLSpanElement>(null);
  const [tip, setTip] = useState<{ x: number; y: number } | null>(null);
  const show = () => {
    if (quiet) return; // an expanded row already lists them
    const r = ref.current?.getBoundingClientRect();
    if (r) setTip({ x: Math.max(8, Math.min(r.left, window.innerWidth - 320)), y: r.bottom + 6 });
  };
  const filled = LEVEL_BARS[level];
  return (
    <span
      ref={ref}
      className={`meter is-${level}`}
      onPointerEnter={show}
      onPointerLeave={() => setTip(null)}
      onFocus={show}
      onBlur={() => setTip(null)}
    >
      <span className="meter-bars" aria-hidden="true">
        {[0, 1, 2, 3].map((i) => (
          <i key={i} className={i < filled ? "is-on" : ""} />
        ))}
      </span>
      <span className="meter-label">{LEVEL_TEXT[level]}</span>
      {tip && !quiet &&
        createPortal(
          <span className="meter-tip" role="tooltip" style={{ left: tip.x, top: tip.y }}>
            <span className="meter-tip-title">{LEVEL_TEXT[level]}</span>
            <ReasonList reasons={reasons} label="Reasons" />
          </span>,
          document.body,
        )}
    </span>
  );
}
