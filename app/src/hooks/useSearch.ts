import { useCallback, useEffect, useRef, useState } from "react";
import { useApi } from "../api/context";
import type { SortKey } from "../api/types";

export interface SearchState {
  searchId: number | null;
  total: number;
  elapsedMs: number;
  error: { message: string; position: number } | null;
  loading: boolean;
  /** True once a successful search has been applied for the current non-empty query. */
  active: boolean;
  /** The index changed since these results were computed; `refresh` brings them up to date. */
  stale: boolean;
}

const IDLE: SearchState = { searchId: null, total: 0, elapsedMs: 0, error: null, loading: false, active: false, stale: false };

/**
 * Runs a search when the query or sort changes — and only then. Index changes (which happen every few
 * seconds on a busy drive) just mark the results `stale`; `refresh()` re-runs the search on request.
 * Newer requests supersede older ones; parse errors keep the last good results.
 */
export function useSearch(query: string, sort: SortKey, descending: boolean): SearchState & { refresh: () => void } {
  const api = useApi();
  const [state, setState] = useState<SearchState>(IDLE);
  const seq = useRef(0);
  const prevEmpty = useRef(true);
  const queryRef = useRef(query);
  queryRef.current = query;

  useEffect(
    () => api.onEvent((e) => e.type === "resultsInvalidated" && setState((s) => (s.active && !s.stale ? { ...s, stale: true } : s))),
    [api],
  );

  const run = useCallback(
    async (q: string, my: number) => {
      setState((s) => ({ ...s, loading: true }));
      try {
        const out = await api.search(q, sort, descending);
        if (my !== seq.current) return; // stale response
        if (out.ok) {
          setState({ searchId: out.searchId, total: out.total, elapsedMs: out.elapsedMs, error: null, loading: false, active: true, stale: false });
        } else {
          setState((s) => ({ ...s, error: { message: out.message, position: out.position }, loading: false }));
        }
      } catch (err) {
        if (my !== seq.current) return;
        setState((s) => ({ ...s, error: { message: String(err), position: 0 }, loading: false }));
      }
    },
    [api, sort, descending],
  );

  useEffect(() => {
    const my = ++seq.current;
    if (query.trim() === "") {
      prevEmpty.current = true;
      setState(IDLE);
      return;
    }
    const wait = prevEmpty.current ? 0 : 30;
    prevEmpty.current = false;
    const t = setTimeout(() => void run(query, my), wait);
    return () => clearTimeout(t);
  }, [query, run]);

  const refresh = useCallback(() => {
    if (queryRef.current.trim() === "") return;
    void run(queryRef.current, ++seq.current);
  }, [run]);

  return { ...state, refresh };
}
