import { useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent, type MouseEvent } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { ArrowDown, ArrowUp, ClipboardCopy, ExternalLink, FolderOpen, Type } from "lucide-react";
import { useApi } from "../api/context";
import type { ResultRow, SortKey } from "../api/types";
import { useRows } from "../hooks/useRows";
import { ICONS, iconGroup } from "../lib/fileicons";
import { formatModified, formatSize, splitPath } from "../lib/format";
import { highlightTerms, segmentName } from "../lib/query";
import { ContextMenu, type MenuItem } from "./ContextMenu";

const ROW_H = 32;
const STORAGE_KEY = "seek.colWidths";

type ColKey = "name" | "path" | "size" | "modified" | "device";
interface Col {
  key: ColKey;
  label: string;
  min: number;
  /** Flex weight while the user hasn't resized the column; fixed-width columns have none. */
  weight?: number;
  fixed?: number;
  align?: "right";
}
const COLS: Col[] = [
  { key: "name", label: "Name", min: 160, weight: 5 },
  { key: "path", label: "Folder", min: 140, weight: 5 },
  { key: "size", label: "Size", min: 70, fixed: 88, align: "right" },
  { key: "modified", label: "Modified", min: 90, fixed: 120 },
  { key: "device", label: "Drive", min: 190, weight: 4 },
];

type Widths = Partial<Record<ColKey, number>>;

function loadWidths(): Widths {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (raw) return JSON.parse(raw) as Widths;
  } catch {
    /* storage unavailable */
  }
  return {};
}

interface Props {
  searchId: number | null;
  total: number;
  query: string;
  sort: SortKey;
  descending: boolean;
  onSort: (key: SortKey) => void;
  notify: (message: string) => void;
  scrollerRef?: React.RefObject<HTMLDivElement | null>;
  /** The service can no longer serve these rows (index renumbered): re-run the search. */
  onStale?: () => void;
  /** Device id → linked PC name, for rows whose files live on another PC (they cannot be opened here). */
  remotePc?: Map<string, string>;
}

const NO_REMOTE = new Map<string, string>();

function Highlighted({ name, terms }: { name: string; terms: string[] }) {
  return (
    <span className="cell-name-text">
      {segmentName(name, terms).map((s, i) =>
        s.hit ? (
          <mark key={i} className="hit">
            {s.text}
          </mark>
        ) : (
          <span key={i}>{s.text}</span>
        ),
      )}
    </span>
  );
}

function FolderCell({ folder }: { folder: string }) {
  const { head, tail } = splitPath(folder);
  return (
    <span className="cell-folder" title={folder}>
      {head && <span className="folder-head">{head}</span>}
      <span className="folder-tail">{tail}</span>
    </span>
  );
}

