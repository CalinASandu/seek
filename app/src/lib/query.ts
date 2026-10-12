/** Helpers that edit or inspect the query string without a full parser. */

const esc = (s: string) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

const tokenRe = (token: string) => new RegExp(`(^|\\s)${esc(token)}(?=\\s|$)`, "g");

export function hasToken(query: string, token: string): boolean {
  return tokenRe(token).test(query);
}

export function removeToken(query: string, token: string): string {
  return query.replace(tokenRe(token), " ").replace(/\s+/g, " ").trimStart();
}

export function addToken(query: string, token: string): string {
  const base = query.trimEnd();
  return base ? `${base} ${token} ` : `${token} `;
}

export function toggleToken(query: string, token: string, exclusive: string[] = []): string {
  if (hasToken(query, token)) return removeToken(query, token).trimEnd();
  let q = query;
  for (const other of exclusive) q = removeToken(q, other);
  return addToken(q, token);
}

/** Filter for one drive; with `pc`, only that PC's drive of that name (`drive:"PC/Drive"`). */
export const driveToken = (name: string, pc?: string): string =>
  pc ? `drive:"${pc}/${name}"` : `drive:"${name}"`;

/** Split a query into whitespace-separated tokens; quotes may sit inside a token (`drive:"a b"`). */
export function tokenize(query: string): string[] {
  const toks: string[] = [];
  let cur = "";
  let inQuote = false;
  for (const ch of query) {
    if (ch === '"') {
      inQuote = !inQuote;
      cur += ch;
    } else if (/\s/.test(ch) && !inQuote) {
      if (cur) toks.push(cur);
      cur = "";
    } else cur += ch;
  }
  if (cur) toks.push(cur);
  return toks;
}

/** Plain name terms to highlight in results (quoted phrases supported, filters ignored). */
export function highlightTerms(query: string): string[] {
  const out: string[] = [];
  for (const tok of tokenize(query)) {
    if (/^[a-z]+:/i.test(tok) || tok.startsWith("!") || tok === "|" || /[*?]/.test(tok)) continue;
    const t = tok.replace(/"/g, "");
    if (t) out.push(t);
  }
  return out;
}

export interface Segment {
  text: string;
  hit: boolean;
}

export function segmentName(name: string, terms: string[]): Segment[] {
  if (terms.length === 0) return [{ text: name, hit: false }];
  const lower = name.toLowerCase();
  const marks = new Uint8Array(name.length);
  for (const t of terms) {
    const needle = t.toLowerCase();
    if (!needle) continue;
    let from = 0;
    for (;;) {
      const at = lower.indexOf(needle, from);
      if (at < 0) break;
      marks.fill(1, at, at + needle.length);
      from = at + needle.length;
    }
  }
  const segs: Segment[] = [];
  let start = 0;
  for (let i = 1; i <= name.length; i++) {
    if (i === name.length || marks[i] !== marks[start]) {
      segs.push({ text: name.slice(start, i), hit: marks[start] === 1 });
      start = i;
    }
  }
  return segs;
}
