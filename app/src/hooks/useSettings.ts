import { useCallback, useEffect, useRef, useState } from "react";
import { useApi } from "../api/context";
import type { Settings } from "../api/types";

const DEFAULTS: Settings = { startWithWindows: false, theme: "system", excludeFolders: [], protectedPaths: [] };

export function useSettings() {
  const api = useApi();
  const [settings, setLocal] = useState<Settings>(DEFAULTS);
  /** Why the saved settings (and so the protected list) can't be used; null when they can. */
  const [problem, setProblem] = useState<string | null>(null);
  // Saving before the first load succeeded would write the defaults over what is really saved.
  const loaded = useRef(false);
  const latest = useRef<Settings>(DEFAULTS);

  useEffect(() => {
    let live = true;
    api.getSettings().then(
      (s) => {
        if (!live) return;
        const next = { ...DEFAULTS, ...s, protectedPaths: s.protectedPaths ?? [] };
        loaded.current = true;
        latest.current = next;
        setLocal(next);
      },
      () => undefined,
    );
    api.settingsProblem().then(
      (p) => live && setProblem(p),
      () => undefined,
    );
    return () => {
      live = false;
    };
  }, [api]);

  useEffect(() => {
    const root = document.documentElement;
    if (settings.theme === "system") root.removeAttribute("data-theme");
    else root.setAttribute("data-theme", settings.theme);
  }, [settings.theme]);

  /** Applies a change at once and saves it. If it can't be saved the change is taken back and this rejects. */
  const update = useCallback(
    async (patch: Partial<Settings>): Promise<void> => {
      if (!loaded.current) throw new Error("Your settings haven't loaded yet, so nothing was saved.");
      const prev = latest.current;
      const next = { ...prev, ...patch };
      latest.current = next;
      setLocal(next);
      try {
        await api.setSettings(next);
      } catch (e) {
        if (latest.current === next) {
          latest.current = prev;
          setLocal(prev);
        }
        throw e;
      }
    },
    [api],
  );

  return { settings, problem, update };
}
