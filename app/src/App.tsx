import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createApi } from "./api";
import { ApiContext, useApi } from "./api/context";
import type { SeekApi, ServiceState, SortKey } from "./api/types";
import { AddShareDialog } from "./components/AddShareDialog";
import { LinkPcDialog } from "./components/LinkPcDialog";
import { EmptyNone, EmptyStart } from "./components/EmptyState";
import { FilterChips } from "./components/FilterChips";
import { ResultsTable } from "./components/ResultsTable";
import { SearchBar } from "./components/SearchBar";
import { ServiceSetup } from "./components/ServiceSetup";
import { SettingsPanel } from "./components/SettingsPanel";
import { CleanView } from "./components/clean/CleanView";
import { SpaceView, type Tab } from "./components/space/SpaceView";
import { Sidebar } from "./components/Sidebar";
import { StatusBar } from "./components/StatusBar";
import { TitleBar } from "./components/TitleBar";
import { ViewSwitch, type View } from "./components/ViewSwitch";
import { useDevices } from "./hooks/useDevices";
import { useHotkeys } from "./hooks/useHotkeys";
import { useMouseNav } from "./hooks/useMouseNav";
import { useSearch } from "./hooks/useSearch";
import { useSettings } from "./hooks/useSettings";
import { useSpaceNav } from "./hooks/useSpaceNav";
import { errText, remotePcNames } from "./lib/pcs";
import { composeQuery, filterOn, folderFilter, toggleFilter, type Filter, type SearchInput } from "./lib/query";

const SORT_KEYS: SortKey[] = ["name", "path", "size", "modified", "device"];
const COLLAPSE_KEY = "seek.sidebarCollapsed";
const VIEW_KEY = "seek.view";
const NO_SEGS: string[] = [];

function readView(): View {
  try {
    const v = localStorage.getItem(VIEW_KEY);
    return v === "space" || v === "clean" ? v : "search";
  } catch {
    return "search";
  }
}

function readCollapsed(): boolean {
  try {
    return localStorage.getItem(COLLAPSE_KEY) === "1";
  } catch {
    return false;
  }
}

export function App({ api }: { api?: SeekApi }) {
  const instance = useMemo(() => api ?? createApi(), [api]);
  return (
    <ApiContext.Provider value={instance}>
      <Shell />
    </ApiContext.Provider>
  );
}

