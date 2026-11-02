import { useCallback, useState } from "react";

/** A place in the Space view. `device` is null until a drive is chosen (the first online one is shown). */
export interface Spot {
  device: string | null;
  segs: string[];
}

interface Nav {
  stack: Spot[];
  at: number;
  /** The last folder seen on each drive, reopened when you come back to it. */
  where: Record<string, string[]>;
}

const START: Nav = { stack: [{ device: null, segs: [] }], at: 0, where: {} };

const same = (a: Spot, b: Spot) => a.device === b.device && a.segs.join("\\") === b.segs.join("\\");

function land(n: Nav, stack: Spot[], at: number): Nav {
  const spot = stack[at]!;
  return { stack, at, where: spot.device ? { ...n.where, [spot.device]: spot.segs } : n.where };
}

/** Moving somewhere new drops the steps ahead, like a browser. */
export function pushSpot(n: Nav, spot: Spot): Nav {
  if (same(n.stack[n.at]!, spot)) return n;
  return land(n, [...n.stack.slice(0, n.at + 1), spot], n.at + 1);
}

export function replaceSpot(n: Nav, spot: Spot): Nav {
  return land(n, n.stack.map((s, i) => (i === n.at ? spot : s)), n.at);
}

export function stepSpot(n: Nav, delta: -1 | 1): Nav {
  const at = n.at + delta;
  return at < 0 || at >= n.stack.length ? n : land(n, n.stack, at);
}

/** Back/forward history of the Space view: every folder and drive change is a step. */
export function useSpaceNav() {
  const [nav, setNav] = useState(START);
  const go = useCallback((device: string, segs: string[]) => setNav((n) => pushSpot(n, { device, segs })), []);
  const replace = useCallback((device: string, segs: string[]) => setNav((n) => replaceSpot(n, { device, segs })), []);
  const openDrive = useCallback((device: string) => setNav((n) => pushSpot(n, { device, segs: n.where[device] ?? [] })), []);
  const step = useCallback((delta: -1 | 1) => setNav((n) => stepSpot(n, delta)), []);
  return { spot: nav.stack[nav.at]!, go, replace, openDrive, step };
}
