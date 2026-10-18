import { useEffect, useRef, useState, type MouseEvent } from "react";
import { ArrowUp } from "lucide-react";
import type { SpaceNode } from "../../api/types";
import { formatInt, formatSize } from "../../lib/format";
import { arcPath, lerpArcs, within, type Arc, type AnimatedArc } from "../../lib/sunburst";

/** Outer radius in viewBox units (the SVG scales to its box). */
export const R = 200;
const DURATION = 560;

/** Critically damped spring, normalised to end at 1 (no overshoot, like `--spring`). */
function spring(t: number): number {
  const w = 9;
  const end = 1 - (1 + w) * Math.exp(-w);
  return (1 - (1 + w * t) * Math.exp(-w * t)) / end;
}

const reducedMotion = () =>
  typeof window !== "undefined" && !!window.matchMedia?.("(prefers-reduced-motion: reduce)").matches;

/** Fill of an arc: its first-ring hue, lighter further out; files and merged items are neutral. In the SVG,
 *  hidden items are hatched (`hatched = false` for HTML, which cannot use the SVG pattern). */
export function fillOf(a: Arc, hatched = true): string {
  if (a.kind === "hidden") return hatched ? "url(#space-hatch)" : "var(--chart-rest)";
  if (a.kind === "smaller") return "var(--chart-rest)";
  if (a.hue < 0) return "var(--chart-file)";
  const pct = Math.round(100 - a.tone * 58);
  return `color-mix(in oklab, var(--chart-${a.hue}) ${pct}%, var(--chart-tone))`;
}

/** `key` when it names one of `arcs` (or lies inside or around one), else null. */
export function liveHover(key: string | null, arcs: Arc[]): string | null {
  return key && arcs.some((a) => within(a.key, key) || within(key, a.key)) ? key : null;
}

export function itemLabel(n: SpaceNode): string {
  if (typeof n.kind === "object") return `Smaller items`;
  if (n.kind === "hidden") return "Other users";
  return n.name;
}

interface Props {
  arcs: Arc[];
  /** Arcs are laid out again from scratch (another drive) instead of moving. */
  scene: string;
  node: SpaceNode;
  title: string;
  /** Name of the folder one level up; null at the drive root. */
  parentName: string | null;
  hoverKey: string | null;
  onHover: (key: string | null) => void;
  onOpen: (arc: Arc) => void;
  onUp: () => void;
  onContextMenu: (arc: Arc, x: number, y: number) => void;
}

export function Sunburst(p: Props) {
  const [frame, setFrame] = useState<AnimatedArc[]>([]);
  const shown = useRef<AnimatedArc[]>([]);
  const scene = useRef(p.scene);

  useEffect(() => {
    const from = scene.current === p.scene ? shown.current : [];
    scene.current = p.scene;
    const show = (f: AnimatedArc[]) => {
      shown.current = f;
      setFrame(f);
    };
    if (from.length === 0 || reducedMotion() || typeof requestAnimationFrame !== "function") {
      show(lerpArcs(p.arcs, p.arcs, 1));
      return;
    }
    show(lerpArcs(from, p.arcs, 0));
    const start = performance.now();
    let raf = 0;
    const step = (now: number) => {
      const t = Math.min(1, (now - start) / DURATION);
      show(lerpArcs(from, p.arcs, t >= 1 ? 1 : spring(t)));
      if (t < 1) raf = requestAnimationFrame(step);
    };
    raf = requestAnimationFrame(step);
    return () => cancelAnimationFrame(raf);
  }, [p.arcs, p.scene]);

  // A hover left over from another folder (the pointer has not moved since) must not dim this one.
  const hoverKey = liveHover(p.hoverKey, p.arcs);
  const hovered = hoverKey ? p.arcs.find((a) => a.key === hoverKey) : undefined;
  const focus = hovered?.node ?? p.node;
  const share = hovered && p.node.bytes > 0 ? hovered.node.bytes / p.node.bytes : null;
  const summary = `${p.title}: ${formatSize(p.node.bytes)} in ${formatInt(p.node.files)} files`;

  const ctx = (e: MouseEvent, a: Arc) => {
    if (a.kind !== "folder" && a.kind !== "file") return;
    e.preventDefault();
    p.onContextMenu(a, e.clientX, e.clientY);
  };

  const center = (
    <>
      <span className="sb-name">{hovered ? itemLabel(focus) : p.title}</span>
      <span className="sb-size tnum">{formatSize(focus.bytes)}</span>
      <span className="sb-sub tnum">
        {share !== null
          ? `${share < 0.001 ? "<0.1" : (share * 100).toFixed(share < 0.1 ? 1 : 0)}% of ${p.title}`
          : `${formatInt(focus.files)} ${focus.files === 1 ? "file" : "files"}`}
      </span>
    </>
  );

  return (
    <div className="sunburst" onPointerLeave={() => p.onHover(null)}>
      <svg viewBox={`0 0 ${2 * R} ${2 * R}`} role="img" aria-label={summary}>
        <defs>
          <pattern id="space-hatch" width="6" height="6" patternUnits="userSpaceOnUse" patternTransform="rotate(45)">
            <rect width="6" height="6" fill="var(--chart-rest)" />
            <rect width="2.2" height="6" fill="var(--chart-file)" />
          </pattern>
        </defs>
        {frame.map((a) => {
          const active = !!hoverKey && (within(a.key, hoverKey) || within(hoverKey, a.key));
          return (
            <path
              key={a.key}
              data-key={a.key}
              data-kind={a.kind}
              data-active={hoverKey ? String(active) : undefined}
              data-dim={hoverKey ? String(!active) : undefined}
              className="sb-arc"
              d={arcPath(a, R, R)}
              fillRule="evenodd"
              style={{ fill: fillOf(a), fillOpacity: a.opacity }}
              onPointerEnter={() => p.onHover(a.key)}
              onClick={() => a.kind === "folder" && p.onOpen(a)}
              onContextMenu={(e) => ctx(e, a)}
            />
          );
        })}
      </svg>
      {p.parentName !== null ? (
        <button
          className="sb-center is-up"
          aria-label={`Go up to ${p.parentName}`}
          title={`Go up to ${p.parentName}`}
          onClick={p.onUp}
          onPointerEnter={() => p.onHover(null)}
        >
          <ArrowUp className="sb-up" size={14} strokeWidth={2.2} aria-hidden="true" />
          {center}
        </button>
      ) : (
        <div className="sb-center" onPointerEnter={() => p.onHover(null)}>
          {center}
        </div>
      )}
    </div>
  );
}
