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

  /** One save at a time, in the order asked; a failed one never blocks the next. */
  const queue = useRef<Promise<unknown>>(Promise.resolve());

  /**
   * Applies a change at once and saves it. If it can't be saved the change is taken back and this rejects.
   * With `saveFirst` nothing changes on screen until the save went through: for the protected list, where a
   * row must never show as protected when the file doesn't say so.
   * Changes run one after another, each worked out from the settings as they are when its turn comes
   * (pass a function to add or remove something relative to them), so two quick changes never overwrite each other.
   */
  const update = useCallback(
    (patch: Partial<Settings> | ((s: Settings) => Partial<Settings>), opts?: { saveFirst?: boolean }): Promise<void> => {
      const run = async (): Promise<void> => {
        if (!loaded.current) throw new Error("Your settings haven't loaded yet, so nothing was saved.");
        const prev = latest.current;
        const next = { ...prev, ...(typeof patch === "function" ? patch(prev) : patch) };
        if (opts?.saveFirst) {
          await api.setSettings(next);
          latest.current = next;
          setLocal(next);
          return;
        }
        latest.current = next;
        setLocal(next);
        try {
          await api.setSettings(next);
        } catch (e) {
          latest.current = prev;
          setLocal(prev);
          throw e;
        }
      };
      const done = queue.current.then(run);
      queue.current = done.catch(() => undefined);
      return done;
    },
    [api],
  );

  return { settings, problem, update };
}
