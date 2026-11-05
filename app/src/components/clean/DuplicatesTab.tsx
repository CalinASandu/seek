import { memo, useEffect, useMemo, useState, type CSSProperties } from "react";
import { ChevronRight, CopyCheck, ScanSearch } from "lucide-react";
import type { DupCopy, DupGroup, PrepareRequest } from "../../api/types";
import { canTick, type CleanController } from "../../hooks/useCleanup";
import { folderOf, groupFreeable, lockOf, lockText, visibleGroups } from "../../lib/clean";
import { ICONS, iconGroup } from "../../lib/fileicons";
import { formatInt, formatModified, formatSize, pluralize } from "../../lib/format";
import { useStore } from "../../lib/store";
import { Collapse, LockNote, MidName, MidPath, Placeholder, SelBar, SkeletonRows, Tick } from "./parts";
import type { SheetSpec } from "./CleanView";

const FIRST_COPIES = 40;
const NO_CONTENTS: ReadonlyMap<string, string[]> = new Map();

interface Props {
  ctl: CleanController;
  deviceId: string | null;
  deviceName: string | null;
  onReview: (spec: SheetSpec) => void;
}

const tickedIn = (group: DupGroup, ticked: ReadonlySet<string>) => {
  let n = 0;
  for (const c of group.copies) if (ticked.has(c.path)) n++;
  return n;
};

export function DuplicatesTab({ ctl, deviceId, deviceName, onReview }: Props) {
  const status = useStore(ctl.dup, (s) => s.status);
  const groups = useStore(ctl.dup, (s) => s.groups);
  const error = useStore(ctl.dup, (s) => s.error);
  const prot = useStore(ctl.protect, (s) => s);
  const shown = useMemo(() => visibleGroups(groups, deviceId, prot), [groups, deviceId, prot]);

  return (
    <div className="clean-panel-inner">
      <div className="clean-scroll" ref={(el) => void (ctl.listEl.current = el)}>
        {status === "idle" ? (
          <Placeholder icon={<ScanSearch size={26} />} title="Find files you have twice">
            Seek compares what's inside your files, not only their names. Scanning reads your drives, so it takes a while, and you
            can stop it at any time. Nothing is selected for you.
          </Placeholder>
        ) : status === "error" ? (
          <Placeholder icon={<ScanSearch size={26} />} title="The scan couldn't start">
            {error}
          </Placeholder>
        ) : shown.length === 0 && status === "scanning" ? (
          <div className="clean-wait">
            <SkeletonRows count={5} />
          </div>
        ) : shown.length === 0 ? (
          <Placeholder icon={<CopyCheck size={26} />} title="No duplicates found">
            {deviceName
              ? `Nothing over 1 MB on ${deviceName} is an exact copy of another file.`
              : "Nothing over 1 MB on your drives is an exact copy of another file."}
          </Placeholder>
        ) : (
          <>
            <div className="cl-toolbar">
              <span className="cl-toolbar-note">{pluralize(shown.length, "group")}, largest first. Sizes are what you can free.</span>
              <button className="link-btn" onClick={() => ctl.applySuggestions(shown)}>
                Use suggestions
              </button>
            </div>
            <ul className="cl-list" aria-label="Duplicate groups">
              {shown.map((g) => (
                <GroupRow key={g.id} group={g} ctl={ctl} />
              ))}
            </ul>
            <OtherPcs ctl={ctl} />
          </>
        )}
      </div>
      <DupBar ctl={ctl} groups={groups} onReview={onReview} />
    </div>
  );
}

const GroupRow = memo(function GroupRow({ group, ctl }: { group: DupGroup; ctl: CleanController }) {
  const prot = useStore(ctl.protect, (s) => s);
  const selected = useStore(ctl.dupTicks, (s) => tickedIn(group, s));
  const leave = useStore(ctl.leaving, (m) => m.get(`g:${group.id}`));
  const [open, setOpen] = useState(false);
  // a group that arrived while the end of the list was on screen rises once; read when the row first exists
  const [isNew, setIsNew] = useState(() => {
    const at = ctl.fresh.get(group.id);
    return at !== undefined && Date.now() - at < 800;
  });
  useEffect(() => {
    if (!isNew) return;
    const t = setTimeout(() => setIsNew(false), 700); // reduced motion may never fire animationend
    return () => clearTimeout(t);
  }, [isNew]);
  const free = groupFreeable(group, prot);
  const Icon = ICONS[iconGroup(group.name, false)];
  const n = group.copies.length;

  return (
    <li
      className={`cl-row${leave !== undefined ? " is-leaving" : ""}${isNew ? " is-new" : ""}`}
      style={leave !== undefined ? ({ "--i": leave } as CSSProperties) : undefined}
      data-key={`g:${group.id}`}
      onAnimationEnd={(e) => e.target === e.currentTarget && setIsNew(false)}
    >
      <div className="cl-row-inner">
        <button className="cl-head" aria-expanded={open} aria-label={`${group.name}, ${n} copies`} onClick={() => setOpen((o) => !o)}>
          <ChevronRight size={14} className="chev" aria-hidden="true" />
          <Icon size={18} strokeWidth={1.75} className={`ficon ficon-${iconGroup(group.name, false)}`} aria-hidden="true" />
          <span className="cl-main">
            <span className="cl-name">
                <MidName name={group.name} className="cl-name-text" />
              </span>
            <span className="cl-sub tnum">
              {n} copies · {formatSize(group.size)} each
            </span>
          </span>
          {selected > 0 && <span className="cl-picked tnum">{selected} selected</span>}
          <span className="cl-free tnum">
            {formatSize(free)}
          </span>
        </button>
        <Collapse open={open}>
          <CopyList group={group} ctl={ctl} />
        </Collapse>
      </div>
    </li>
  );
});

