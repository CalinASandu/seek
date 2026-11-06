import { useEffect, useMemo, useRef, useState, type KeyboardEvent } from "react";
import { ChevronRight } from "lucide-react";
import { useApi } from "../../api/context";
import type { CommitResult, Level, PrepareRequest, Prepared, ReviewItem } from "../../api/types";
import { ICONS, iconGroup } from "../../lib/fileicons";
import { fileName, folderOf, lockText, LEVEL_TEXT, normPath } from "../../lib/clean";
import { formatInt, formatSize } from "../../lib/format";
import { errText } from "../../lib/pcs";
import { Dialog } from "../Dialog";
import { LockNote, MidName, MidPath, Shielded, SkeletonRows, Tick } from "./parts";
import { VirtualRows } from "./VirtualRows";

interface Props {
  request: PrepareRequest;
  /** What the user ticked. For duplicates the request also holds the copies that stay, so only these are listed. */
  picked: ReadonlySet<string> | null;
  /** What is inside each folder, to show when it is opened. */
  contents: ReadonlyMap<string, string[]>;
  /** "Always protect": the item leaves this removal and goes on the protected list. */
  /** Rejects when the choice cannot be saved. */
  onProtect: (path: string) => Promise<void>;
  /** Called when the sheet closes: the paths that really went to the Recycle Bin (and were not put back), and whether a removal ran. */
  onClose: (moved: string[], committed: boolean) => void;
}

type Row =
  | { t: "folder"; key: string; folder: string; ids: number[]; size: number }
  | { t: "item"; key: string; item: ReviewItem }
  | { t: "child"; key: string; text: string; dir: boolean }
  | { t: "more"; key: string; n: number };

const H = { folder: 32, item: 52, child: 26, more: 26 } as const;
/** The list is never taller than this; with few rows it is exactly as tall as they are. */
const MAX_LIST = "min(50vh, 460px)";
const PREPARING_H = 180;
const SHOWN_INSIDE = 8;

function buildRows(items: ReviewItem[], open: ReadonlySet<number>, contents: ReadonlyMap<string, string[]>): Row[] {
  const byFolder = new Map<string, ReviewItem[]>();
  for (const it of items) {
    const f = folderOf(it.path);
    const list = byFolder.get(f);
    if (list) list.push(it);
    else byFolder.set(f, [it]);
  }
  const folders = [...byFolder.entries()].map(([folder, list]) => ({
    folder,
    list: list.sort((a, b) => b.size - a.size || a.path.localeCompare(b.path)),
    size: list.reduce((s, i) => s + i.size, 0),
  }));
  folders.sort((a, b) => b.size - a.size || a.folder.localeCompare(b.folder));
  const rows: Row[] = [];
  for (const f of folders) {
    rows.push({ t: "folder", key: `f:${f.folder}`, folder: f.folder, ids: f.list.map((i) => i.id), size: f.size });
    for (const it of f.list) {
      rows.push({ t: "item", key: `i:${it.id}`, item: it });
      if (it.isDir && open.has(it.id)) {
        const inside = contents.get(it.path) ?? [];
        if (inside.length === 0) rows.push({ t: "child", key: `c:${it.id}:none`, text: it.files > 0 ? `${formatInt(it.files)} files, moved together with the folder` : "Nothing inside", dir: false });
        inside.slice(0, SHOWN_INSIDE).forEach((name, k) => rows.push({ t: "child", key: `c:${it.id}:${k}`, text: name, dir: false }));
        if (inside.length > SHOWN_INSIDE) rows.push({ t: "more", key: `m:${it.id}`, n: inside.length - SHOWN_INSIDE });
      }
    }
  }
  return rows;
}

const plural = (n: number) => `${formatInt(n)} ${n === 1 ? "item" : "items"}`;

