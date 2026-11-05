import { useEffect, useMemo } from "react";
import type { AppRow, DupCopy, DupGroup, DupScanState, LeftoverRow, OtherPcMatch, SeekApi } from "../api/types";
import { freeableOf, groupFreeable, isProtectedPath, lockOf, normPath } from "../lib/clean";
import { errText } from "../lib/pcs";
import { createStore, type Store } from "../lib/store";

export type ScanStatus = "idle" | "scanning" | "cancelling" | "done" | "cancelled" | "error";
export type LoadStatus = "idle" | "loading" | "ready" | "error";

export interface DupState {
  status: ScanStatus;
  groups: DupGroup[];
  otherPcs: OtherPcMatch[];
  checkedBytes: number;
  totalBytes: number;
  current: string | null;
  error: string | null;
}
export interface ListState<T> {
  status: LoadStatus;
  rows: T[];
  error: string | null;
}

const NONE: ReadonlySet<string> = new Set();
const NO_PROTECTED: readonly string[] = [];
const NO_LEAVING: ReadonlyMap<string, number> = new Map();
const IDLE_DUP: DupState = { status: "idle", groups: [], otherPcs: [], checkedBytes: 0, totalBytes: 0, current: null, error: null };

/** How long a row takes to leave (CSS: 200 ms) plus the longest stagger, plus a little. */
const LEAVE_MS = 200 + 30 * 8 + 80;
/** Arrivals during a scan are applied together this often, so a busy scan doesn't re-render the list per group. */
const FLUSH_MS = 120;

export interface CleanController {
  dup: Store<DupState>;
  dupTicks: Store<ReadonlySet<string>>;
  leftovers: Store<ListState<LeftoverRow>>;
  leftTicks: Store<ReadonlySet<string>>;
  apps: Store<ListState<AppRow>>;
  /** Rows on their way out: key → place in the stagger. */
  leaving: Store<ReadonlyMap<string, number>>;
  protect: Store<readonly string[]>;
  /** Groups that arrived while the end of the list was on screen: id → when. Read once, when the row mounts. */
  fresh: Map<number, number>;
  /** The scrolling list of the Duplicates tab, to tell whether an arrival is in view. */
  listEl: { current: HTMLElement | null };
  attach(): void;
  detach(): void;
  scanStart(deviceIds: string[]): void;
  scanCancel(): void;
  loadLeftovers(): void;
  loadApps(): void;
  setProtected(list: readonly string[]): void;
  toggleCopy(group: DupGroup, path: string): void;
  applySuggestions(groups: DupGroup[]): void;
  suggestion(group: DupGroup): void;
  clearDup(): void;
  toggleLeftover(path: string): void;
  clearLeftovers(): void;
  /** Rows that were moved leave the lists: visible ones animate out first. */
  moveOut(paths: string[], container: HTMLElement | null): void;
}

const lower = normPath;

/** A group can't have all its local copies ticked unless a locked copy stays. */
export function canTick(group: DupGroup, path: string, ticked: ReadonlySet<string>, prot: readonly string[]): boolean {
  const copy = group.copies.find((c) => c.path === path);
  if (!copy || lockOf(copy, prot) !== null) return false;
  if (ticked.has(path)) return true; // unticking is always fine
  const removable = group.copies.filter((c) => lockOf(c, prot) === null);
  const anyLocked = removable.length < group.copies.length;
  if (anyLocked) return true;
  const already = removable.filter((c) => ticked.has(c.path)).length;
  return already + 1 < removable.length;
}

