import { useLayoutEffect, useRef, type KeyboardEvent } from "react";
import type { LucideIcon } from "lucide-react";

export interface SegmentedItem<T extends string> {
  id: T;
  label: string;
  hint?: string;
  icon?: LucideIcon;
}

interface Props<T extends string> {
  items: SegmentedItem<T>[];
  value: T;
  onChange: (v: T) => void;
  label: string;
  /** `radio` for a choice of view, `tab` for tabs over a panel. */
  kind: "radio" | "tab";
  className?: string;
}

// The capsule's two edges are separate springs: the one leading the way is stiffer, so it stretches toward the
// target and the other catches up. Slightly under-damped: it settles past the target by about a pixel.
const STIFFNESS = 480;
const STRETCH = 0.85;
const DAMPING = 0.8;

/** A segmented control with a capsule that glides between segments; labels change colour exactly at its edge. */
export function Segmented<T extends string>({ items, value, onChange, label, kind, className = "" }: Props<T>) {
  const root = useRef<HTMLDivElement>(null);
  const thumb = useRef<HTMLSpanElement>(null);
  const ink = useRef<HTMLDivElement>(null);
  const halo = useRef<HTMLSpanElement>(null);
  const btns = useRef<(HTMLButtonElement | null)[]>([]);
  const move = useRef<() => void>(() => {});
  const index = items.findIndex((i) => i.id === value);
  const active = useRef(index);
  active.current = index;

  useLayoutEffect(() => {
    let x0 = 0;
    let x1 = 0;
    let v0 = 0;
    let v1 = 0;
    let raf = 0;
    let last = 0;

    const edges = (): [number, number] => {
      const b = btns.current[active.current];
      return b ? [b.offsetLeft, b.offsetLeft + b.offsetWidth] : [0, 0];
    };
    const paint = () => {
      if (!root.current || !thumb.current) return;
      const width = Math.max(0, x1 - x0);
      const [a, b] = edges();
      // a stretched capsule gets a little thinner, as if it kept its volume
      const excess = Math.max(0, width / Math.max(b - a, 1) - 1);
      thumb.current.style.width = `${width}px`;
      thumb.current.style.transform = `translate3d(${x0}px, 0, 0) scaleY(${1 - Math.min(excess * 0.28, 0.16)})`;
      if (ink.current) ink.current.style.clipPath = `inset(0 ${root.current.offsetWidth - x1}px 0 ${x0}px round 999px)`;
    };
    const snap = () => {
      [x0, x1] = edges();
      v0 = v1 = 0;
      paint();
    };
    const tick = (now: number) => {
      raf = 0;
      const dt = last ? Math.min(0.05, (now - last) / 1000) : 1 / 60;
      last = now;
      const [a, b] = edges();
      const dir = Math.sign((a + b) / 2 - (x0 + x1) / 2);
      const lead = STIFFNESS * (1 + STRETCH);
      const trail = STIFFNESS * (1 - STRETCH * 0.55);
      const k0 = dir < 0 ? lead : dir > 0 ? trail : STIFFNESS;
      const k1 = dir > 0 ? lead : dir < 0 ? trail : STIFFNESS;
      const steps = Math.ceil(dt * 240);
      const h = dt / steps;
      for (let i = 0; i < steps; i++) {
        v0 += (-k0 * (x0 - a) - 2 * DAMPING * Math.sqrt(k0) * v0) * h;
        x0 += v0 * h;
        v1 += (-k1 * (x1 - b) - 2 * DAMPING * Math.sqrt(k1) * v1) * h;
        x1 += v1 * h;
      }
      if (Math.abs(x0 - a) + Math.abs(x1 - b) > 0.1 || Math.abs(v0) + Math.abs(v1) > 1) {
        paint();
        raf = requestAnimationFrame(tick);
      } else snap();
    };
    move.current = () => {
      if (window.matchMedia?.("(prefers-reduced-motion: reduce)").matches) snap();
      else if (!raf) {
        last = 0;
        raf = requestAnimationFrame(tick);
      }
    };

    snap();
    const ro = new ResizeObserver(() => {
      if (!raf) snap();
    });
    if (root.current) ro.observe(root.current);
    return () => {
      cancelAnimationFrame(raf);
      ro.disconnect();
    };
  }, []);

  const first = useRef(true);
  useLayoutEffect(() => {
    if (first.current) first.current = false;
    else move.current();
  }, [index]);

  const showHalo = (i: number) => {
    const b = btns.current[i];
    const h = halo.current;
    if (!b || !h) return;
    if (i === index) {
      h.style.opacity = "0";
      return;
    }
    // appears in place; only moves once it is already showing
    h.style.transition = h.style.opacity === "1" ? "" : "opacity 120ms linear";
    h.style.width = `${b.offsetWidth}px`;
    h.style.transform = `translate3d(${b.offsetLeft}px, 0, 0)`;
    h.style.opacity = "1";
  };

  const onKey = (e: KeyboardEvent, i: number) => {
    if (e.altKey || (e.key !== "ArrowLeft" && e.key !== "ArrowRight")) return;
    e.preventDefault();
    e.stopPropagation();
    const next = (i + (e.key === "ArrowRight" ? 1 : items.length - 1)) % items.length;
    onChange(items[next]!.id);
    btns.current[next]?.focus();
  };

  return (
    <div
      ref={root}
      className={`seg ${className}`}
      role={kind === "radio" ? "radiogroup" : "tablist"}
      aria-label={label}
      onPointerLeave={() => {
        if (halo.current) halo.current.style.opacity = "0";
      }}
    >
      <span ref={halo} className="seg-halo" aria-hidden="true" />
      <span ref={thumb} className="seg-thumb" aria-hidden="true">
        <span className="seg-face" />
      </span>
      {items.map((it, i) => (
        <button
          key={it.id}
          ref={(el) => {
            btns.current[i] = el;
          }}
          className={`seg-btn${i === index ? " is-active" : ""}`}
          role={kind}
          aria-checked={kind === "radio" ? i === index : undefined}
          aria-selected={kind === "tab" ? i === index : undefined}
          tabIndex={i === index ? 0 : -1}
          title={it.hint}
          onClick={() => onChange(it.id)}
          onPointerEnter={() => showHalo(i)}
          onKeyDown={(e) => onKey(e, i)}
        >
          {it.icon && <it.icon size={14} aria-hidden="true" />}
          {it.label}
        </button>
      ))}
      <div ref={ink} className="seg-ink" aria-hidden="true">
        {items.map((it) => (
          <span key={it.id} className="seg-btn">
            {it.icon && <it.icon size={14} aria-hidden="true" />}
            {it.label}
          </span>
        ))}
      </div>
    </div>
  );
}
