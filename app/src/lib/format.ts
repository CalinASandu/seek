const UNITS = ["B", "KB", "MB", "GB", "TB", "PB"];

export function formatSize(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 B";
  let v = bytes;
  let u = 0;
  while (v >= 1024 && u < UNITS.length - 1) {
    v /= 1024;
    u++;
  }
  const text = u > 0 && v < 10 ? v.toFixed(1).replace(/\.0$/, "") : String(Math.round(v));
  return `${text} ${UNITS[u]}`;
}

export function formatCount(n: number): string {
  if (n < 1000) return String(n);
  if (n < 1_000_000) return `${trim(n / 1000)}k`;
  return `${trim(n / 1_000_000)}M`;
}

const trim = (v: number) => (v < 10 ? v.toFixed(1).replace(/\.0$/, "") : String(Math.round(v)));

/** Thousands-separated integer, e.g. 12,481. */
export const formatInt = (n: number): string => n.toLocaleString("en-US");

/** Timestamps from the service are unix seconds. */
/** Timestamps from the service are unix epoch milliseconds already. */
export const toMs = (ts: number): number => ts;

export function formatRelative(ts: number, now: number = Date.now()): string {
  const s = Math.max(0, Math.round((now - toMs(ts)) / 1000));
  if (s < 45) return "just now";
  const m = Math.round(s / 60);
  if (m < 60) return `${m} min ago`;
  const h = Math.round(m / 60);
  if (h < 24) return `${h} h ago`;
  const d = Math.round(h / 24);
  if (d < 30) return `${d} ${d === 1 ? "day" : "days"} ago`;
  const mo = Math.round(d / 30);
  if (mo < 12) return `${mo} ${mo === 1 ? "month" : "months"} ago`;
  const y = Math.round(d / 365);
  return `${y} ${y === 1 ? "year" : "years"} ago`;
}

/** Relative for < 7 days, locale date otherwise. */
export function formatModified(ts: number, now: number = Date.now()): string {
  const ms = toMs(ts);
  if (now - ms < 7 * 86_400_000 && now - ms >= 0) return formatRelative(ts, now);
  return new Date(ms).toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" });
}

export const pluralize = (n: number, one: string, many = `${one}s`): string => `${formatInt(n)} ${n === 1 ? one : many}`;

export function splitPath(folder: string): { head: string; tail: string } {
  const parts = folder.split("\\");
  if (parts.length <= 3) return { head: "", tail: folder };
  return { head: parts.slice(0, -2).join("\\") + "\\", tail: parts.slice(-2).join("\\") };
}