function Shell() {
  const api = useApi();
  const [service, setService] = useState<ServiceState>("connecting");
  const [input, setInput] = useState<SearchInput>({ text: "", filters: [] });
  const [sort, setSort] = useState<SortKey>("name");
  const [descending, setDescending] = useState(false);
  const [collapsed, setCollapsed] = useState(readCollapsed);
  const [view, setViewState] = useState<View>(readView);
  const [spaceTab, setSpaceTab] = useState<Tab>("contents");
  const nav = useSpaceNav();
  // Decimator: the drive picked in the sidebar narrows Duplicates to it
  const [cleanDevice, setCleanDevice] = useState<string | null>(null);
  const [showSettings, setShowSettings] = useState(false);
  const [showShare, setShowShare] = useState(false);
  const [showLink, setShowLink] = useState(false);
  const [toast, setToast] = useState<string | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  const scrollerRef = useRef<HTMLDivElement>(null);
  const toastTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  // Set by "Search in this folder": the back button returns to Space.
  const backToSpace = useRef(false);

  const { devices, status, progress, peers, hub, refresh } = useDevices();
  const remotePc = useMemo(() => remotePcNames(devices, peers), [devices, peers]);
  const { settings, update } = useSettings();
  const settingsRef = useRef(settings);
  settingsRef.current = settings;
  const protectPath = useCallback(
    (path: string) => {
      const cur = settingsRef.current.protectedPaths;
      if (!cur.some((p) => p.toLowerCase() === path.toLowerCase())) update({ protectedPaths: [...cur, path] });
    },
    [update],
  );
  const scanDeviceIds = useMemo(
    () => devices.filter((d) => !remotePc.has(d.id) && d.state !== "offline").map((d) => d.id),
    [devices, remotePc],
  );
  const [cleanOpened, setCleanOpened] = useState(view === "clean");
  useEffect(() => {
    if (view === "clean") setCleanOpened(true);
  }, [view]);
  const query = useMemo(() => composeQuery(input), [input]);
  const search = useSearch(query, sort, descending);

  useEffect(() => {
    let live = true;
    api.serviceState().then((s) => live && setService(s)).catch(() => live && setService("error"));
    const off = api.onEvent((e) => e.type === "serviceState" && setService(e.state));
    return () => {
      live = false;
      off();
    };
  }, [api]);

  const notify = useCallback((message: string) => {
    setToast(message);
    clearTimeout(toastTimer.current);
    toastTimer.current = setTimeout(() => setToast(null), 1800);
  }, []);

  const onSort = useCallback(
    (key: SortKey) => {
      if (key === sort) setDescending((d) => !d);
      else {
        setSort(key);
        setDescending(false);
      }
    },
    [sort],
  );

  const focusSearch = () => {
    inputRef.current?.focus();
    inputRef.current?.select();
  };

  const setView = useCallback((v: View) => {
    backToSpace.current = false;
    setViewState(v);
    try {
      localStorage.setItem(VIEW_KEY, v);
    } catch {
      /* ignore */
    }
  }, []);

  const goBack = useCallback(() => {
    if (document.querySelector('[role="dialog"]')) return;
    if (view === "space") nav.step(-1);
    else if (backToSpace.current) setView("space");
  }, [nav.step, setView, view]);
  const goForward = useCallback(() => {
    if (view === "space" && !document.querySelector('[role="dialog"]')) nav.step(1);
  }, [nav.step, view]);
  useMouseNav(goBack, goForward);

  const hotkeys = useMemo(() => {
    const map: Record<string, () => void> = {
      "ctrl+f": () => {
        setView("search");
        setTimeout(focusSearch);
      },
      f5: search.refresh,
      "ctrl+shift+s": () => setView(view === "space" ? "search" : "space"),
      "ctrl+shift+d": () => setView(view === "clean" ? "search" : "clean"),
      "alt+arrowleft": goBack,
      "alt+arrowright": goForward,
    };
    if (view === "search") SORT_KEYS.forEach((k, i) => (map[`ctrl+${i + 1}`] = () => onSort(k)));
    return map;
  }, [goBack, goForward, onSort, search.refresh, setView, view]);
  useHotkeys(hotkeys);

  const toggleCollapsed = () => {
    setCollapsed((c) => {
      try {
        localStorage.setItem(COLLAPSE_KEY, c ? "0" : "1");
      } catch {
        /* ignore */
      }
      return !c;
    });
  };

  const setText = (text: string) => setInput((s) => ({ ...s, text }));

  const applyFilter = (filter: Filter, exclusive?: string[]) => {
    setInput((s) => toggleFilter(s, filter, exclusive));
    inputRef.current?.focus();
  };

  const focusGrid = () => scrollerRef.current?.focus();

  if (service !== "connected") {
    return (
      <div className="app app-bare">
        <TitleBar />
        <main className="main-bare">
          <ServiceSetup state={service} />
        </main>
      </div>
    );
  }

  const hasQuery = query.trim() !== "";
  // Space view: the chosen drive, else the first one of this PC that is online.
  const spaceDrive =
    devices.find((d) => d.id === nav.spot.device) ??
    devices.find((d) => !remotePc.has(d.id) && d.state !== "offline") ??
    devices[0];

  const searchIn = (folder: string) => {
    setInput({ text: "", filters: [folderFilter(folder)] });
    setView("search");
    backToSpace.current = true;
    setTimeout(() => inputRef.current?.focus());
  };

  const isFilterOn = (token: string) => filterOn(input, token);
  // Errors are positioned in the composed query; the box shows only the typed end of it.
  const typedError = search.error && {
    ...search.error,
    position: Math.max(0, search.error.position - (query.length - input.text.length)),
  };

  return (
    <div className={`app${collapsed ? " is-collapsed" : ""}`}>
      <TitleBar center={<ViewSwitch view={view} onChange={setView} />} />
      <Sidebar
        devices={devices}
        progress={progress}
        peers={peers}
        hub={hub}
        isFilterOn={isFilterOn}
        collapsed={collapsed}
        onCollapse={toggleCollapsed}
        selectedDevice={view === "space" ? (spaceDrive?.id ?? null) : view === "clean" ? cleanDevice : undefined}
        onSelectDevice={(d) =>
          view === "clean" ? setCleanDevice((cur) => (cur === d.id ? null : d.id)) : d.id !== spaceDrive?.id && nav.openDrive(d.id)
        }
        onToggleDevice={(f) => applyFilter(f)}
        onAddShare={() => setShowShare(true)}
        onLinkPc={() => setShowLink(true)}
        onOpenSettings={() => setShowSettings(true)}
        onChanged={() => void refresh()}
        notify={notify}
      />
      {cleanOpened && (
        <CleanView
          active={view === "clean"}
          deviceId={cleanDevice}
          deviceName={devices.find((d) => d.id === cleanDevice)?.displayName ?? null}
          scanDeviceIds={scanDeviceIds}
          protectedPaths={settings.protectedPaths}
          onProtect={protectPath}
          notify={notify}
        />
      )}
      {view === "clean" ? null : view === "space" ? (
        <main className="main">
          {spaceDrive ? (
            <SpaceView
              device={spaceDrive}
              segs={nav.spot.device === spaceDrive.id ? nav.spot.segs : NO_SEGS}
              tab={spaceTab}
              onTab={setSpaceTab}
              onGo={nav.go}
              onReplace={nav.replace}
              remotePc={remotePc.get(spaceDrive.id)}
              scanned={progress[spaceDrive.id]}
              notify={notify}
              onSearchIn={searchIn}
            />
          ) : (
            <div className="space-empty">
              <p className="space-empty-title">No drives yet</p>
            </div>
          )}
        </main>
      ) : (
      <main className="main">
        <div className="main-head">
          <SearchBar value={input.text} onChange={setText} error={typedError} inputRef={inputRef} onArrowDown={focusGrid} />
          <FilterChips filters={input.filters} isOn={isFilterOn} onToggle={applyFilter} />
        </div>
        <div className="main-body">
          {!hasQuery ? (
            <EmptyStart
              totalEntries={status?.totalEntries ?? 0}
              devices={devices.length}
              onPick={(q) => {
                setText(q);
                inputRef.current?.focus();
              }}
            />
          ) : search.active && search.total === 0 && !search.error ? (
            <EmptyNone query={input.text.trim() || input.filters.map((f) => f.label).join(", ")} />
          ) : search.active ? (
            <ResultsTable
              searchId={search.searchId}
              total={search.total}
              query={query}
              sort={sort}
              descending={descending}
              onSort={onSort}
              notify={notify}
              scrollerRef={scrollerRef}
              onStale={search.refresh}
              remotePc={remotePc}
            />
          ) : null}
        </div>
      </main>
      )}
      <StatusBar
        total={view === "search" && hasQuery && search.active ? search.total : null}
        elapsedMs={search.elapsedMs}
        loading={search.loading}
        stale={view === "search" && hasQuery && search.active && search.stale}
        onRefresh={search.refresh}
        status={status}
        devices={devices}
        progress={progress}
        peers={peers}
        hub={hub}
      />

      {toast && (
        <div className="toast" role="status">
          {toast}
        </div>
      )}
      {showShare && <AddShareDialog onClose={() => setShowShare(false)} onAdded={(n) => notify(`Added ${n}`)} />}
      {showSettings && (
        <SettingsPanel
          settings={settings}
          version={status?.serviceVersion}
          peers={peers}
          hub={hub}
          onChange={update}
          onSharing={(on) => void api.setSharing(on).catch((e: unknown) => notify(errText(e)))}
          onLinkPc={() => setShowLink(true)}
          onUnlinkHub={(done) =>
            void api.unlinkHub().then(
              () => done && notify(done),
              (e: unknown) => notify(errText(e)),
            )
          }
          onClose={() => setShowSettings(false)}
        />
      )}
      {showLink && <LinkPcDialog peers={peers} onClose={() => setShowLink(false)} onLinked={(n) => notify(`Linked with ${n}`)} />}
    </div>
  );
}
