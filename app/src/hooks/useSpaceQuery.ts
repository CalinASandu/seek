import { useEffect, useRef, useState } from "react";
import { useApi } from "../api/context";
import { errText } from "../lib/pcs";

export interface SpaceQuery<T> {
  data: T | null;
  /** The key `data` belongs to (data is kept while the next key loads). */
  dataKey: string | null;
  loading: boolean;
  error: string | null;
  /** The key `error` belongs to. */
  errorKey: string | null;
}

/** Loads `fetch()` whenever `key` changes, and again (quietly, after a second) when the index changes. */
export function useSpaceQuery<T>(key: string | null, fetch: () => Promise<T>): SpaceQuery<T> {
  const api = useApi();
  const [state, setState] = useState<SpaceQuery<T>>({
    data: null,
    dataKey: null,
    loading: false,
    error: null,
    errorKey: null,
  });
  const fetchRef = useRef(fetch);
  fetchRef.current = fetch;
  const seq = useRef(0);
  const [tick, setTick] = useState(0);

  useEffect(() => {
    if (key === null) return;
    const mine = ++seq.current;
    setState((s) => ({ ...s, loading: true, error: null, errorKey: null }));
    fetchRef.current().then(
      (data) => mine === seq.current && setState({ data, dataKey: key, loading: false, error: null, errorKey: null }),
      (e: unknown) =>
        mine === seq.current && setState((s) => ({ ...s, loading: false, error: errText(e), errorKey: key })),
    );
  }, [key, tick]);

  useEffect(() => {
    let timer: ReturnType<typeof setTimeout> | undefined;
    const off = api.onEvent((e) => {
      if (e.type !== "resultsInvalidated" && e.type !== "devicesChanged") return;
      clearTimeout(timer);
      timer = setTimeout(() => setTick((t) => t + 1), 1000);
    });
    return () => {
      clearTimeout(timer);
      off();
    };
  }, [api]);

  return state;
}