export function ResultsTable({
  searchId,
  total,
  query,
  sort,
  descending,
  onSort,
  notify,
  scrollerRef,
  onStale,
  remotePc = NO_REMOTE,
}: Props) {
  const api = useApi();
  const innerRef = useRef<HTMLDivElement>(null);
  const scroller = scrollerRef ?? innerRef;
  const { getRow, ensure } = useRows(searchId, onStale);
  const [widths, setWidths] = useState(loadWidths);
  const [selected, setSelected] = useState<Set<number>>(new Set());
  const [focus, setFocus] = useState(0);
  const anchor = useRef(0);
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);
  const terms = useMemo(() => highlightTerms(query), [query]);

  const virt = useVirtualizer({
    count: total,
    getScrollElement: () => scroller.current,
    estimateSize: () => ROW_H,
    overscan: 12,
    scrollPaddingStart: ROW_H,
    initialRect: { width: 900, height: 600 },
  });
  const items = virt.getVirtualItems();
  const firstIdx = items[0]?.index ?? 0;
  const lastIdx = items[items.length - 1]?.index ?? 0;

  useEffect(() => {
    if (total > 0) ensure(firstIdx, lastIdx);
  }, [firstIdx, lastIdx, total, searchId, ensure, getRow]);

  // New query or sort: back to the top. A refresh of the same query keeps the scroll position.
  useEffect(() => {
    setFocus(0);
    anchor.current = 0;
    scroller.current?.scrollTo?.({ top: 0 });
  }, [query, sort, descending, scroller]);

  // Any new result set: selected row numbers may now be other files.
  useEffect(() => setSelected(new Set()), [searchId]);

  useEffect(() => {
    try {
      localStorage.setItem(STORAGE_KEY, JSON.stringify(widths));
    } catch {
      /* ignore */
    }
  }, [widths]);

  const template = COLS.map((c) => {
    const w = widths[c.key] ?? c.fixed;
    return w !== undefined ? `${w}px` : `minmax(${c.min}px, ${c.weight}fr)`;
  }).join(" ");
  const minWidth = COLS.reduce((s, c) => s + (widths[c.key] ?? c.fixed ?? c.min), 0);

  const selectedRows = useCallback((): ResultRow[] => {
    const idx = selected.size ? [...selected].sort((a, b) => a - b) : [focus];
    return idx.map(getRow).filter((r): r is ResultRow => !!r);
  }, [selected, focus, getRow]);

  // ---- actions ------------------------------------------------------------
  const copy = useCallback(
    async (text: string, what: string) => {
      try {
        await navigator.clipboard.writeText(text);
        notify(what);
      } catch {
        notify("Couldn't copy to the clipboard");
      }
    },
    [notify],
  );
  /** On this device and connected: the path can be opened here. */
  const openable = useCallback((r: ResultRow) => r.online && !remotePc.has(r.deviceId), [remotePc]);
  const open = useCallback(() => {
    const rows = selectedRows().filter(openable);
    rows.forEach((r) => void api.openPath(r.fullPath));
  }, [api, selectedRows, openable]);
  const reveal = useCallback(() => {
    selectedRows()
      .filter(openable)
      .forEach((r) => void api.revealPath(r.fullPath));
  }, [api, selectedRows, openable]);
  const copyPaths = useCallback(() => {
    const rows = selectedRows();
    void copy(rows.map((r) => r.fullPath).join("\n"), rows.length > 1 ? `${rows.length} paths copied` : "Path copied");
  }, [selectedRows, copy]);
  const copyNames = useCallback(() => {
    const rows = selectedRows();
    void copy(rows.map((r) => r.name).join("\n"), rows.length > 1 ? `${rows.length} names copied` : "Name copied");
  }, [selectedRows, copy]);

  // ---- selection ------------------------------------------------------------
  const moveTo = (i: number, extend: boolean) => {
    const next = Math.max(0, Math.min(total - 1, i));
    setFocus(next);
    if (extend) {
      const [a, b] = [Math.min(anchor.current, next), Math.max(anchor.current, next)];
      setSelected(new Set(Array.from({ length: b - a + 1 }, (_, k) => a + k)));
    } else {
      anchor.current = next;
      setSelected(new Set([next]));
    }
    virt.scrollToIndex(next, { align: "auto" });
  };

  const onRowClick = (i: number, e: MouseEvent) => {
    setMenu(null);
    if (e.shiftKey) moveTo(i, true);
    else if (e.ctrlKey || e.metaKey) {
      anchor.current = i;
      setFocus(i);
      setSelected((s) => {
        const n = new Set(s);
        if (n.has(i)) n.delete(i);
        else n.add(i);
        return n;
      });
    } else moveTo(i, false);
  };

  const page = () => Math.max(1, Math.floor((scroller.current?.clientHeight || 600) / ROW_H) - 2);

  const onKeyDown = (e: KeyboardEvent) => {
    if (total === 0) return;
    const mod = e.ctrlKey || e.metaKey;
    const k = e.key;
    if (k === "ArrowDown") moveTo(focus + 1, e.shiftKey);
    else if (k === "ArrowUp") moveTo(focus - 1, e.shiftKey);
    else if (k === "PageDown") moveTo(focus + page(), e.shiftKey);
    else if (k === "PageUp") moveTo(focus - page(), e.shiftKey);
    else if (k === "Home") moveTo(0, e.shiftKey);
    else if (k === "End") moveTo(total - 1, e.shiftKey);
    else if (mod && k.toLowerCase() === "a") setSelected(new Set(Array.from({ length: total }, (_, i) => i)));
    else if (k === "Enter" && mod) reveal();
    else if (k === "Enter") open();
    else if (mod && e.shiftKey && k.toLowerCase() === "c") copyNames();
    else if (mod && k.toLowerCase() === "c") copyPaths();
    else if (k === "ContextMenu" || (e.shiftKey && k === "F10")) {
      const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
      setMenu({ x: r.left + 80, y: r.top + 120 });
    } else return;
    e.preventDefault();
  };

  const onRowContext = (i: number, e: MouseEvent) => {
    e.preventDefault();
    if (!selected.has(i)) {
      anchor.current = i;
      setSelected(new Set([i]));
    }
    setFocus(i);
    setMenu({ x: e.clientX, y: e.clientY });
  };

  const menuItems = (): MenuItem[] => {
    const rows = selectedRows();
    const remote = rows.map((r) => remotePc.get(r.deviceId)).find((pc) => pc !== undefined);
    const anyOffline = rows.some((r) => !r.online);
    const why = remote ? `On ${remote} — not on this device` : anyOffline ? "The drive is offline" : undefined;
    const blocked = !!remote || anyOffline || rows.length === 0;
    return [
      { id: "open", label: "Open", icon: <ExternalLink size={14} />, shortcut: "Enter", disabled: blocked, title: why, onSelect: open },
      { id: "reveal", label: "Open containing folder", icon: <FolderOpen size={14} />, shortcut: "Ctrl+Enter", disabled: blocked, title: why, onSelect: reveal },
      { id: "copyPath", label: rows.length > 1 ? "Copy paths" : "Copy path", icon: <ClipboardCopy size={14} />, shortcut: "Ctrl+C", separatorBefore: true, onSelect: copyPaths },
      { id: "copyName", label: rows.length > 1 ? "Copy names" : "Copy name", icon: <Type size={14} />, shortcut: "Ctrl+Shift+C", onSelect: copyNames },
    ];
  };

  // ---- column resizing ------------------------------------------------------
  const startResize = (col: Col, e: React.PointerEvent) => {
    e.preventDefault();
    e.stopPropagation();
    const startX = e.clientX;
    const startW = (e.currentTarget as HTMLElement).parentElement?.getBoundingClientRect().width ?? col.min;
    const move = (ev: PointerEvent) => setWidths((w) => ({ ...w, [col.key]: Math.max(col.min, Math.round(startW + ev.clientX - startX)) }));
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  };

  return (
    <div
      className="grid-scroller"
      ref={scroller}
      role="grid"
      aria-label="Search results"
      aria-rowcount={total + 1}
      aria-multiselectable="true"
      tabIndex={0}
      onKeyDown={onKeyDown}
    >
      <div className="grid-inner" style={{ minWidth: minWidth + 16 /* row inset */ }}>
        <div className="grid-header" role="row" style={{ gridTemplateColumns: template }}>
          {COLS.map((c) => {
            const on = sort === c.key;
            return (
              <div
                key={c.key}
                role="columnheader"
                aria-sort={on ? (descending ? "descending" : "ascending") : "none"}
                className={`th${c.align === "right" ? " is-right" : ""}${on ? " is-sorted" : ""}`}
              >
                <button className="th-btn" aria-label={`Sort by ${c.label}`} onClick={() => onSort(c.key)}>
                  <span>{c.label}</span>
                  {on && (descending ? <ArrowDown size={12} /> : <ArrowUp size={12} />)}
                </button>
                <span className="th-resize" onPointerDown={(e) => startResize(c, e)} role="separator" aria-orientation="vertical" aria-label={`Resize ${c.label} column`} />
              </div>
            );
          })}
        </div>

        <div className="grid-body" style={{ height: virt.getTotalSize() }}>
          {items.map((v) => {
            const row = getRow(v.index);
            const sel = selected.has(v.index);
            return (
              <div
                key={v.key}
                role="row"
                aria-rowindex={v.index + 2}
                aria-selected={sel}
                data-index={v.index}
                className={`tr${v.index % 2 ? " is-odd" : ""}${sel ? " is-selected" : ""}${v.index === focus ? " is-focus" : ""}${row && !row.online ? " is-offline" : ""}`}
                style={{ gridTemplateColumns: template, transform: `translateY(${v.start}px)` }}
                onClick={(e) => onRowClick(v.index, e)}
                onDoubleClick={() => row && openable(row) && void api.openPath(row.fullPath)}
                onContextMenu={(e) => onRowContext(v.index, e)}
              >
                {row ? (
                  <>
                    <div role="gridcell" className="td td-name">
                      {(() => {
                        const Icon = ICONS[iconGroup(row.name, row.isDir)];
                        return <Icon size={16} strokeWidth={1.75} className={`ficon ficon-${iconGroup(row.name, row.isDir)}`} aria-hidden="true" />;
                      })()}
                      <Highlighted name={row.name} terms={terms} />
                    </div>
                    <div role="gridcell" className="td td-muted">
                      <FolderCell folder={row.folder} />
                    </div>
                    <div role="gridcell" className="td td-right td-muted tnum">
                      {row.isDir ? "—" : formatSize(row.size)}
                    </div>
                    <div role="gridcell" className="td td-muted tnum">
                      {formatModified(row.modified)}
                    </div>
                    <div role="gridcell" className="td td-device">
                      <span className={`dot dot-${row.online ? "online" : "offline"}`} aria-hidden="true" />
                      <span className="device-cell-name" title={remotePc.has(row.deviceId) ? `On ${remotePc.get(row.deviceId)}` : undefined}>
                        {remotePc.has(row.deviceId) ? `${remotePc.get(row.deviceId)} › ${row.deviceName}` : row.deviceName}
                      </span>
                      {!row.online && <span className="pill">offline</span>}
                    </div>
                  </>
                ) : (
                  <>
                    {[58, 72, 40, 52, 46].map((w, i) => (
                      <div key={i} role="gridcell" className={`td${i === 2 ? " td-right" : ""}`}>
                        <span className="skeleton" style={{ width: `${w + ((v.index * 7 + i * 13) % 28)}%` }} />
                      </div>
                    ))}
                  </>
                )}
              </div>
            );
          })}
        </div>
      </div>

      {menu && <ContextMenu x={menu.x} y={menu.y} label="Result actions" items={menuItems()} onClose={() => setMenu(null)} />}
    </div>
  );
}
