import { useEffect, useRef } from "react";

/** Map of combos like "ctrl+f", "ctrl+shift+c", "escape" to handlers. Handler returns false to not preventDefault. */
export type HotkeyMap = Record<string, (e: KeyboardEvent) => void | boolean>;

export function comboOf(e: KeyboardEvent): string {
  const parts: string[] = [];
  if (e.ctrlKey || e.metaKey) parts.push("ctrl");
  if (e.shiftKey) parts.push("shift");
  if (e.altKey) parts.push("alt");
  parts.push(e.key.length === 1 ? e.key.toLowerCase() : e.key.toLowerCase());
  return parts.join("+");
}

export function useHotkeys(map: HotkeyMap): void {
  const ref = useRef(map);
  ref.current = map;
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const fn = ref.current[comboOf(e)];
      if (!fn) return;
      if (fn(e) !== false) e.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);
}
