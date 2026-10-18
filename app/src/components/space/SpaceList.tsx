import { useEffect, useRef, type KeyboardEvent, type MouseEvent } from "react";
import { ChevronRight, Cloud, Ellipsis, Folder, Lock } from "lucide-react";
import type { SpaceNode } from "../../api/types";
import { ICONS, iconGroup } from "../../lib/fileicons";
import { formatInt, formatSize } from "../../lib/format";
import { childKey, kindOf } from "../../lib/sunburst";
import { itemLabel } from "./Sunburst";

interface Props {
  node: SpaceNode;
  nodeKey: string;
  /** Chart colour of each child (by key); -1 or missing = neutral. */
  hues: Map<string, number>;
  hoverKey: string | null;
  selected: string | null;
  onSelect: (key: string | null) => void;
  onHover: (key: string | null) => void;
  onOpen: (child: SpaceNode) => void;
  onContextMenu: (child: SpaceNode, x: number, y: number) => void;
}

const colorOf = (hue: number | undefined) => (hue !== undefined && hue >= 0 ? `var(--chart-${hue})` : "var(--chart-file)");

function Icon({ n, hue }: { n: SpaceNode; hue: number | undefined }) {
  const kind = kindOf(n);
  if (kind === "smaller") return <Ellipsis className="ficon" size={16} aria-hidden="true" />;
  if (kind === "hidden") return <Lock className="ficon" size={15} aria-hidden="true" />;
  if (kind === "folder") {
    const c = colorOf(hue);
    return (
      <Folder
        className="ficon sl-folder"
        size={16}
        strokeWidth={1.75}
        style={{ color: c, fill: `color-mix(in srgb, ${c} 26%, transparent)` }}
        aria-hidden="true"
      />
    );
  }
  const group = iconGroup(n.name, false);
  const Glyph = ICONS[group];
  return <Glyph className={`ficon ficon-${group}`} size={16} strokeWidth={1.75} aria-hidden="true" />;
}

export function SpaceList(p: Props) {
  const ref = useRef<HTMLDivElement>(null);
  const rows = p.node.children;
  const keys = rows.map((c) => childKey(p.nodeKey, c));
  const idx = p.selected ? keys.indexOf(p.selected) : -1;

  useEffect(() => {
    if (idx >= 0) ref.current?.querySelector(`[data-row="${idx}"]`)?.scrollIntoView?.({ block: "nearest" });
  }, [idx]);

  const move = (to: number) => {
    const i = Math.max(0, Math.min(rows.length - 1, to));
    p.onSelect(keys[i] ?? null);
    p.onHover(keys[i] ?? null);
  };

  const onKey = (e: KeyboardEvent) => {
    const cur = rows[idx];
    if (e.key === "ArrowDown") move(idx + 1);
    else if (e.key === "ArrowUp") move(idx < 0 ? rows.length - 1 : idx - 1);
    else if (e.key === "Home") move(0);
    else if (e.key === "End") move(rows.length - 1);
    else if ((e.key === "Enter" || e.key === "ArrowRight") && cur && kindOf(cur) === "folder") p.onOpen(cur);
    else return;
    e.preventDefault();
    e.stopPropagation();
  };

  const ctx = (e: MouseEvent, c: SpaceNode, key: string) => {
    const kind = kindOf(c);
    if (kind !== "folder" && kind !== "file") return;
    e.preventDefault();
    p.onSelect(key);
    p.onContextMenu(c, e.clientX, e.clientY);
  };

  if (rows.length === 0) {
    return <p className="space-note">This folder is empty</p>;
  }

  return (
    <div
      ref={ref}
      className="sl"
      role="listbox"
      aria-label="Contents"
      tabIndex={0}
      aria-activedescendant={idx >= 0 ? `sl-${idx}` : undefined}
      onKeyDown={onKey}
      onPointerLeave={() => p.onHover(null)}
    >
      {rows.map((c, i) => {
        const key = keys[i]!;
        const kind = kindOf(c);
        const share = p.node.bytes > 0 ? c.bytes / p.node.bytes : 0;
        const hue = p.hues.get(key);
        const label = itemLabel(c);
        const count = typeof c.kind === "object" ? c.kind.smaller.count : 0;
        return (
          <div
            key={key}
            id={`sl-${i}`}
            data-row={i}
            role="option"
            aria-selected={i === idx}
            aria-label={`${label}, ${formatSize(c.bytes)}`}
            className={`sl-row is-${kind}${p.hoverKey === key ? " is-hover" : ""}`}
            onClick={() => p.onSelect(key)}
            onDoubleClick={() => kind === "folder" && p.onOpen(c)}
            onContextMenu={(e) => ctx(e, c, key)}
            onPointerEnter={() => p.onHover(key)}
          >
            <span className="sl-icon">
              <Icon n={c} hue={hue} />
            </span>
            <span className="sl-main">
              <span className="sl-name">
                <span className="sl-text">{label}</span>
                {count > 0 && <span className="sl-count tnum">{formatInt(count)}</span>}
                {c.cloud && <Cloud className="sl-cloud" size={12} aria-label="Contains cloud-only files" />}
              </span>
              <span className="sl-bar" aria-hidden="true">
                <span style={{ width: `${Math.max(share * 100, share > 0 ? 0.8 : 0)}%`, background: colorOf(hue) }} />
              </span>
            </span>
            <span className="sl-size tnum">{formatSize(c.bytes)}</span>
            <span className="sl-chev" aria-hidden="true">
              {kind === "folder" && <ChevronRight size={14} />}
            </span>
          </div>
        );
      })}
    </div>
  );
}
