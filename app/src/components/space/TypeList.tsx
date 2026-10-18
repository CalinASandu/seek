import type { TypeCategory, TypeTotal } from "../../api/types";
import { ICONS, type IconGroup } from "../../lib/fileicons";
import { formatInt, formatSize } from "../../lib/format";
import { Skeleton } from "./LargestList";

export const TYPE_LABEL: Record<TypeCategory, string> = {
  video: "Video",
  pictures: "Pictures",
  music: "Music",
  documents: "Documents",
  archives: "Archives & disk images",
  apps: "Apps & code",
  system: "System files",
  other: "Other",
};

/** Same icons (and icon colours) as in search results. Bars stay neutral: in Space, colour means "this
 *  folder" (the chart), so kinds of files must not borrow those colours. */
const TYPE_ICON: Record<TypeCategory, IconGroup> = {
  video: "video",
  pictures: "image",
  music: "audio",
  documents: "doc",
  archives: "archive",
  apps: "app",
  system: "system",
  other: "file",
};

export function TypeList({ types }: { types: TypeTotal[] | null }) {
  if (!types) return <Skeleton />;
  if (types.length === 0) return <p className="space-note">No files here</p>;
  const max = Math.max(...types.map((t) => t.bytes), 1);
  return (
    <ul className="tl" aria-label="By type">
      {types.map((t) => {
        const group = TYPE_ICON[t.category];
        const Glyph = ICONS[group];
        return (
          <li key={t.category} className="tl-row">
            <Glyph className={`ficon ficon-${group}`} size={16} strokeWidth={1.75} aria-hidden="true" />
            <span className="tl-main">
              <span className="tl-top">
                <span className="tl-name">{TYPE_LABEL[t.category]}</span>
                <span className="tl-size tnum">{formatSize(t.bytes)}</span>
              </span>
              <span className="tl-bottom">
                <span className="sl-bar" aria-hidden="true">
                  <span className="tl-fill" style={{ width: `${(t.bytes / max) * 100}%` }} />
                </span>
                <span className="tl-files tnum">
                  {formatInt(t.files)} {t.files === 1 ? "file" : "files"}
                </span>
              </span>
            </span>
          </li>
        );
      })}
    </ul>
  );
}
