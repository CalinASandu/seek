import { useCallback, useEffect, useState } from "react";
import { useApi } from "../api/context";
import type { Settings } from "../api/types";

const DEFAULTS: Settings = { startWithWindows: false, theme: "system", excludeFolders: [] };

export function useSettings() {
  const api = useApi();
  const [settings, setLocal] = useState<Settings>(DEFAULTS);

  useEffect(() => {
    api.getSettings().then(setLocal).catch(() => undefined);
  }, [api]);

  useEffect(() => {
    const root = document.documentElement;
    if (settings.theme === "system") root.removeAttribute("data-theme");
    else root.setAttribute("data-theme", settings.theme);
  }, [settings.theme]);

  const update = useCallback(
    (patch: Partial<Settings>) => {
      setLocal((cur) => {
        const next = { ...cur, ...patch };
        void api.setSettings(next);
        return next;
      });
    },
    [api],
  );

  return { settings, update };
}
