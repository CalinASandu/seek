import type { MouseEvent } from "react";
import type { ResultRow } from "../../api/types";
import { ICONS, iconGroup } from "../../lib/fileicons";
import { formatSize } from "../../lib/format";

interface Props {
  files: ResultRow[] | null;
  /** Full path of the folder being shown, to print paths relative to it. */
  base: string;
  onReveal: ((f: ResultRow) => void) | null;
  onContextMenu: (f: ResultRow, x: number, y: number) => void;
}

export function LargestList({ files, base, onReveal, onContextMenu }: Props) {
  if (!files) return <Skeleton />;
  if (files.length === 0) return <p className="space-note">No files here</p>;
  const prefix = base.replace(/\\+$/, "").toLowerCase();
  const rel = (folder: string) => {
    const f = folder.toLowerCase();
    if (f === prefix) return "In this folder";
    return f.startsWith(prefix + "\\") ? folder.slice(prefix.length + 1) : folder;
  };
  return (
    <ul className="ll" aria-label="Largest files">
      {files.map((f) => {
        const group = iconGroup(f.name, false);
        const Glyph = ICONS[group];
        return (
          <li
            key={f.fullPath}
            className="ll-row"
            title={f.fullPath}
            onDoubleClick={() => onReveal?.(f)}
            onContextMenu={(e: MouseEvent) => {
              e.preventDefault();
              onContextMenu(f, e.clientX, e.clientY);
            }}
          >
            <Glyph className={`ficon ficon-${group}`} size={16} strokeWidth={1.75} aria-hidden="true" />
            <span className="ll-main">
              <span className="ll-name">{f.name}</span>
              <span className="ll-folder">{rel(f.folder)}</span>
            </span>
            <span className="ll-size tnum">{formatSize(f.size)}</span>
          </li>
        );
      })}
    </ul>
  );
}

export function Skeleton() {
  return (
    <div className="space-skeleton" aria-hidden="true">
      {Array.from({ length: 6 }, (_, i) => (
        <span key={i} className="skeleton" style={{ width: `${82 - i * 9}%` }} />
      ))}
    </div>
  );
}
