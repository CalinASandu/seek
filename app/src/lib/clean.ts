import type { DupCopy, DupGroup, Level, Lock } from "../api/types";

/** The level, always written out; colour and the bars only repeat it. */
export const LEVEL_TEXT: Record<Level, string> = {
  veryLikely: "Very likely left over",
  likely: "Likely left over",
  notSure: "Not sure",
  probablyInUse: "Probably still in use",
};

/** Bars filled out of four. */
export const LEVEL_BARS: Record<Level, number> = { veryLikely: 4, likely: 3, notSure: 2, probablyInUse: 1 };

export const LEVEL_ORDER: Level[] = ["veryLikely", "likely", "notSure", "probablyInUse"];

/**
 * Why a row can't be ticked, in plain words. Mirrors `reasons.rs::lock_reason` (which is phrased for
 * after the fact, "...so it was left in place"); here it is said about a row that is still there.
 */
export function lockText(lock: Lock): string {
  switch (lock) {
    case "systemFolder": return "Part of Windows or your user folders, so it stays.";
    case "programFiles": return "In Program Files, so it stays.";
    case "otherUser": return "Belongs to another user, so it stays.";
    case "appData": return "In an app data folder, so it stays.";
    case "toolFolder": return "Inside a tool folder (.git, node_modules and the like), so it stays.";
    case "noRecycleBin": return "This drive has no Recycle Bin, so Windows might delete it for good. It stays.";
    case "binFull": return "The Recycle Bin on this drive is too full to hold it. Empty it first.";
    case "pathTooLong": return "Some paths in it are too long for the Recycle Bin, so it stays.";
    case "cloudPlaceholder": return "An online-only cloud file, so it stays.";
    case "protected": return "You marked it as always protected, so it stays.";
    case "linkedPc": return "On a linked PC. Seek only removes files on this PC.";
    case "unsupportedPath": return "Seek can't remove this kind of path, so it stays.";
    case "reparsePoint": return "A link to somewhere else, so it stays.";
    case "claimed": return "An app or a running program is using it, so it stays.";
  }
}

/** One spelling for comparing paths from the UI and from the engine: lower case, backslashes, no trailing separator. */
export const normPath = (p: string): string => p.replace(/\//g, "\\").replace(/\\+$/, "").toLowerCase();
const norm = normPath;

/** Is `path` one of the protected entries, or inside one? Case-insensitive, like Windows. */
export function isProtectedPath(path: string, list: readonly string[]): boolean {
  if (list.length === 0) return false;
  const p = norm(path);
  return list.some((entry) => {
    const e = norm(entry);
    return e !== "" && (p === e || p.startsWith(`${e}\\`));
  });
}

export function fileName(path: string): string {
  const i = path.lastIndexOf("\\");
  return i < 0 ? path : path.slice(i + 1);
}

export function folderOf(path: string): string {
  const i = path.lastIndexOf("\\");
  return i <= 0 ? path : path.slice(0, i);
}

/** Splits a path so the end (the name) stays visible when the start is cut: `head` shrinks, `tail` doesn't. */
export function splitTail(path: string, keep = 36): { head: string; tail: string } {
  const i = path.lastIndexOf("\\");
  if (i < 0) return { head: "", tail: path };
  // the separator goes with the name, so a cut path reads like C:\Users\Ca…\name
  const cut = path.length - i - 1 > keep ? path.length - keep : i;
  return { head: path.slice(0, cut), tail: path.slice(cut) };
}

/** Space that can really be freed from one group: removable copies, leaving one copy if none is locked. */
export function freeableOf(size: number, locked: boolean[]): number {
  const removable = locked.filter((l) => !l).length;
  const anyLocked = removable < locked.length;
  return size * Math.max(0, anyLocked ? removable : removable - 1);
}

/** A group's copies as the UI sees them: protected paths count as locked. */
export function lockOf(copy: DupCopy, protectedPaths: readonly string[]): Lock | null {
  return copy.lock ?? (isProtectedPath(copy.path, protectedPaths) ? "protected" : null);
}

export function groupFreeable(group: DupGroup, protectedPaths: readonly string[]): number {
  return freeableOf(group.size, group.copies.map((c) => lockOf(c, protectedPaths) !== null));
}

/** What kind of removal it was, for the history list. */
export const WHY_VERB: Record<string, string> = {
  duplicate: "Duplicate",
  leftover: "Leftover",
  uninstall: "Left after uninstall",
  staleData: "Seek's old data",
};

/** The groups a tab shows: at least one copy can really be removed, and (if a drive is picked) one copy is on it. */
export function visibleGroups(groups: DupGroup[], deviceId: string | null, protectedPaths: readonly string[]): DupGroup[] {
  return groups.filter(
    (g) => groupFreeable(g, protectedPaths) > 0 && (deviceId === null || g.copies.some((c) => c.deviceId === deviceId)),
  );
}