export function ReviewSheet({ request, picked, contents, onProtect, onClose }: Props) {
  const api = useApi();
  const [phase, setPhase] = useState<"preparing" | "ready" | "failed" | "moving" | "done">("preparing");
  const [problem, setProblem] = useState<string | null>(null);
  const [protectError, setProtectError] = useState<string | null>(null);
  const protecting = useRef(new Set<number>());
  const [prepared, setPrepared] = useState<Prepared | null>(null);
  const [ticked, setTicked] = useState<ReadonlySet<number>>(new Set());
  const [hidden, setHidden] = useState<ReadonlySet<number>>(new Set());
  const [open, setOpen] = useState<ReadonlySet<number>>(new Set());
  const [focus, setFocus] = useState(0);
  const [result, setResult] = useState<CommitResult | null>(null);
  const [putBack, setPutBack] = useState<{ busy: boolean; done: number; failed: string[]; notes: string[] } | null>(null);
  const [binProblem, setBinProblem] = useState<string | null>(null);
  // When Recently removed could not record everything: how many of this batch's moved items it does hold
  // (null while that is being looked up). Put back is offered when any are there.
  const [recorded, setRecorded] = useState<number | null>(null);
  const [restored, setRestored] = useState<ReadonlySet<string>>(new Set());
  const root = useRef<HTMLDivElement>(null);
  const listRef = useRef<HTMLDivElement | null>(null);
  const first = useRef(request);
  // the list height is set once from what was found, so unticking never changes the sheet height
  const [listHeight, setListHeight] = useState<number | null>(null);
  const pickedN = useMemo(() => (picked ? new Set([...picked].map(normPath)) : null), [picked]);
  const isPicked = (path: string) => !pickedN || pickedN.has(normPath(path));

  useEffect(() => {
    let live = true;
    api.cleanPrepare(first.current).then(
      (p) => {
        if (!live) return;
        setPrepared(p);
        const mine = p.items.filter((i) => isPicked(i.path));
        setTicked(new Set(mine.filter((i) => i.lock === null).map((i) => i.id)));
        setListHeight(buildRows(mine, new Set(), contents).reduce((s, r) => s + H[r.t], 0) + 2);
        setPhase("ready");
      },
      (e: unknown) => {
        if (!live) return;
        setProblem(errText(e));
        setPhase("failed");
      },
    );
    return () => {
      live = false;
    };
    // the review is made once, from what was ticked when the sheet opened
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [api]);

  const shown = useMemo(
    () => (prepared ? prepared.items.filter((it) => isPicked(it.path) && !hidden.has(it.id)) : []),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [prepared, pickedN, hidden],
  );
  const missing = useMemo(() => {
    if (!prepared || !pickedN) return 0;
    return pickedN.size - prepared.items.filter((i) => pickedN.has(normPath(i.path))).length;
  }, [prepared, pickedN]);
  const rows = useMemo(() => buildRows(shown, open, contents), [shown, open, contents]);
  const focusable = useMemo(() => rows.flatMap((r, i) => (r.t === "folder" || r.t === "item" ? [i] : [])), [rows]);

  const stats = useMemo(() => {
    let n = 0;
    let bytes = 0;
    let admin = false;
    for (const it of shown) {
      if (it.lock !== null || !ticked.has(it.id)) continue;
      n++;
      bytes += it.size;
      admin ||= it.needsAdmin;
    }
    return { n, bytes, admin };
  }, [shown, ticked]);

  const toggle = (it: ReviewItem) => {
    if (it.lock !== null) return;
    setTicked((cur) => {
      const next = new Set(cur);
      if (!next.delete(it.id)) next.add(it.id);
      return next;
    });
  };
  const toggleFolder = (ids: number[]) => {
    const live = shown.filter((i) => ids.includes(i.id) && i.lock === null);
    const all = live.every((i) => ticked.has(i.id));
    setTicked((cur) => {
      const next = new Set(cur);
      live.forEach((i) => (all ? next.delete(i.id) : next.add(i.id)));
      return next;
    });
  };
  const protect = async (it: ReviewItem) => {
    if (protecting.current.has(it.id)) return;
    protecting.current.add(it.id);
    setProtectError(null);
    try {
      await onProtect(it.path);
    } catch (e) {
      // not saved: the row stays, and the sheet says so rather than looking as if it worked
      setProtectError(`"Always protect" could not be saved. ${errText(e)}`);
      return;
    } finally {
      protecting.current.delete(it.id);
    }
    setHidden((cur) => new Set(cur).add(it.id));
    setTicked((cur) => {
      const next = new Set(cur);
      next.delete(it.id);
      return next;
    });
  };
  const toggleOpen = (it: ReviewItem) => {
    if (!it.isDir) return;
    setOpen((cur) => {
      const next = new Set(cur);
      if (!next.delete(it.id)) next.add(it.id);
      return next;
    });
  };

  const move = async () => {
    if (!prepared || stats.n === 0 || phase !== "ready") return;
    const ids = shown.filter((i) => i.lock === null && ticked.has(i.id)).map((i) => i.id);
    setPhase("moving");
    try {
      const r = await api.cleanCommit(prepared.token, ids);
      setResult(r);
      setPhase("done");
    } catch (e) {
      setProblem(errText(e));
      setPhase("failed");
    }
  };

  const movedPaths = result ? result.items.filter((i) => i.outcome.kind === "moved").map((i) => i.path) : [];
  const close = () => {
    if (phase === "moving") return;
    onClose(movedPaths.filter((p) => !restored.has(p)), result !== null);
  };

  useEffect(() => {
    if (phase !== "done" || !result?.historyError) return;
    let live = true;
    const bins = new Set(result.items.flatMap((i) => (i.outcome.kind === "moved" ? [i.outcome.binPath] : [])));
    api.cleanHistory().then(
      (h) => live && setRecorded(h.filter((e) => e.putBackMs === null && e.binPath !== null && bins.has(e.binPath)).length),
      () => live && setRecorded(0),
    );
    return () => {
      live = false;
    };
  }, [api, phase, result]);
  // Nothing to put back from here only when it is known that none of the moved items are in the history.
  const lookingUp = !!result?.historyError && recorded === null;
  const canPutBack = !result?.historyError || (recorded ?? 0) > 0;

  // Where the result or an error shows, focus goes to its Done or Close button (Esc still works); Put back never gets it.
  useEffect(() => {
    if (phase === "done" || phase === "failed") {
      root.current?.closest(".dialog")?.querySelector<HTMLElement>("[data-autofocus]")?.focus();
    }
  }, [phase]);

  // The row that has focus; when it is gone (protected away), the row that took its place, clamped to the end.
  const at = focusable.includes(focus) ? focus : (focusable.find((i) => i >= focus) ?? focusable[focusable.length - 1] ?? 0);

  const doPutBack = async () => {
    if (!result) return;
    setPutBack({ busy: true, done: 0, failed: [], notes: [] });
    try {
      const bins = new Map(result.items.flatMap((i) => (i.outcome.kind === "moved" ? [[i.outcome.binPath, i.path] as const] : [])));
      const entries = (await api.cleanHistory()).filter((h) => h.putBackMs === null && h.binPath !== null && bins.has(h.binPath));
      const results = await api.cleanPutBack(entries.map((e) => e.id));
      const okPaths = new Set<string>();
      results.forEach((r) => {
        const e = entries.find((x) => x.id === r.id);
        if (r.restored && e) okPaths.add(e.originalPath);
      });
      setRestored(okPaths);
      setPutBack({
        busy: false,
        done: okPaths.size,
        failed: results.filter((r) => !r.restored).map((r) => r.reason ?? "It could not be put back."),
        // Put back, but Recently removed couldn't record it: still shown.
        notes: results.filter((r) => r.restored && r.reason).map((r) => r.reason as string),
      });
    } catch (e) {
      setPutBack({ busy: false, done: 0, failed: [errText(e)], notes: [] });
    }
  };

  // ---- keyboard: the list owns it; Enter never removes anything ------------------------------------
  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    // a key that reaches here from a control inside a row belongs to that control
    if (e.target !== e.currentTarget || focusable.length === 0 || e.altKey || e.ctrlKey || e.metaKey) return;
    const pos = Math.max(0, focusable.indexOf(at));
    const row = rows[at];
    const go = (k: number) => setFocus(focusable[Math.max(0, Math.min(focusable.length - 1, k))]!);
    if (e.key === "ArrowDown") go(pos + 1);
    else if (e.key === "ArrowUp") go(pos - 1);
    else if (e.key === "PageDown") go(pos + 8);
    else if (e.key === "PageUp") go(pos - 8);
    else if (e.key === "Home") go(0);
    else if (e.key === "End") go(focusable.length - 1);
    else if (e.key === " " && row) (row.t === "item" ? toggle(row.item) : row.t === "folder" && toggleFolder(row.ids));
    else if ((e.key === "p" || e.key === "P") && row?.t === "item") {
      if (!e.repeat) protect(row.item); // a held key protects one row, not a run of them
    }
    else if (e.key === "Enter" && row?.t === "item") toggleOpen(row.item);
    else if (e.key === "ArrowRight" && row?.t === "item" && row.item.isDir && !open.has(row.item.id)) toggleOpen(row.item);
    else if (e.key === "ArrowLeft" && row?.t === "item" && open.has(row.item.id)) toggleOpen(row.item);
    else return;
    e.preventDefault();
  };

  const title =
    phase === "done" && result
      ? movedPaths.length > 0
        ? `Moved ${plural(movedPaths.length)}, ${formatSize(result.movedBytes)}`
        : "Nothing was moved"
      : phase === "preparing"
        ? "Checking what you picked…"
        : phase === "failed"
          ? "This couldn't go ahead"
          : stats.n === 0
            ? "Nothing to move"
            : `Move ${plural(stats.n)} to the Recycle Bin?`;

  const duplicates = request.kind === "duplicates";
  const left = result ? result.items.filter((i) => i.outcome.kind === "left") : [];
  const buttonText = `Move ${plural(stats.n)}${stats.admin ? "… (asks for admin)" : ""}`;

  const footer =
    phase === "done" ? (
      <>
        {movedPaths.length > 0 &&
          !lookingUp &&
          (!canPutBack ? (
            // Nothing was recorded to put back from here: the Recycle Bin is the way.
            <button className="btn" onClick={() => void api.openRecycleBin().catch(() => setBinProblem("Couldn't open the Recycle Bin."))}>
              Open Recycle Bin
            </button>
          ) : (
            <button className="btn" onClick={() => void doPutBack()} disabled={!!putBack && (putBack.busy || putBack.done > 0)}>
              Put back
            </button>
          ))}
        <button className="btn btn-primary" data-autofocus onClick={close}>
          Done
        </button>
      </>
    ) : (
      <>
        <button className="btn" data-autofocus={phase === "failed" ? "" : undefined} onClick={close} disabled={phase === "moving"}>
          {phase === "failed" ? "Close" : "Cancel"}
        </button>
        {phase !== "failed" && (
          <button className="btn btn-primary" disabled={phase !== "ready" || stats.n === 0} onClick={() => void move()}>
            {phase === "moving" ? "Moving…" : buttonText}
          </button>
        )}
      </>
    );

  return (
    <Dialog title={title} onClose={close} width={720} footer={footer}>
      <div ref={root} className="sheet-root">
      {phase === "done" && result ? (
        <div className="review-result">
          <p className="review-note">
            {movedPaths.length > 0
              ? `${plural(movedPaths.length)} ${movedPaths.length === 1 ? "is" : "are"} in the Recycle Bin. ${
                  !canPutBack ? "" : `You can put ${movedPaths.length === 1 ? "it" : "them"} back from there, or here.`
                }`.trim()
              : "Everything was left where it was."}
          </p>
          {result.stoppedEarly && <p className="review-warn">Removal stopped early. {result.stoppedEarly}</p>}
          {result.historyError && (
            <p className="review-warn" role="status">
              {result.historyError}
            </p>
          )}
          {result.historyNote && <p className="review-note">{result.historyNote}</p>}
          {binProblem && <p className="review-warn">{binProblem}</p>}
          {putBack && !putBack.busy && (
            <p className="review-note" role="status">
              {putBack.done > 0 ? `Put back ${plural(putBack.done)}.` : "Nothing was put back."}
              {putBack.failed.length > 0 && ` ${putBack.failed[0]}`}
              {putBack.notes.length > 0 && <span className="removed-note"> {putBack.notes[0]}</span>}
            </p>
          )}
          {left.length > 0 && (
            <>
              <h3 className="review-sub">Left in place · {formatInt(left.length)}</h3>
              <VirtualRows
                className="review-list is-left"
                role="list"
                rowRole="listitem"
                aria-label="Left in place"
                rows={left}
                keyOf={(r) => String(r.id)}
                size={() => 58}
                render={(r) => (
                  <div className="rv-left">
                    <MidPath path={r.path} className="rv-path-strong" />
                    <span className="rv-reason">{r.outcome.kind === "left" ? r.outcome.reason : ""}</span>
                  </div>
                )}
              />
            </>
          )}
        </div>
      ) : phase === "failed" ? (
        <p className="review-warn">{problem}</p>
      ) : (
        <>
          <div className="review-meta">
            <span className="review-total tnum">{formatSize(stats.bytes)}</span>
            <span className="review-note">
              They go to the Recycle Bin, so you can put them back.
              {duplicates && " One copy of each file stays."}
              {missing > 0 && ` ${plural(missing)} ${missing === 1 ? "is" : "are"} no longer there and left out.`}
            </span>
          </div>
          {protectError && (
            <p className="review-warn" role="alert">
              {protectError}
            </p>
          )}
          <VirtualRows
            className="review-list"
            role="grid"
            aria-label="Items to move"
            aria-rowcount={rows.length}
            autoFocus
            listRef={listRef}
            style={{ height: listHeight ?? PREPARING_H, maxHeight: MAX_LIST }}
            rows={rows}
            keyOf={(r) => r.key}
            size={(r) => H[r.t]}
            focusIndex={at}
            onKeyDown={onKeyDown}
            rowClass={(_, i) => (i === at ? "is-focus" : "")}
            render={(r, i) => {
              if (r.t === "folder") {
                const ids = shown.filter((i) => r.ids.includes(i.id) && i.lock === null);
                const on = ids.filter((i) => ticked.has(i.id)).length;
                return (
                  <div className="rv-folder">
                    <Tick
                      loose
                      checked={ids.length > 0 && on === ids.length}
                      indeterminate={on > 0}
                      disabled={ids.length === 0}
                      label={`Move everything in ${r.folder}`}
                      onChange={() => toggleFolder(r.ids)}
                    />
                    <MidPath path={r.folder} />
                    <span className="rv-count tnum">{formatInt(r.ids.length)}</span>
                  </div>
                );
              }
              if (r.t === "child") {
                return (
                  <div className="rv-child">
                    <span>{r.text}</span>
                  </div>
                );
              }
              if (r.t === "more") return <div className="rv-child is-more">and {formatInt(r.n)} more</div>;
              const it = r.item;
              const Icon = ICONS[iconGroup(fileName(it.path), it.isDir)];
              const locked = it.lock !== null;
              return (
                <div
                  className={`rv-item${locked ? " is-locked" : ""}`}
                  onMouseDown={(e) => e.preventDefault() /* clicking never moves DOM focus off the list */}
                  onClickCapture={() => {
                    setFocus(i);
                    listRef.current?.focus();
                  }}
                  onClick={() => toggle(it)}
                >
                  {locked ? (
                    <span className="rv-lock">
                      <LockNote text={lockText(it.lock!)} label={false} />
                    </span>
                  ) : (
                    <Tick loose checked={ticked.has(it.id)} label={`Move ${it.path}`} onChange={() => toggle(it)} />
                  )}
                  {it.isDir ? (
                    <button
                      className="rv-open"
                      tabIndex={-1}
                      aria-expanded={open.has(it.id)}
                      aria-label={`Show what is inside ${fileName(it.path)}`}
                      onClick={(e) => {
                        e.stopPropagation();
                        toggleOpen(it);
                      }}
                    >
                      <ChevronRight size={14} aria-hidden="true" />
                    </button>
                  ) : (
                    <Icon size={16} strokeWidth={1.75} className={`ficon ficon-${iconGroup(fileName(it.path), false)}`} aria-hidden="true" />
                  )}
                  <div className="rv-main">
                    <div className="rv-name">
                      {it.isDir ? <span className="rv-name-text">{fileName(it.path)}</span> : <MidName name={fileName(it.path)} className="rv-name-text" />}
                      {it.level && <LevelTag level={it.level} />}
                      {it.needsAdmin && <Shielded />}
                    </div>
                    {locked ? <span className="rv-locked-why">{lockText(it.lock!)}</span> : <MidPath path={it.path} className="rv-path" />}
                  </div>
                  {!locked && (
                    <button
                      className="rv-protect"
                      tabIndex={-1}
                      aria-label={`Always protect ${it.path}`}
                      title="Always protect (P)"
                      onClick={(e) => {
                        e.stopPropagation();
                        protect(it);
                      }}
                    >
                      Always protect
                    </button>
                  )}
                  <span className="rv-size tnum">{formatSize(it.size)}</span>
                </div>
              );
            }}
          />
          {phase === "preparing" && <SkeletonRows count={4} />}
        </>
      )}
      </div>
    </Dialog>
  );
}

function LevelTag({ level }: { level: Level }) {
  return <span className={`rv-level is-${level}`}>{LEVEL_TEXT[level]}</span>;
}