export function createCleanController(api: SeekApi): CleanController {
  const dup = createStore<DupState>(IDLE_DUP);
  const dupTicks = createStore<ReadonlySet<string>>(NONE);
  const leftovers = createStore<ListState<LeftoverRow>>({ status: "idle", rows: [], error: null });
  const leftTicks = createStore<ReadonlySet<string>>(NONE);
  const apps = createStore<ListState<AppRow>>({ status: "idle", rows: [], error: null });
  const leaving = createStore<ReadonlyMap<string, number>>(NO_LEAVING);
  const protect = createStore<readonly string[]>(NO_PROTECTED);
  const fresh = new Map<number, number>();
  const listEl: { current: HTMLElement | null } = { current: null };

  let attached = false;
  let off: (() => void) | null = null;
  let timer: ReturnType<typeof setTimeout> | null = null;
  let gen = 0;
  let pending: DupGroup[] = [];
  let latestProgress: { checkedBytes: number; totalBytes: number; current: string | null } | null = null;
  let finished = false;

  const atEnd = () => {
    const el = listEl.current;
    return !el || el.scrollHeight <= el.scrollTop + el.clientHeight + 48;
  };

  function flush() {
    if (timer) clearTimeout(timer);
    timer = null;
    const added = pending;
    const progress = latestProgress;
    const done = finished;
    pending = [];
    latestProgress = null;
    finished = false;
    if (added.length === 0 && !progress && !done) return;
    const inView = added.length > 0 && atEnd();
    dup.set((cur) => {
      let groups = cur.groups;
      if (added.length > 0) {
        const seen = new Set(groups.map((g) => g.id));
        const fresher = added.filter((g) => !seen.has(g.id));
        if (fresher.length > 0) {
          if (inView) fresher.forEach((g) => fresh.set(g.id, Date.now()));
          groups = [...groups, ...fresher];
        }
      }
      return {
        ...cur,
        groups,
        checkedBytes: progress?.checkedBytes ?? cur.checkedBytes,
        totalBytes: progress?.totalBytes ?? cur.totalBytes,
        current: done ? null : progress ? progress.current : cur.current,
        status: done ? (cur.status === "scanning" ? "done" : cur.status === "cancelling" ? "cancelled" : cur.status) : cur.status,
      };
    });
    if (done) {
      const mine = gen;
      api.cleanScanState().then(
        (st) => attached && mine === gen && dup.set((cur) => ({ ...cur, otherPcs: st.otherPcs })),
        () => undefined,
      );
    }
  }

  function onEvent(e: Parameters<Parameters<SeekApi["onCleanEvent"]>[0]>[0]) {
    if (e.type === "group") pending.push(e.group);
    else if (e.type === "progress") latestProgress = e;
    else finished = true;
    if (finished) flush();
    else if (!timer) timer = setTimeout(flush, FLUSH_MS);
  }

  /** Catch up with a scan that went on (or ended) while the view was hidden. */
  function adopt(st: DupScanState) {
    dup.set((cur) => {
      if (!st.running && st.found.length === 0) {
        // results dropped after a long absence: offer Scan again. A scan that really found nothing stays as it is.
        if (cur.groups.length > 0) return IDLE_DUP;
        if (cur.status === "scanning") return { ...cur, status: "done", current: null };
        return cur.status === "cancelling" ? { ...cur, status: "cancelled", current: null } : cur;
      }
      const byId = new Map(cur.groups.map((g) => [g.id, g]));
      const groups = st.found.map((g) => {
        const old = byId.get(g.id);
        return old && old.copies.length === g.copies.length && old.freeable === g.freeable ? old : g;
      });
      return {
        status: st.running
          ? cur.status === "cancelling"
            ? "cancelling"
            : "scanning"
          : cur.status === "cancelled" || cur.status === "cancelling"
            ? "cancelled"
            : "done",
        groups,
        otherPcs: st.otherPcs,
        checkedBytes: st.checkedBytes,
        totalBytes: st.totalBytes,
        current: st.current,
        error: null,
      };
    });
    if (dup.get().status === "idle") dupTicks.set(NONE);
    else reconcileTicks();
  }

  /** Ticks must name copies that still exist and are unlocked, and never every removable copy of a group with none locked. */
  function reconcileTicks() {
    const prot = protect.get();
    const groups = dup.get().groups;
    dupTicks.set((cur) => {
      const copies = new Map<string, DupCopy>();
      groups.forEach((g) => g.copies.forEach((c) => copies.set(c.path, c)));
      const next = new Set<string>();
      cur.forEach((p) => {
        const c = copies.get(p);
        if (c && lockOf(c, prot) === null) next.add(p);
      });
      for (const g of groups) {
        const removable = g.copies.filter((c) => lockOf(c, prot) === null);
        const anyLocked = removable.length < g.copies.length;
        if (!anyLocked && removable.length > 0 && removable.every((c) => next.has(c.path))) {
          next.delete(removable[removable.length - 1]!.path);
        }
      }
      return next.size === cur.size && [...next].every((p) => cur.has(p)) ? cur : next;
    });
  }

  function drop(movedLower: Set<string>) {
    const prot = protect.get();
    dup.set((cur) => {
      let changed = false;
      const groups = cur.groups.flatMap((g) => {
        if (!g.copies.some((c) => movedLower.has(lower(c.path)))) return [g];
        changed = true;
        const copies = g.copies.filter((c) => !movedLower.has(lower(c.path)));
        const next = { ...g, copies, freeable: freeableOf(g.size, copies.map((c) => c.lock !== null)) };
        return groupFreeable(next, prot) > 0 ? [next] : [];
      });
      return changed ? { ...cur, groups } : cur;
    });
    leftovers.set((cur) => {
      const rows = cur.rows.filter((r) => !movedLower.has(lower(r.path)));
      return rows.length === cur.rows.length ? cur : { ...cur, rows };
    });
    const prune = (s: ReadonlySet<string>) => {
      let any = false;
      const next = new Set<string>();
      s.forEach((p) => (movedLower.has(lower(p)) ? (any = true) : next.add(p)));
      return any ? next : s;
    };
    dupTicks.set(prune);
    leftTicks.set(prune);
    leaving.set(NO_LEAVING);
  }

  const withTicks = (store: Store<ReadonlySet<string>>, fn: (next: Set<string>) => void) =>
    store.set((cur) => {
      const next = new Set(cur);
      fn(next);
      return next.size === cur.size && [...next].every((p) => cur.has(p)) ? cur : next;
    });

  /** Ticks the others in each group, never the suggested keep. These are the user's clicks; nothing is ticked before. */
  function applySuggestions(groups: DupGroup[]) {
    const prot = protect.get();
    withTicks(dupTicks, (next) => {
      for (const g of groups) {
        g.copies.forEach((c) => next.delete(c.path));
        const keep = g.copies.find((c) => c.suggestedKeep);
        const picks = g.copies.filter((c) => c !== keep && lockOf(c, prot) === null);
        const anyLocked = g.copies.some((c) => lockOf(c, prot) !== null);
        // never every removable copy: with no keep and nothing locked, the last one stays
        const take = !keep && !anyLocked ? picks.slice(0, -1) : picks;
        take.forEach((c: DupCopy) => next.add(c.path));
      }
    });
  }

  return {
    dup,
    dupTicks,
    leftovers,
    leftTicks,
    apps,
    leaving,
    protect,
    fresh,
    listEl,

    attach() {
      if (attached) return;
      attached = true;
      off = api.onCleanEvent(onEvent);
      const mine = gen;
      api.cleanScanState().then(
        (st) => attached && mine === gen && adopt(st),
        () => undefined, // not wired yet: the tab stays at Scan
      );
    },
    detach() {
      attached = false;
      off?.();
      off = null;
      if (timer) clearTimeout(timer);
      timer = null;
      pending = [];
      latestProgress = null;
      finished = false;
    },

    scanStart(deviceIds) {
      // events carry no scan id, so a new scan waits until the old one has said it is done
      const now = dup.get().status;
      if (now === "scanning" || now === "cancelling") return;
      gen++;
      fresh.clear();
      pending = [];
      dupTicks.set(NONE);
      dup.set({ ...IDLE_DUP, status: "scanning" });
      api.cleanScanStart(deviceIds).catch((e: unknown) =>
        dup.set((cur) => ({ ...cur, status: "error", error: errText(e) })),
      );
    },
    scanCancel() {
      flush();
      dup.set((cur) => (cur.status === "scanning" ? { ...cur, status: "cancelling", current: null } : cur));
      // stopping is over when "done" arrives, or when the backend says nothing is running
      api
        .cleanScanCancel()
        .then(() => api.cleanScanState())
        .then(
          (st) => !st.running && dup.set((cur) => (cur.status === "cancelling" ? { ...cur, status: "cancelled" } : cur)),
          () => undefined,
        );
    },

    loadLeftovers() {
      if (leftovers.get().status === "loading") return;
      leftovers.set((cur) => ({ ...cur, status: "loading", error: null }));
      api.cleanLeftovers().then(
        (rows) => {
          leftovers.set({ status: "ready", rows, error: null });
          const have = new Set(rows.map((r) => r.path));
          leftTicks.set((s) => ([...s].every((p) => have.has(p)) ? s : new Set([...s].filter((p) => have.has(p)))));
        },
        (e: unknown) => leftovers.set((cur) => ({ ...cur, status: "error", error: errText(e) })),
      );
    },
    loadApps() {
      if (apps.get().status === "loading") return;
      apps.set((cur) => ({ ...cur, status: "loading", error: null }));
      api.cleanApps().then(
        (rows) => apps.set({ status: "ready", rows, error: null }),
        (e: unknown) => apps.set((cur) => ({ ...cur, status: "error", error: errText(e) })),
      );
    },

    setProtected(list) {
      protect.set(list);
      const prune = (s: ReadonlySet<string>) =>
        [...s].some((p) => isProtectedPath(p, list)) ? new Set([...s].filter((p) => !isProtectedPath(p, list))) : s;
      dupTicks.set(prune);
      leftTicks.set(prune);
    },

    toggleCopy(group, path) {
      const prot = protect.get();
      withTicks(dupTicks, (next) => {
        if (!canTick(group, path, next, prot)) return;
        if (next.has(path)) next.delete(path);
        else next.add(path);
      });
    },
    applySuggestions,
    suggestion(group) {
      applySuggestions([group]);
    },
    clearDup() {
      dupTicks.set(NONE);
    },
    toggleLeftover(path) {
      if (isProtectedPath(path, protect.get())) return;
      withTicks(leftTicks, (next) => (next.has(path) ? next.delete(path) : next.add(path)));
    },
    clearLeftovers() {
      leftTicks.set(NONE);
    },

    moveOut(paths, container) {
      if (paths.length === 0) return;
      const moved = new Set(paths.map(lower));
      const keys = new Set<string>();
      const prot = protect.get();
      for (const g of dup.get().groups) {
        const hit = g.copies.filter((c) => moved.has(lower(c.path)));
        if (hit.length === 0) continue;
        const rest = g.copies.filter((c) => !moved.has(lower(c.path)));
        if (groupFreeable({ ...g, copies: rest }, prot) === 0) keys.add(`g:${g.id}`);
        else hit.forEach((c) => keys.add(c.path));
      }
      for (const r of leftovers.get().rows) if (moved.has(lower(r.path))) keys.add(r.path);

      // only rows on screen animate; they leave one after another, the rest simply go with them
      const order = new Map<string, number>();
      if (container) {
        const box = container.getBoundingClientRect();
        let i = 0;
        container.querySelectorAll<HTMLElement>("[data-key]").forEach((el) => {
          const key = el.dataset.key!;
          if (!keys.has(key) || order.has(key)) return;
          const r = el.getBoundingClientRect();
          if (r.bottom > box.top && r.top < box.bottom) order.set(key, Math.min(i++, 8));
        });
      }
      if (order.size === 0) {
        drop(moved);
        return;
      }
      leaving.set(order);
      setTimeout(() => drop(moved), LEAVE_MS);
    },
  };
}

/** One controller per API; it keeps its state while the view is hidden, and listens only while `active`. */
export function useCleanup(api: SeekApi, active: boolean): CleanController {
  const ctl = useMemo(() => createCleanController(api), [api]);
  useEffect(() => {
    if (!active) return;
    ctl.attach();
    return () => ctl.detach();
  }, [ctl, active]);
  return ctl;
}