function CopyList({ group, ctl }: { group: DupGroup; ctl: CleanController }) {
  const ticked = useStore(ctl.dupTicks, (s) => s);
  const prot = useStore(ctl.protect, (s) => s);
  const [all, setAll] = useState(false);
  const copies = all ? group.copies : group.copies.slice(0, FIRST_COPIES);
  return (
    <div className="copies">
      <div className="copies-head">
        <span>{pluralize(group.copies.length, "copy", "copies")}. One always stays.</span>
        <button className="link-btn" onClick={() => ctl.suggestion(group)}>
          Use suggestion
        </button>
      </div>
      <ul className="copies-list" aria-label={`Copies of ${group.name}`}>
        {copies.map((c) => (
          <CopyRow
            key={c.path}
            copy={c}
            ticked={ticked.has(c.path)}
            allowed={canTick(group, c.path, ticked, prot)}
            lock={lockOf(c, prot)}
            ctl={ctl}
            group={group}
          />
        ))}
      </ul>
      {!all && group.copies.length > FIRST_COPIES && (
        <button className="link-btn copies-more" onClick={() => setAll(true)}>
          Show all {formatInt(group.copies.length)} copies
        </button>
      )}
    </div>
  );
}

const CopyRow = memo(function CopyRow({
  copy,
  ticked,
  allowed,
  lock,
  ctl,
  group,
}: {
  copy: DupCopy;
  ticked: boolean;
  allowed: boolean;
  lock: ReturnType<typeof lockOf>;
  ctl: CleanController;
  group: DupGroup;
}) {
  const leave = useStore(ctl.leaving, (m) => m.get(copy.path));
  return (
    <li
      className={`copy${lock ? " is-locked" : ""}${leave !== undefined ? " is-leaving" : ""}`}
      style={leave !== undefined ? ({ "--i": leave } as CSSProperties) : undefined}
      data-key={copy.path}
    >
      <div className="copy-inner">
        <div className="copy-line">
          {lock ? (
            <span className="copy-lock-icon">
              <LockNote text={lockText(lock)} label={false} />
            </span>
          ) : (
            <Tick
              checked={ticked}
              disabled={!allowed && !ticked}
              title={!allowed && !ticked ? "One copy always stays" : undefined}
              label={`Select ${copy.path}`}
              onChange={() => ctl.toggleCopy(group, copy.path)}
            />
          )}
          <MidPath path={copy.path} className="copy-path" />
          {copy.suggestedKeep && <span className="keep-tag">Suggested keep</span>}
          <span className="copy-date tnum">{formatModified(copy.modified)}</span>
        </div>
        {lock && <div className="copy-why">{lockText(lock)}</div>}
      </div>
    </li>
  );
});

function OtherPcs({ ctl }: { ctl: CleanController }) {
  const other = useStore(ctl.dup, (s) => s.otherPcs);
  if (other.length === 0) return null;
  return (
    <section className="other-pcs" aria-label="On your other PCs">
      <h3>On your other PCs</h3>
      <p className="other-sub">Same name and size, contents not compared</p>
      <ul>
        {other.map((m) => (
          <li key={`${m.pcName}:${m.path}`} title={m.path}>
            <span className="other-name">{m.name}</span>
            <span className="other-pc">{m.pcName}</span>
            <MidPath path={folderOf(m.path)} className="other-path" />
            <span className="other-size tnum">{formatSize(m.size)}</span>
          </li>
        ))}
      </ul>
    </section>
  );
}

function DupBar({ ctl, groups, onReview }: { ctl: CleanController; groups: DupGroup[]; onReview: (s: SheetSpec) => void }) {
  const ticked = useStore(ctl.dupTicks, (s) => s);
  const sizes = useMemo(() => {
    const m = new Map<string, number>();
    groups.forEach((g) => g.copies.forEach((c) => m.set(c.path, c.size)));
    return m;
  }, [groups]);
  const [count, bytes] = useMemo(() => {
    let b = 0;
    ticked.forEach((p) => (b += sizes.get(p) ?? 0));
    return [ticked.size, b];
  }, [ticked, sizes]);
  if (count === 0) return null;

  const review = () => {
    const picked = ctl.dupTicks.get();
    const request: PrepareRequest = {
      kind: "duplicates",
      groups: ctl.dup
        .get()
        .groups.filter((g) => g.copies.some((c) => picked.has(c.path)))
        .map((g) => g.copies.map((c) => ({ path: c.path, deviceId: c.deviceId }))),
    };
    onReview({ request, picked: new Set(picked), contents: NO_CONTENTS, clear: () => ctl.clearDup() });
  };

  return (
    <SelBar>
      <span className="sel-text tnum">
        {formatInt(count)} {count === 1 ? "item" : "items"}, {formatSize(bytes)}
      </span>
      <button className="link-btn" onClick={() => ctl.clearDup()}>
        Clear
      </button>
      <button className="btn btn-primary" onClick={review}>
        Review…
      </button>
    </SelBar>
  );
}
