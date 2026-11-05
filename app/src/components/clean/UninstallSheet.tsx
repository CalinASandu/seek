import { useEffect, useMemo, useRef, useState } from "react";
import { TriangleAlert } from "lucide-react";
import { useApi } from "../../api/context";
import type { AppRow, LeftoverRow } from "../../api/types";
import { isProtectedPath } from "../../lib/clean";
import { formatSize, pluralize } from "../../lib/format";
import { errText } from "../../lib/pcs";
import { Dialog } from "../Dialog";
import type { SheetSpec } from "./CleanView";
import { contentsOf, folderRequest, LeftLine } from "./LeftoversTab";
import { MidPath, SkeletonRows } from "./parts";

interface Props {
  app: AppRow;
  protectedPaths: readonly string[];
  /** Opens the review sheet; `after` runs when it closes with what really moved. */
  onReview: (spec: SheetSpec) => void;
  /** The uninstaller ran to the end, so the list of apps has changed. */
  onUninstalled: () => void;
  onClose: () => void;
}

/** How often the sheet asks how the uninstaller is doing, while it is open. */
const POLL_MS = 1000;

/** Three steps in one sheet: before, the app's own uninstaller running, and the leftovers Seek can move afterwards. */
export function UninstallSheet({ app, protectedPaths, onReview, onUninstalled, onClose }: Props) {
  const api = useApi();
  const [step, setStep] = useState<"before" | "running" | "stalled" | "unresolved" | "left">("before");
  const [problem, setProblem] = useState<string | null>(null);
  const [rows, setRows] = useState<LeftoverRow[] | null>(null);
  // the uninstaller has been started, so asking how it is doing makes sense
  const [started, setStarted] = useState(false);
  const [ticked, setTicked] = useState<ReadonlySet<string>>(new Set());
  const live = useRef(true);

  useEffect(() => {
    live.current = true;
    return () => {
      live.current = false;
    };
  }, []);

  const load = async () => {
    setProblem(null);
    setRows(null);
    try {
      const r = await api.uninstallLeftovers(app.id);
      if (live.current) setRows(r);
    } catch (e) {
      if (live.current) {
        setRows([]);
        setProblem(errText(e));
      }
    }
  };

  const showLeft = () => {
    setStep("left");
    onUninstalled();
    void load();
  };

  // An uninstall already in progress for this app (the sheet was closed and opened again): ask first
  // and carry on at the right step. An app Seek never started is an error here, and the sheet starts at the beginning.
  useEffect(() => {
    let stopped = false;
    api
      .uninstallState(app.id)
      .then((s) => {
        if (stopped) return;
        // Only one still in progress is picked up. A finished or unresolved one is over: the app is
        // listed again (or still), so the sheet offers Uninstall.
        if (s === "running") {
          setStarted(true);
          setStep("running");
        }
      })
      .catch(() => {});
    return () => {
      stopped = true;
    };
    // once per opening
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [api, app.id]);

  // Waits for the uninstaller: the process ended and the registry entry is gone. A failed question is
  // asked again; three in a row stop the waiting and say so, which is not the same as "unresolved".
  useEffect(() => {
    if (step !== "running" || !started) return;
    let stopped = false;
    let failures = 0;
    let timer: ReturnType<typeof setTimeout>;
    const poll = async () => {
      try {
        const s = await api.uninstallState(app.id);
        if (stopped) return;
        failures = 0;
        if (s === "finished") showLeft();
        else if (s === "unresolved") setStep("unresolved");
        else timer = setTimeout(poll, POLL_MS);
      } catch (e) {
        if (stopped) return;
        if (++failures < 3) {
          timer = setTimeout(poll, POLL_MS);
          return;
        }
        setProblem(errText(e));
        setStep("stalled");
      }
    };
    void poll();
    return () => {
      stopped = true;
      clearTimeout(timer);
    };
    // showLeft only reads stable values
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [step, started, api, app.id]);

  const start = async () => {
    setProblem(null);
    setStep("running");
    try {
      await api.uninstallStart(app.id);
      if (live.current) setStarted(true);
    } catch (e) {
      // It never started (no permission, no such app, another uninstaller running): say why and stay at the start.
      if (!live.current) return;
      setProblem(errText(e));
      setStep("before");
    }
  };

  // a leftover that was protected meanwhile (from its review) is no longer ticked
  useEffect(() => {
    setTicked((cur) =>
      cur.size === 0 || ![...cur].some((p) => isProtectedPath(p, protectedPaths))
        ? cur
        : new Set([...cur].filter((p) => !isProtectedPath(p, protectedPaths))),
    );
  }, [protectedPaths]);

  const pickedRows = useMemo(() => (rows ?? []).filter((r) => ticked.has(r.path)), [rows, ticked]);
  const toggle = (path: string) =>
    setTicked((cur) => {
      const next = new Set(cur);
      if (!next.delete(path)) next.add(path);
      return next;
    });

  const review = () =>
    onReview({
      request: folderRequest(pickedRows, "uninstall", app.id),
      picked: null,
      contents: contentsOf(pickedRows),
      after: (moved) => {
        const gone = new Set(moved.map((p) => p.toLowerCase()));
        setRows((cur) => (cur ?? []).filter((r) => !gone.has(r.path.toLowerCase())));
        setTicked((cur) => new Set([...cur].filter((p) => !gone.has(p.toLowerCase()))));
      },
    });

  // One sheet for all three steps: its title, width and buttons change, and focus moves into the new step.
  const body = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const dialog = body.current?.closest(".dialog");
    dialog?.querySelector<HTMLElement>("[data-autofocus]")?.focus();
  }, [step, rows === null]);

  const title =
    step === "before" ? `Uninstall ${app.name}?` : step === "left" ? `${app.name}: what's left` : `Uninstalling ${app.name}…`;

  const footer =
    step === "before" ? (
      <>
        <button className="btn" data-autofocus onClick={onClose}>
          Cancel
        </button>
        <button className="btn btn-danger" onClick={() => void start()}>
          Uninstall
        </button>
      </>
    ) : step === "stalled" ? (
      <>
        <button className="btn" onClick={onClose}>
          Cancel
        </button>
        <button
          className="btn btn-primary"
          data-autofocus
          onClick={() => {
            setProblem(null);
            setStep("running");
          }}
        >
          Try again
        </button>
      </>
    ) : step === "unresolved" ? (
      <>
        <button className="btn" onClick={onClose}>
          Cancel
        </button>
        <button className="btn btn-primary" data-autofocus onClick={showLeft}>
          It's finished
        </button>
      </>
    ) : step === "left" ? (
      <>
        <span className="un-picked tnum">{pickedRows.length > 0 ? `${pluralize(pickedRows.length, "item")} selected` : ""}</span>
        <button className="btn" onClick={onClose}>
          Close
        </button>
        {problem && rows?.length === 0 && (
          <button className="btn" onClick={() => void load()}>
            Look again
          </button>
        )}
        <button className="btn btn-primary" disabled={pickedRows.length === 0} onClick={review}>
          Review…
        </button>
      </>
    ) : undefined;

  return (
    <Dialog title={title} onClose={onClose} width={step === "left" ? 720 : 520} footer={footer}>
      <div ref={body}>
        {step === "before" && (
          <>
            <p className="dialog-lead">
              Seek runs {app.name}'s own uninstaller next. What it removes is up to that uninstaller, and that can't be undone. No
              restore point is made.
            </p>
            <dl className="un-facts">
              <dt>Install folder</dt>
              <dd>
                {app.folder ? (
                  <>
                    <MidPath path={app.folder} />
                    {app.folderGuessed && " (Seek's best guess)"}
                  </>
                ) : "Seek couldn't find this app's folder, so leftovers are matched by name and publisher only."}
              </dd>
              <dt>Size</dt>
              <dd className="tnum">{formatSize(app.size)}</dd>
            </dl>
            {app.running && (
              <p className="un-warn">
                <TriangleAlert size={14} aria-hidden="true" /> {app.name} is running. Close it first, or its uninstaller may stop to ask.
              </p>
            )}
            {app.note && <p className="un-warn is-soft">{app.note}</p>}
            {problem && <p className="review-warn">{problem}</p>}
            <p className="dialog-lead un-after">Afterwards Seek lists the leftovers it finds. You choose what moves to the Recycle Bin.</p>
          </>
        )}
        {step === "running" && (
          <div className="un-wait" tabIndex={-1} data-autofocus>
            <div className="spinner" aria-hidden="true" />
            <p className="dialog-lead">Windows is running the uninstaller. Follow its steps if it asks. This sheet carries on when it's done.</p>
          </div>
        )}
        {step === "stalled" && (
          <p className="dialog-lead">
            {problem} Seek can't tell how {app.name}'s uninstaller is doing. It keeps running.
          </p>
        )}
        {step === "unresolved" && (
          <p className="dialog-lead">
            {problem ?? `Seek can't tell whether ${app.name}'s uninstaller finished.`} If it did, choose It's finished to look for leftovers.
          </p>
        )}
        {step === "left" && (
          <>
            <p className="dialog-lead">
              Seek looked for what the uninstaller left behind. Nothing is selected. You review the list before anything moves to the
              Recycle Bin.
            </p>
            {rows === null ? (
              <SkeletonRows count={4} />
            ) : problem ? (
              <p className="review-warn">{problem}</p>
            ) : rows.length === 0 ? (
              <p className="dialog-lead">Nothing was left behind.</p>
            ) : (
              <div className="un-left" data-autofocus tabIndex={0}>
                <ul className="cl-list" aria-label="Left behind">
                  {rows.map((r) => (
                    <LeftLine
                      key={r.path}
                      row={r}
                      ticked={ticked.has(r.path)}
                      locked={isProtectedPath(r.path, protectedPaths)}
                      onToggle={() => toggle(r.path)}
                    />
                  ))}
                </ul>
              </div>
            )}
          </>
        )}
      </div>
    </Dialog>
  );
}
