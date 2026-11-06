import { memo, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { History } from "lucide-react";
import { useApi } from "../../api/context";
import type { AppRow, PrepareRequest } from "../../api/types";
import { useCleanup, type CleanController } from "../../hooks/useCleanup";
import { groupFreeable, visibleGroups } from "../../lib/clean";
import { formatSize, pluralize } from "../../lib/format";
import { useStore } from "../../lib/store";
import { Segmented, type SegmentedItem } from "../Segmented";
import { AppsTab } from "./AppsTab";
import { DuplicatesTab } from "./DuplicatesTab";
import { LeftoversTab } from "./LeftoversTab";
import { MidPath } from "./parts";
import { RemovedSheet } from "./RemovedSheet";
import { RowActionsProvider, type RowActions } from "./rowMenu";
import { ReviewSheet } from "./ReviewSheet";
import { UninstallSheet } from "./UninstallSheet";

const ERROR_TOAST_MS = 5000;

export type CleanTab = "duplicates" | "leftovers" | "apps";
const TABS: SegmentedItem<CleanTab>[] = [
  { id: "duplicates", label: "Duplicates" },
  { id: "leftovers", label: "Leftovers" },
  { id: "apps", label: "Apps" },
];

/** What the review sheet is opened with. `after` runs when it closes, with the paths that really moved. */
export interface SheetSpec {
  request: PrepareRequest;
  picked: ReadonlySet<string> | null;
  contents: ReadonlyMap<string, string[]>;
  after?: (moved: string[]) => void;
  /** Runs when a removal went through (not on Cancel): the tab's ticks are cleared. */
  clear?: () => void;
}

interface Props {
  /** Shown. Hidden, the view keeps its state and does no work. */
  active: boolean;
  /** A drive picked in the sidebar: Duplicates narrows to it. */
  deviceId: string | null;
  deviceName: string | null;
  /** The drives of this PC that can be scanned. */
  scanDeviceIds: string[];
  protectedPaths: string[];
  onProtect: (path: string) => Promise<void>;
  /** Takes one entry off the protected list (the entry itself, not a path inside it). */
  onUnprotect: (entry: string) => Promise<void>;
  /** `ms`: how long the message stays; errors pass a longer time. */
  notify: (message: string, ms?: number) => void;
}

export const CleanView = memo(function CleanView({ active, deviceId, deviceName, scanDeviceIds, protectedPaths, onProtect, onUnprotect, notify }: Props) {
  const api = useApi();
  const ctl = useCleanup(api, active);
  const [tab, setTab] = useState<CleanTab>("duplicates");
  const [seen, setSeen] = useState<Record<CleanTab, boolean>>({ duplicates: true, leftovers: false, apps: false });
  const [sheet, setSheet] = useState<SheetSpec | null>(null);
  const [removedOpen, setRemovedOpen] = useState(false);
  const [uninstalling, setUninstalling] = useState<AppRow | null>(null);
  const body = useRef<HTMLDivElement>(null);
  const top = useRef<HTMLDivElement>(null);
  const anySheet = !!sheet || !!uninstalling || removedOpen;
  const wasSheet = useRef(false);

  // When a sheet closes and what opened it is gone (its rows were removed), focus goes to the active tab, not the page.
  useEffect(() => {
    if (wasSheet.current && !anySheet) {
      const at = document.activeElement as HTMLElement | null;
      if (!at || at === document.body || !at.isConnected || (at as HTMLButtonElement).disabled) {
        top.current?.querySelector<HTMLElement>('[role="tab"][aria-selected="true"]')?.focus();
      }
    }
    wasSheet.current = anySheet;
  }, [anySheet]);

  useEffect(() => ctl.setProtected(protectedPaths), [ctl, protectedPaths]);

  // Leftovers and Apps are read the first time their tab is shown; Duplicates waits for Scan.
  useEffect(() => {
    if (!active) return;
    setSeen((s) => (s[tab] ? s : { ...s, [tab]: true }));
    if (tab === "leftovers" && ctl.leftovers.get().status === "idle") ctl.loadLeftovers();
    if (tab === "apps" && ctl.apps.get().status === "idle") ctl.loadApps();
  }, [active, tab, ctl]);

  const openSheet = useCallback((spec: SheetSpec) => setSheet(spec), []);
  const closeSheet = (moved: string[], committed: boolean) => {
    const done = sheet;
    setSheet(null);
    if (committed) done?.clear?.();
    if (moved.length > 0) {
      ctl.moveOut(moved, body.current?.querySelector<HTMLElement>(".clean-panel:not([hidden]) .clean-scroll") ?? null);
    }
    done?.after?.(moved);
  };

  // The row menus: a save that fails is said out loud and the row is not shown as protected.
  const rowActions = useMemo<RowActions>(() => {
    const say = (what: string) => (e: unknown) => notify(`${what} ${e instanceof Error ? e.message : String(e)}`, ERROR_TOAST_MS);
    return {
      protectedPaths,
      protect: (path) => onProtect(path).catch(say('"Always protect" could not be saved.')),
      unprotect: (entry) => onUnprotect(entry).catch(say("The protected list could not be saved.")),
      reveal: (path) => api.revealPath(path).catch(say("Explorer could not show it.")),
    };
  }, [api, notify, onProtect, onUnprotect, protectedPaths]);

  const scan = () => ctl.scanStart(deviceId ? [deviceId] : scanDeviceIds);

  return (
    <RowActionsProvider value={rowActions}>
      <main className="main clean" hidden={!active}>
        <header className="clean-head">
          <div className="clean-top" ref={top}>
            <Segmented items={TABS} value={tab} onChange={setTab} label="Cleanup" kind="tab" />
            <button className="clean-recent" onClick={() => setRemovedOpen(true)}>
              <History size={14} aria-hidden="true" />
              Recently removed
            </button>
          </div>
          {tab === "duplicates" && <DuplicatesHead ctl={ctl} deviceId={deviceId} deviceName={deviceName} onScan={scan} />}
          {tab === "leftovers" && <LeftoversHead ctl={ctl} onScan={() => ctl.loadLeftovers()} />}
          {tab === "apps" && <AppsHead ctl={ctl} onScan={() => ctl.loadApps()} />}
        </header>

        <div className="clean-body" ref={body}>
          <div className="clean-panel" hidden={tab !== "duplicates"}>
            <DuplicatesTab ctl={ctl} deviceId={deviceId} deviceName={deviceName} onReview={openSheet} />
          </div>
          {seen.leftovers && (
            <div className="clean-panel" hidden={tab !== "leftovers"}>
              <LeftoversTab ctl={ctl} onReview={openSheet} />
            </div>
          )}
          {seen.apps && (
            <div className="clean-panel" hidden={tab !== "apps"}>
              <AppsTab ctl={ctl} onUninstall={setUninstalling} />
            </div>
          )}
        </div>

        {uninstalling && (
          <UninstallSheet
            app={uninstalling}
            protectedPaths={protectedPaths}
            onReview={openSheet}
            onUninstalled={() => ctl.loadApps()}
            onClose={() => setUninstalling(null)}
          />
        )}
        {removedOpen && <RemovedSheet onClose={() => setRemovedOpen(false)} notify={notify} />}
        {sheet && (
          <ReviewSheet
            request={sheet.request}
            picked={sheet.picked}
            contents={sheet.contents}
            onProtect={onProtect}
            onClose={closeSheet}
          />
        )}
      </main>
    </RowActionsProvider>
  );
});

// ---- the head: one number and one action per tab ---------------------------------------------------------------

function Hero({
  figure,
  caption,
  action,
  disabled,
  onAction,
}: {
  figure: string;
  caption: string;
  action: string;
  disabled: boolean;
  onAction: () => void;
}) {
  return (
    <div className="clean-hero">
      <div className="clean-hero-text">
        <div className="clean-figure tnum" aria-live="polite">
          {figure}
        </div>
        <div className="clean-caption">{caption}</div>
      </div>
      <button className="btn btn-primary clean-action" disabled={disabled} onClick={onAction}>
        {action}
      </button>
    </div>
  );
}

function ScanLine({ fraction, text, current, onCancel }: { fraction: number | null; text: string; current?: string | null; onCancel?: () => void }) {
  return (
    <div className="scanline is-on">
      <div className={`scanline-track${fraction === null ? " is-indeterminate" : ""}`} role="progressbar" aria-label="Progress" aria-valuenow={fraction === null ? undefined : Math.round(fraction * 100)}>
        <span style={fraction === null ? undefined : { transform: `scaleX(${Math.max(0.01, fraction)})` }} />
      </div>
      <div className="scanline-row">
        <span className="scanline-text">
          {current ? (
            <>
              Checking <MidPath path={current} />
            </>
          ) : (
            text
          )}
        </span>
        {fraction !== null && <span className="scanline-num tnum">{text}</span>}
        {onCancel && (
          <button className="link-btn" onClick={onCancel}>
            Cancel
          </button>
        )}
      </div>
    </div>
  );
}

const IdleLine = () => <div className="scanline" aria-hidden="true" />;

function DuplicatesHead({ ctl, deviceId, deviceName, onScan }: { ctl: CleanController; deviceId: string | null; deviceName: string | null; onScan: () => void }) {
  const status = useStore(ctl.dup, (s) => s.status);
  const groups = useStore(ctl.dup, (s) => s.groups);
  const checked = useStore(ctl.dup, (s) => s.checkedBytes);
  const total = useStore(ctl.dup, (s) => s.totalBytes);
  const current = useStore(ctl.dup, (s) => s.current);
  const prot = useStore(ctl.protect, (s) => s);
  const shown = useMemo(() => visibleGroups(groups, deviceId, prot), [groups, deviceId, prot]);
  const free = useMemo(() => shown.reduce((s, g) => s + groupFreeable(g, prot), 0), [shown, prot]);
  const stopping = status === "cancelling";
  const scanning = status === "scanning" || stopping;
  const none = (status === "done" || status === "cancelled") && shown.length === 0;
  const where = deviceName ? ` on ${deviceName}` : "";

  const figure = status === "idle" || status === "error" || none ? "Duplicate files" : formatSize(free);
  const caption =
    none
      ? "None found in the last scan"
      : status === "idle"
      ? deviceName
        ? `Finds exact copies on ${deviceName}.`
        : "Finds files that are exact copies of each other."
      : status === "error"
        ? "The scan didn't start."
        : scanning
          ? `in duplicates so far${where} · ${pluralize(shown.length, "group")}`
          : status === "cancelled"
            ? `in duplicates found before you stopped${where} · ${pluralize(shown.length, "group")}`
            : `in duplicates${where} · ${pluralize(shown.length, "group")}`;

  return (
    <>
      <Hero figure={figure} caption={caption} action={status === "idle" || status === "error" ? "Scan" : "Rescan"} disabled={scanning} onAction={onScan} />
      {scanning ? (
        <ScanLine
          fraction={total > 0 ? Math.min(1, checked / total) : null}
          text={stopping ? "Stopping…" : total > 0 ? `${formatSize(checked)} of ${formatSize(total)}` : "Starting…"}
          current={stopping ? null : current}
          onCancel={stopping ? undefined : () => ctl.scanCancel()}
        />
      ) : (
        <IdleLine />
      )}
    </>
  );
}

function LeftoversHead({ ctl, onScan }: { ctl: CleanController; onScan: () => void }) {
  const status = useStore(ctl.leftovers, (s) => s.status);
  const rows = useStore(ctl.leftovers, (s) => s.rows);
  const loading = status === "loading";
  const likely = useMemo(() => rows.filter((r) => r.level !== "probablyInUse"), [rows]);
  const bytes = useMemo(() => likely.reduce((s, r) => s + r.size, 0), [likely]);
  const first = status === "idle" || (loading && rows.length === 0);
  const none = status === "ready" && rows.length === 0;
  return (
    <>
      <Hero
        figure={first || none ? "Leftovers" : formatSize(bytes)}
        caption={
          none
            ? "None found in the last scan"
            : first
            ? "Folders that apps you removed left behind."
            : status === "error"
              ? "Couldn't look for leftovers."
              : `in ${pluralize(likely.length, "folder")} that may be left over`
        }
        action={first ? "Scan" : "Rescan"}
        disabled={loading}
        onAction={onScan}
      />
      {loading ? <ScanLine fraction={null} text="Looking in Program Files and AppData…" /> : <IdleLine />}
    </>
  );
}

function AppsHead({ ctl, onScan }: { ctl: CleanController; onScan: () => void }) {
  const status = useStore(ctl.apps, (s) => s.status);
  const rows = useStore(ctl.apps, (s) => s.rows);
  const loading = status === "loading";
  const bytes = useMemo(() => rows.reduce((s, a) => s + a.size, 0), [rows]);
  const first = status === "idle" || (loading && rows.length === 0);
  const none = status === "ready" && rows.length === 0;
  return (
    <>
      <Hero
        figure={first || none ? "Apps" : formatSize(bytes)}
        caption={none ? "None found in the last scan" : first ? "Programs installed on this PC." : status === "error" ? "Couldn't read the list of apps." : `in ${pluralize(rows.length, "app")}`}
        action={first ? "Scan" : "Rescan"}
        disabled={loading}
        onAction={onScan}
      />
      {loading ? <ScanLine fraction={null} text="Reading the list of apps…" /> : <IdleLine />}
    </>
  );
}

