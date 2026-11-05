import { memo, useMemo, useState } from "react";
import { AppWindow, Search, X } from "lucide-react";
import type { AppRow } from "../../api/types";
import type { CleanController } from "../../hooks/useCleanup";
import { formatSize } from "../../lib/format";
import { useStore } from "../../lib/store";
import { Segmented, type SegmentedItem } from "../Segmented";
import { Placeholder, SkeletonRows } from "./parts";

type Sort = "name" | "size" | "installed";
const SORTS: SegmentedItem<Sort>[] = [
  { id: "name", label: "Name" },
  { id: "size", label: "Size" },
  { id: "installed", label: "Installed" },
];

const HUES = ["--chart-0", "--chart-2", "--chart-3", "--chart-5", "--chart-6", "--chart-1", "--chart-4", "--chart-9"];
const hueOf = (name: string) => HUES[[...name].reduce((h, c) => (h * 31 + c.charCodeAt(0)) >>> 0, 7) % HUES.length]!;

const dateOf = (ms: number) => new Date(ms).toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" });

interface Props {
  ctl: CleanController;
  onUninstall: (app: AppRow) => void;
}

export function AppsTab({ ctl, onUninstall }: Props) {
  const status = useStore(ctl.apps, (s) => s.status);
  const rows = useStore(ctl.apps, (s) => s.rows);
  const error = useStore(ctl.apps, (s) => s.error);
  const [filter, setFilter] = useState("");
  const [sort, setSort] = useState<Sort>("name");
  const [selected, setSelected] = useState<string | null>(null);

  const shown = useMemo(() => {
    const q = filter.trim().toLowerCase();
    const list = q ? rows.filter((a) => `${a.name} ${a.publisher}`.toLowerCase().includes(q)) : [...rows];
    list.sort((a, b) =>
      sort === "size" ? b.size - a.size : sort === "installed" ? (b.installDate ?? 0) - (a.installDate ?? 0) : a.name.localeCompare(b.name),
    );
    return list;
  }, [rows, filter, sort]);

  return (
    <div className="clean-panel-inner">
      {rows.length > 0 && (
        <div className="apps-tools">
          <label className="field apps-filter">
            <Search size={14} aria-hidden="true" />
            <input value={filter} placeholder="Filter apps" aria-label="Filter apps" spellCheck={false} onChange={(e) => setFilter(e.target.value)} />
            {filter && (
              <button className="icon-btn" aria-label="Clear filter" onClick={() => setFilter("")}>
                <X size={13} />
              </button>
            )}
          </label>
          <Segmented items={SORTS} value={sort} onChange={setSort} label="Sort apps" kind="radio" className="is-small" />
        </div>
      )}
      <div className="clean-scroll">
        {status === "error" ? (
          <Placeholder icon={<AppWindow size={26} />} title="Couldn't read the list of apps">
            {error}
          </Placeholder>
        ) : rows.length === 0 && (status === "idle" || status === "loading") ? (
          <div className="clean-wait">
            <SkeletonRows count={6} />
          </div>
        ) : rows.length === 0 ? (
          <Placeholder icon={<AppWindow size={26} />} title="No apps found">
            Windows doesn't list any installed apps that Seek can uninstall.
          </Placeholder>
        ) : shown.length === 0 ? (
          <Placeholder icon={<Search size={26} />} title="No app matches">
            Nothing installed matches “{filter.trim()}”.
          </Placeholder>
        ) : (
          <ul className="cl-list" aria-label="Installed apps">
            {shown.map((a) => (
              <AppItem key={a.id} app={a} selected={selected === a.id} onSelect={setSelected} onUninstall={onUninstall} />
            ))}
          </ul>
        )}
      </div>
    </div>
  );
}

const AppItem = memo(function AppItem({
  app,
  selected,
  onSelect,
  onUninstall,
}: {
  app: AppRow;
  selected: boolean;
  onSelect: (id: string | null) => void;
  onUninstall: (app: AppRow) => void;
}) {
  return (
    <li className={`cl-row app-row${selected ? " is-selected" : ""}`}>
      <div className="cl-row-inner">
        <div className="app-line">
          <button className="cl-head app-pick" aria-pressed={selected} onClick={() => onSelect(selected ? null : app.id)}>
            <span className="app-tile" style={{ background: `color-mix(in srgb, var(${hueOf(app.name)}) 18%, transparent)`, color: `var(${hueOf(app.name)})` }} aria-hidden="true">
              {app.name.replace(/^[^A-Za-z0-9]+/, "").charAt(0).toUpperCase()}
            </span>
            <span className="cl-main">
              <span className="cl-name">
                <span className="cl-name-text">{app.name}</span>
                {app.running && <span className="pill">Running</span>}
              </span>
              <span className="cl-sub">
                {[app.publisher, app.version].filter(Boolean).join(" · ")}
                {app.note && <span className="app-note"> · {app.note}</span>}
              </span>
            </span>
          </button>
          <span className="app-slot">
            {selected ? (
              <button className="btn app-uninstall" onClick={() => onUninstall(app)}>
                Uninstall…
              </button>
            ) : (
              <span className="app-date tnum">{app.installDate ? dateOf(app.installDate) : ""}</span>
            )}
          </span>
          <span className="app-size tnum">{formatSize(app.size)}</span>
        </div>
      </div>
    </li>
  );
});
