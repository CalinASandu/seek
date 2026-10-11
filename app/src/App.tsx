import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createApi } from "./api";
import { ApiContext, useApi } from "./api/context";
import type { SeekApi, ServiceState, SortKey } from "./api/types";
import { AddShareDialog } from "./components/AddShareDialog";
import { EmptyNone, EmptyStart } from "./components/EmptyState";
import { FilterChips } from "./components/FilterChips";
import { ResultsTable } from "./components/ResultsTable";
import { SearchBar } from "./components/SearchBar";
import { ServiceSetup } from "./components/ServiceSetup";
import { SettingsPanel } from "./components/SettingsPanel";
import { Sidebar } from "./components/Sidebar";
import { StatusBar } from "./components/StatusBar";
import { TitleBar } from "./components/TitleBar";
import { useDevices } from "./hooks/useDevices";
import { useHotkeys } from "./hooks/useHotkeys";
import { useSearch } from "./hooks/useSearch";
import { useSettings } from "./hooks/useSettings";
import { toggleToken } from "./lib/query";

const SORT_KEYS: SortKey[] = ["name", "path", "size", "modified", "device"];
const COLLAPSE_KEY = "seek.sidebarCollapsed";

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
  const [query, setQuery] = useState("");
  const [sort, setSort] = useState<SortKey>("name");
  const [descending, setDescending] = useState(false);
  const [collapsed, setCollapsed] = useState(readCollapsed);
  const [showSettings, setShowSettings] = useState(false);
  const [showShare, setShowShare] = useState(false);
  const [toast, setToast] = useState<string | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  const scrollerRef = useRef<HTMLDivElement>(null);
  const toastTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);

  const { devices, status, progress, refresh } = useDevices();
  const { settings, update } = useSettings();
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

  const hotkeys = useMemo(() => {
    const map: Record<string, () => void> = { "ctrl+f": focusSearch, f5: search.refresh };
    SORT_KEYS.forEach((k, i) => (map[`ctrl+${i + 1}`] = () => onSort(k)));
    return map;
  }, [onSort, search.refresh]);
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

  const applyToken = (token: string, exclusive?: string[]) => {
    setQuery((q) => toggleToken(q, token, exclusive));
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

  return (
    <div className={`app${collapsed ? " is-collapsed" : ""}`}>
      <TitleBar />
      <Sidebar
        devices={devices}
        progress={progress}
        query={query}
        collapsed={collapsed}
        onCollapse={toggleCollapsed}
        onToggleDevice={(t) => applyToken(t)}
        onAddShare={() => setShowShare(true)}
        onOpenSettings={() => setShowSettings(true)}
        onChanged={() => void refresh()}
      />
      <main className="main">
        <div className="main-head">
          <SearchBar value={query} onChange={setQuery} error={search.error} inputRef={inputRef} onArrowDown={focusGrid} />
          <FilterChips query={query} onToggle={applyToken} />
        </div>
        <div className="main-body">
          {!hasQuery ? (
            <EmptyStart
              totalEntries={status?.totalEntries ?? 0}
              devices={devices.length}
              onPick={(q) => {
                setQuery(q);
                inputRef.current?.focus();
              }}
            />
          ) : search.active && search.total === 0 && !search.error ? (
            <EmptyNone query={query} />
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
            />
          ) : null}
        </div>
      </main>
      <StatusBar
        total={hasQuery && search.active ? search.total : null}
        elapsedMs={search.elapsedMs}
        loading={search.loading}
        stale={hasQuery && search.active && search.stale}
        onRefresh={search.refresh}
        status={status}
        devices={devices}
        progress={progress}
      />

      {toast && (
        <div className="toast" role="status">
          {toast}
        </div>
      )}
      {showShare && <AddShareDialog onClose={() => setShowShare(false)} onAdded={(n) => notify(`Added ${n}`)} />}
      {showSettings && (
        <SettingsPanel settings={settings} version={status?.serviceVersion} onChange={update} onClose={() => setShowSettings(false)} />
      )}
    </div>
  );
}
