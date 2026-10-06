import { useCallback, useRef, useState } from "react";
import { useApi } from "../api/context";
import type { ResultRow } from "../api/types";

export const PAGE = 200;

/** Page cache of result rows keyed by searchId. Rows load in pages of 200 as `ensure` is called. */
export function useRows(searchId: number | null) {
  const api = useApi();
  const pages = useRef(new Map<number, ResultRow[]>());
  const pending = useRef(new Set<number>());
  const current = useRef<number | null>(searchId);
  const [, bump] = useState(0);

  const ensure = useCallback(
    (first: number, last: number) => {
      const id = current.current;
      if (id === null) return;
      for (let p = Math.floor(first / PAGE); p <= Math.floor(last / PAGE); p++) {
        const key = p;
        if (pages.current.has(key) || pending.current.has(key)) continue;
        pending.current.add(key);
        api
          .fetchRows(id, key * PAGE, PAGE)
          .then((rows) => {
            pending.current.delete(key);
            if (current.current !== id) return;
            pages.current.set(key, rows);
            bump((n) => n + 1);
          })
          .catch(() => pending.current.delete(key));
      }
    },
    [api],
  );

  // New search: drop the cache and fetch afresh.
  const lastId = useRef<number | null>(null);
  if (lastId.current !== searchId) {
    lastId.current = searchId;
    current.current = searchId;
    pages.current = new Map();
    pending.current = new Set();
  }

  const getRow = useCallback((i: number): ResultRow | undefined => pages.current.get(Math.floor(i / PAGE))?.[i % PAGE], []);
  return { getRow, ensure };
}
