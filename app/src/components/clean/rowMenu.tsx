import { createContext, useCallback, useContext, useMemo, useRef, useState, type KeyboardEvent, type MouseEvent, type ReactNode } from "react";
import { FolderOpen, ShieldCheck, ShieldOff } from "lucide-react";
import { isProtectedPath, normPath } from "../../lib/clean";
import { ContextMenu, type MenuItem } from "../ContextMenu";

/** What a Cleanup row's menu needs from the app: the saved protected list and the three things it can do. */
export interface RowActions {
  protectedPaths: readonly string[];
  /** Saves first; rejects when the list can't be saved. */
  protect: (path: string) => Promise<void>;
  unprotect: (path: string) => Promise<void>;
  reveal: (path: string) => Promise<void>;
}

const NONE: RowActions = {
  protectedPaths: [],
  protect: async () => undefined,
  unprotect: async () => undefined,
  reveal: async () => undefined,
};

const RowActionsContext = createContext<RowActions>(NONE);

export function RowActionsProvider({ value, children }: { value: RowActions; children: ReactNode }) {
  return <RowActionsContext.Provider value={value}>{children}</RowActionsContext.Provider>;
}

/**
 * The context menu of one duplicate copy or leftover folder: Always protect (or Stop protecting) and Reveal in Explorer.
 * Opens on right click, the context-menu key, and Shift+F10 on anything focused inside the row.
 */
export function useRowMenu(path: string, label: string) {
  const a = useContext(RowActionsContext);
  const [at, setAt] = useState<{ x: number; y: number } | null>(null);
  const opener = useRef<HTMLElement | null>(null);

  const open = useCallback((x: number, y: number) => {
    opener.current = document.activeElement as HTMLElement | null;
    setAt({ x, y });
  }, []);
  const close = useCallback(() => {
    setAt(null);
    // keyboard users land back on the row they came from
    const el = opener.current;
    opener.current = null;
    if (el?.isConnected) el.focus();
  }, []);

  const rowProps = {
    // the open menu is a portal: React still bubbles its events here, so only the row's own DOM counts
    onContextMenu: (e: MouseEvent<HTMLElement>) => {
      if (!e.currentTarget.contains(e.target as Node)) return;
      e.preventDefault();
      e.stopPropagation();
      open(e.clientX, e.clientY);
    },
    onKeyDown: (e: KeyboardEvent<HTMLElement>) => {
      if (!e.currentTarget.contains(e.target as Node)) return;
      if (e.key !== "ContextMenu" && !(e.shiftKey && e.key === "F10")) return;
      e.preventDefault();
      e.stopPropagation();
      const r = e.currentTarget.getBoundingClientRect();
      open(r.left + 24, r.bottom - 8);
    },
  };

  const exact = a.protectedPaths.find((p) => normPath(p) === normPath(path));
  const inherited = exact === undefined ? a.protectedPaths.find((p) => isProtectedPath(path, [p])) : undefined;
  const items = useMemo<MenuItem[]>(
    () => [
      exact !== undefined
        ? { id: "protect", label: "Stop protecting", icon: <ShieldOff size={14} />, onSelect: () => void a.unprotect(exact) }
        : inherited !== undefined
          ? { id: "protect", label: "Stop protecting", icon: <ShieldOff size={14} />, disabled: true, title: `Protected through ${inherited}. Stop protecting that instead.`, onSelect: () => undefined }
          : { id: "protect", label: "Always protect", icon: <ShieldCheck size={14} />, onSelect: () => void a.protect(path) },
      { id: "reveal", label: "Reveal in Explorer", icon: <FolderOpen size={14} />, onSelect: () => void a.reveal(path) },
    ],
    [a, exact, inherited, path],
  );

  const menu = at ? <ContextMenu x={at.x} y={at.y} label={label} items={items} onClose={close} /> : null;
  return { rowProps, menu };
}
