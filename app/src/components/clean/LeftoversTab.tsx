import { memo, useMemo, useState, type CSSProperties } from "react";
import { ChevronRight, Folder, FolderCheck } from "lucide-react";
import type { LeftoverRow, PrepareRequest, Why } from "../../api/types";
import type { CleanController } from "../../hooks/useCleanup";
import { isProtectedPath, LEVEL_ORDER, lockText } from "../../lib/clean";
import { formatInt, formatModified, formatSize, pluralize } from "../../lib/format";
import { useStore } from "../../lib/store";
import { ConfidenceMeter, ReasonList } from "./ConfidenceMeter";
import { Collapse, LockNote, MidPath, Placeholder, SelBar, Shielded, SkeletonRows, Tick } from "./parts";
import type { SheetSpec } from "./CleanView";
import { useRowMenu } from "./rowMenu";

interface Props {
  ctl: CleanController;
  onReview: (spec: SheetSpec) => void;
}

/** `appId` names the app whose uninstall left the rows (for `why` "uninstall"). */
export function folderRequest(rows: LeftoverRow[], why: Why, appId?: string): PrepareRequest {
  const folders = rows.map((r) => ({ path: r.path, level: r.level, reasons: r.reasons }));
  return appId === undefined ? { kind: "folders", why, folders } : { kind: "folders", why, folders, appId };
}

export function contentsOf(rows: LeftoverRow[]): Map<string, string[]> {
  return new Map(rows.map((r) => [r.path, r.contents]));
}

export function LeftoversTab({ ctl, onReview }: Props) {
  const status = useStore(ctl.leftovers, (s) => s.status);
  const rows = useStore(ctl.leftovers, (s) => s.rows);
  const error = useStore(ctl.leftovers, (s) => s.error);
  const [inUseOpen, setInUseOpen] = useState(false);

  const sorted = useMemo(
    () => [...rows].sort((a, b) => LEVEL_ORDER.indexOf(a.level) - LEVEL_ORDER.indexOf(b.level) || b.size - a.size),
    [rows],
  );
  const main = useMemo(() => sorted.filter((r) => r.level !== "probablyInUse"), [sorted]);
  const inUse = useMemo(() => sorted.filter((r) => r.level === "probablyInUse"), [sorted]);

  return (
    <div className="clean-panel-inner">
      <div className="clean-scroll">
        {status === "error" ? (
          <Placeholder icon={<Folder size={26} />} title="Couldn't look for leftovers">
            {error}
          </Placeholder>
        ) : rows.length === 0 && (status === "idle" || status === "loading") ? (
          <div className="clean-wait">
            <SkeletonRows count={5} />
          </div>
        ) : rows.length === 0 ? (
          <Placeholder icon={<FolderCheck size={26} />} title="No leftovers found">
            Every folder in Program Files and AppData belongs to an installed app or to Windows.
          </Placeholder>
        ) : (
          <>
            <div className="cl-toolbar">
              <span className="cl-toolbar-note">Most likely left over first. Nothing is selected for you.</span>
            </div>
            <ul className="cl-list" aria-label="Leftover folders">
              {main.map((r) => (
                <LeftRow key={r.path} row={r} ctl={ctl} />
              ))}
              {inUse.length > 0 && (
                <>
                  <li className="cl-section">
                    <button className="cl-section-btn" aria-expanded={inUseOpen} onClick={() => setInUseOpen((o) => !o)}>
                      <ChevronRight size={14} className="chev" aria-hidden="true" />
                      <span>Probably still in use</span>
                      <span className="cl-section-count tnum">{formatInt(inUse.length)}</span>
                    </button>
                  </li>
                  {inUseOpen && inUse.map((r) => <LeftRow key={r.path} row={r} ctl={ctl} />)}
                </>
              )}
            </ul>
          </>
        )}
      </div>
      <LeftBar ctl={ctl} rows={rows} onReview={onReview} />
    </div>
  );
}

