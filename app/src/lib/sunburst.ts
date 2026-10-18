// Sunburst geometry for the Space view: rings of arcs around a center disk, angle 0 at the top, clockwise.
import type { SpaceNode } from "../api/types";

const TAU = Math.PI * 2;
/** Center disk radius as a share of the outer radius. */
export const HOLE = 0.34;
/** Number of first-ring colours (see `--chart-*` in tokens.css). */
export const HUES = 10;
/** Arcs thinner than this (radians) are merged into one "smaller items" arc. */
export const MIN_ANGLE = (0.6 * Math.PI) / 180;

export type ArcKind = "folder" | "file" | "smaller" | "hidden";

export interface Arc {
  /** Path from the drive root (`Users\Calin\Documents`); merged and hidden arcs end in `\~smaller` / `\~hidden`. */
  key: string;
  node: SpaceNode;
  kind: ArcKind;
  /** 1 = innermost ring. */
  depth: number;
  a0: number;
  a1: number;
  r0: number;
  r1: number;
  /** First-ring colour index, inherited outwards; -1 = neutral (files, merged, hidden). */
  hue: number;
  /** 0 innermost … approaching 1 outermost: how much the colour is lightened. */
  tone: number;
}

export function kindOf(n: SpaceNode): ArcKind {
  return typeof n.kind === "string" ? n.kind : "smaller";
}

/** Key of `child` shown inside the folder keyed `parent` (same keys as `layoutSunburst`). */
export function childKey(parent: string, child: SpaceNode): string {
  const kind = kindOf(child);
  if (kind === "smaller") return `${parent}\\~smaller`;
  if (kind === "hidden") return `${parent}\\~hidden`;
  return parent ? `${parent}\\${child.name}` : child.name;
}

/** `key` is `of` or lies inside it. */
export const within = (key: string, of: string): boolean => key === of || key.startsWith(`${of}\\`);

function smallerCount(n: SpaceNode): number {
  return typeof n.kind === "object" ? n.kind.smaller.count : 1;
}

/** Arcs for `root`'s contents, `rings` deep, fitting a circle of `radius`. `rootKey` is root's own path. */
export function layoutSunburst(root: SpaceNode, rootKey: string, rings: number, radius: number, minAngle = MIN_ANGLE): Arc[] {
  const out: Arc[] = [];
  if (root.bytes <= 0 || rings < 1) return out;
  const hole = radius * HOLE;
  const ring = (radius - hole) / rings;

  const place = (node: SpaceNode, key: string, depth: number, a0: number, a1: number, hue: number) => {
    if (depth > rings || node.bytes <= 0) return;
    const scale = (a1 - a0) / node.bytes;
    const shown: SpaceNode[] = [];
    let mergedBytes = 0;
    let mergedFiles = 0;
    let mergedCount = 0;
    for (const c of node.children) {
      if (c.bytes * scale < minAngle || kindOf(c) === "smaller") {
        mergedBytes += c.bytes;
        mergedFiles += c.files;
        mergedCount += smallerCount(c);
      } else shown.push(c);
    }
    if (mergedBytes > 0 || mergedCount > 0) {
      shown.push({
        name: "",
        kind: { smaller: { count: mergedCount } },
        bytes: mergedBytes,
        files: mergedFiles,
        modified: 0,
        cloud: false,
        children: [],
      });
    }
    let at = a0;
    let folderIndex = 0;
    let sibling = 0;
    for (const c of shown) {
      const kind = kindOf(c);
      const end = at + c.bytes * scale;
      const own = kind === "folder" ? (depth === 1 ? folderIndex++ % HUES : hue) : -1;
      const ckey = childKey(key, c);
      out.push({
        key: ckey,
        node: c,
        kind,
        depth,
        a0: at,
        a1: end,
        r0: hole + (depth - 1) * ring,
        r1: hole + depth * ring,
        hue: own,
        // neighbours of one hue alternate slightly, so a folder that fills its parent still shows its parts
        tone: (depth - 1) / rings + (depth > 1 && kind === "folder" && sibling++ % 2 === 1 ? 0.1 : 0),
      });
      if (kind === "folder") place(c, ckey, depth + 1, at, end, own);
      at = end;
    }
  };

  place(root, rootKey, 1, 0, TAU, -1);
  return out;
}

function polar(cx: number, cy: number, r: number, a: number): string {
  return `${(cx + r * Math.sin(a)).toFixed(2)} ${(cy - r * Math.cos(a)).toFixed(2)}`;
}

/** SVG path of an arc; a full ring is drawn as two half arcs per edge (use `fill-rule="evenodd"`). */
export function arcPath(a: Pick<Arc, "a0" | "a1" | "r0" | "r1">, cx: number, cy: number): string {
  const span = a.a1 - a.a0;
  const { r0, r1 } = a;
  if (span >= TAU - 1e-6) {
    const half = a.a0 + Math.PI;
    return (
      `M ${polar(cx, cy, r1, a.a0)} A ${r1} ${r1} 0 1 1 ${polar(cx, cy, r1, half)} ` +
      `A ${r1} ${r1} 0 1 1 ${polar(cx, cy, r1, a.a0)} Z ` +
      `M ${polar(cx, cy, r0, a.a0)} A ${r0} ${r0} 0 1 0 ${polar(cx, cy, r0, half)} ` +
      `A ${r0} ${r0} 0 1 0 ${polar(cx, cy, r0, a.a0)} Z`
    );
  }
  const large = span > Math.PI ? 1 : 0;
  return (
    `M ${polar(cx, cy, r1, a.a0)} A ${r1} ${r1} 0 ${large} 1 ${polar(cx, cy, r1, a.a1)} ` +
    `L ${polar(cx, cy, r0, a.a1)} A ${r0} ${r0} 0 ${large} 0 ${polar(cx, cy, r0, a.a0)} Z`
  );
}

export type AnimatedArc = Arc & { opacity: number };

const mix = (a: number, b: number, t: number) => a * (1 - t) + b * t;

/** `to` at progress `t` (0..1) of the move from the `from` layout: shared arcs travel, new ones grow out of
 *  their nearest old ancestor (or fade in where they are). */
export function lerpArcs(from: Arc[], to: Arc[], t: number): AnimatedArc[] {
  const old = new Map(from.map((a) => [a.key, a]));
  return to.map((a) => {
    let start: { a0: number; a1: number; r0: number; r1: number; opacity: number } | undefined;
    const same = old.get(a.key);
    if (same) start = { ...same, opacity: 1 };
    else {
      let k = a.key;
      while (!start && k.includes("\\")) {
        k = k.slice(0, k.lastIndexOf("\\"));
        const anc = old.get(k);
        if (anc) start = { a0: anc.a0, a1: anc.a1, r0: anc.r1, r1: anc.r1, opacity: 0 };
      }
      if (!start) {
        const mid = (a.a0 + a.a1) / 2;
        start = { a0: mid, a1: mid, r0: a.r0, r1: a.r1, opacity: 0 };
      }
    }
    return {
      ...a,
      a0: mix(start.a0, a.a0, t),
      a1: mix(start.a1, a.a1, t),
      r0: mix(start.r0, a.r0, t),
      r1: mix(start.r1, a.r1, t),
      opacity: mix(start.opacity, 1, t),
    };
  });
}