const LeftRow = memo(function LeftRow({ row, ctl }: { row: LeftoverRow; ctl: CleanController }) {
  const ticked = useStore(ctl.leftTicks, (s) => s.has(row.path));
  const protectedNow = useStore(ctl.protect, (p) => isProtectedPath(row.path, p));
  const leave = useStore(ctl.leaving, (m) => m.get(row.path));
  return (
    <LeftLine
      row={row}
      ticked={ticked}
      locked={protectedNow}
      leave={leave}
      onToggle={() => ctl.toggleLeftover(row.path)}
    />
  );
});

interface LineProps {
  row: LeftoverRow;
  ticked: boolean;
  locked: boolean;
  leave?: number;
  onToggle: () => void;
}

/** One leftover folder: shared by the Leftovers tab and the "what's left" step of an uninstall. */
export function LeftLine({ row, ticked, locked, leave, onToggle }: LineProps) {
  const [open, setOpen] = useState(false);
  const { rowProps, menu } = useRowMenu(row.path, "Folder actions");
  const name = row.path.slice(row.path.lastIndexOf("\\") + 1);
  return (
    <li
      {...rowProps}
      className={`cl-row lf-row${leave !== undefined ? " is-leaving" : ""}`}
      style={leave !== undefined ? ({ "--i": leave } as CSSProperties) : undefined}
      data-key={row.path}
    >
      <div className="cl-row-inner">
        <div className="lf-line">
          {locked ? (
            <span className="lf-lock" title={lockText("protected")}>
              <LockNote text={lockText("protected")} label={false} />
            </span>
          ) : (
            <Tick checked={ticked} label={`Select ${row.path}`} onChange={onToggle} />
          )}
          <button className="cl-head lf-head" aria-expanded={open} onClick={() => setOpen((o) => !o)}>
            <ChevronRight size={14} className="chev" aria-hidden="true" />
            <Folder size={18} strokeWidth={1.75} className="ficon ficon-folder" aria-hidden="true" />
            <span className="cl-main">
              <span className="cl-name">
                <span className="cl-name-text">{name}</span>
              </span>
              <MidPath path={row.parent} className="cl-sub" />
            </span>
          </button>
          {locked ? <span className="lf-protected">Protected</span> : <ConfidenceMeter level={row.level} reasons={row.reasons} quiet={open} />}
          {row.needsAdmin ? <Shielded /> : <span className="shield-gap" />}
          <span className="lf-size tnum">{formatSize(row.size)}</span>
          <span className="lf-changed tnum">{formatModified(row.modified)}</span>
        </div>
        <Collapse open={open}>
          <div className="lf-detail">
            <div>
              <h4>Why Seek thinks so</h4>
              <ReasonList reasons={row.reasons} />
              <p className="lf-path">
                <MidPath path={row.path} />
              </p>
            </div>
            <div>
              <h4>Inside · {pluralize(row.contents.length, "item")}</h4>
              {row.contents.length === 0 ? (
                <p className="reasons-none">Empty.</p>
              ) : (
                <ul className="lf-contents">
                  {row.contents.slice(0, 10).map((c) => (
                    <li key={c} title={c}>
                      {c}
                    </li>
                  ))}
                  {row.contents.length > 10 && <li className="is-more">and {formatInt(row.contents.length - 10)} more</li>}
                </ul>
              )}
            </div>
          </div>
        </Collapse>
      </div>
      {menu}
    </li>
  );
}

function LeftBar({ ctl, rows, onReview }: { ctl: CleanController; rows: LeftoverRow[]; onReview: (s: SheetSpec) => void }) {
  const ticked = useStore(ctl.leftTicks, (s) => s);
  const picked = useMemo(() => rows.filter((r) => ticked.has(r.path)), [rows, ticked]);
  if (picked.length === 0) return null;
  const bytes = picked.reduce((s, r) => s + r.size, 0);
  return (
    <SelBar>
      <span className="sel-text tnum">
        {formatInt(picked.length)} {picked.length === 1 ? "item" : "items"}, {formatSize(bytes)}
      </span>
      <button className="link-btn" onClick={() => ctl.clearLeftovers()}>
        Clear
      </button>
      <button
        className="btn btn-primary"
        onClick={() => onReview({ request: folderRequest(picked, "leftover"), picked: null, contents: contentsOf(picked), clear: () => ctl.clearLeftovers() })}
      >
        Review…
      </button>
    </SelBar>
  );
}
