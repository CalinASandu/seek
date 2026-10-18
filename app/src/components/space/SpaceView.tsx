import { useCallback, useEffect, useMemo, useState, type KeyboardEvent } from "react";
import { ClipboardCopy, FolderOpen, Search } from "lucide-react";
import { useApi } from "../../api/context";
import type { Device, SpaceNode } from "../../api/types";
import { useSpaceQuery } from "../../hooks/useSpaceQuery";
import { formatInt, formatRelative, formatSize } from "../../lib/format";
import { kindOf, layoutSunburst, type Arc } from "../../lib/sunburst";
import { ContextMenu, type MenuItem } from "../ContextMenu";
import { CapacityBar } from "./CapacityBar";
import { LargestList } from "./LargestList";
import { PathBar } from "./PathBar";
import { SpaceList } from "./SpaceList";
import { R, Sunburst } from "./Sunburst";
import { TypeList } from "./TypeList";

const RINGS = 3;
const GONE = "That folder no longer exists";
type Tab = "contents" | "largest" | "types";
const TABS: { id: Tab; label: string }[] = [
  { id: "contents", label: "Contents" },
  { id: "largest", label: "Largest" },
  { id: "types", label: "By type" },
];

interface Props {
  device: Device;
  /** Name of the linked PC the drive belongs to; undefined for this PC's drives. */
  remotePc: string | undefined;
  scanned: number | undefined;
  notify: (message: string) => void;
  onSearchIn: (folder: string) => void;
}

/** `D:\` + ["Games", "Steam"] → `D:\Games\Steam`; no segments → the mount point itself. */
export function folderPath(mount: string, segs: string[]): string {
  if (segs.length === 0) return mount;
  return `${mount.replace(/\\+$/, "")}\\${segs.join("\\")}`;
}

type Menu = { x: number; y: number; path: string; folder: boolean };

export function SpaceView({ device, remotePc, scanned, notify, onSearchIn }: Props) {
  const api = useApi();
  const [where, setWhere] = useState<Record<string, string[]>>({});
  const segs = useMemo(() => where[device.id] ?? [], [where, device.id]);
  const setSegs = useCallback((s: string[]) => setWhere((w) => ({ ...w, [device.id]: s })), [device.id]);
  const [tab, setTab] = useState<Tab>("contents");
  const [hover, setHover] = useState<string | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [menu, setMenu] = useState<Menu | null>(null);

  const path = folderPath(device.lastMountPoint, segs);
  const key = `${device.id}|${path}`;
  const atRoot = segs.length === 0;

  const tree = useSpaceQuery(key, () =>
    api.spaceTree(device.id, path, RINGS).then((node) => ({ node, deviceId: device.id, segs })),
  );
  const largest = useSpaceQuery(tab === "largest" ? key : null, () => api.spaceLargest(device.id, path, 50));
  const types = useSpaceQuery(tab === "types" ? key : null, () => api.spaceTypes(device.id, path));

  // A folder that disappeared (deleted, renamed, or hidden from you): show its parent instead.
  useEffect(() => {
    if (tree.error === GONE && tree.errorKey === key && segs.length > 0) {
      notify(GONE);
      setSegs(segs.slice(0, -1));
    }
  }, [tree.error, tree.errorKey, key, segs, setSegs, notify]);

  useEffect(() => {
    setHover(null);
    setSelected(null);
  }, [key]);

  const data = tree.data && tree.data.deviceId === device.id ? tree.data : null;
  const node = data?.node ?? null;
  const shownSegs = data?.segs ?? segs;
  const nodeKey = shownSegs.join("\\");
  const title = shownSegs.length ? shownSegs[shownSegs.length - 1]! : device.displayName;
  const arcs = useMemo(() => (node ? layoutSunburst(node, nodeKey, RINGS, R) : []), [node, nodeKey]);
  const hues = useMemo(() => new Map(arcs.filter((a) => a.depth === 1).map((a) => [a.key, a.hue])), [arcs]);

  const local = !remotePc;
  const canReveal = local && device.state !== "offline";

  const open = (name: string) => setSegs([...shownSegs, name]);
  const up = () => shownSegs.length > 0 && setSegs(shownSegs.slice(0, -1));

  const menuItems = (m: Menu): MenuItem[] => {
    const items: MenuItem[] = [];
    if (local) {
      items.push({
        id: "reveal",
        label: "Show in Explorer",
        icon: <FolderOpen size={14} />,
        disabled: !canReveal,
        title: canReveal ? undefined : "The drive is offline",
        onSelect: () => void api.revealPath(m.path).catch(() => notify("Couldn't open Explorer")),
      });
    }
    items.push({
      id: "copy",
      label: "Copy path",
      icon: <ClipboardCopy size={14} />,
      separatorBefore: items.length > 0,
      onSelect: () =>
        void navigator.clipboard.writeText(m.path).then(
          () => notify("Copied path"),
          () => notify("Couldn't copy to the clipboard"),
        ),
    });
    if (m.folder) {
      items.push({ id: "search", label: "Search in this folder", icon: <Search size={14} />, onSelect: () => onSearchIn(m.path) });
    }
    return items;
  };

  const menuFor = (n: SpaceNode, x: number, y: number) =>
    setMenu({ x, y, path: folderPath(path, [n.name]), folder: kindOf(n) === "folder" });

  const onKey = (e: KeyboardEvent) => {
    if (e.key === "Backspace" || e.key === "ArrowLeft" || (e.altKey && e.key === "ArrowUp")) {
      if (shownSegs.length === 0) return;
      e.preventDefault();
      up();
    }
  };

  const measuring = device.state === "scanning" && (!node || node.bytes === 0);
  const status =
    device.state === "offline"
      ? `Offline · indexed ${formatRelative(device.indexUpdatedAt)}`
      : remotePc
        ? `On ${remotePc} · indexed ${formatRelative(device.indexUpdatedAt)}`
        : null;

  return (
    <div className="space" onKeyDown={onKey} data-hover={hover ?? undefined}>
      <div className="space-head">
        <div className="space-head-row">
          <PathBar device={device} segs={segs} onGo={(depth) => setSegs(segs.slice(0, depth))} />
          <span className="space-meta tnum">
            {node && `${formatSize(node.bytes)} · ${formatInt(node.files)} ${node.files === 1 ? "file" : "files"}`}
            {status && <span className="space-status">{status}</span>}
          </span>
        </div>
        {atRoot && node && shownSegs.length === 0 && (
          <CapacityBar
            device={device}
            arcs={arcs.filter((a) => a.depth === 1)}
            indexed={node.bytes}
            hoverKey={hover}
            onHover={setHover}
          />
        )}
      </div>

      <div className="space-body">
        <div className="space-chart">
          {measuring ? (
            <div className="space-empty">
              <div className="spinner" />
              <p className="space-empty-title">Measuring {device.displayName}…</p>
              <p className="space-empty-sub tnum">{scanned ? `${formatInt(scanned)} items so far` : "Reading the drive"}</p>
            </div>
          ) : !node ? (
            tree.error ? (
              <div className="space-empty">
                <p className="space-empty-title">Couldn't measure this folder</p>
                <p className="space-empty-sub">{tree.error}</p>
              </div>
            ) : (
              <div className="sunburst is-loading" aria-hidden="true" />
            )
          ) : node.bytes === 0 ? (
            <div className="space-empty">
              <p className="space-empty-title">{atRoot ? "Nothing indexed here yet" : "This folder is empty"}</p>
            </div>
          ) : (
            <Sunburst
              arcs={arcs}
              scene={device.id}
              node={node}
              title={title}
              parentName={shownSegs.length === 0 ? null : shownSegs.length === 1 ? device.displayName : shownSegs[shownSegs.length - 2]!}
              hoverKey={hover}
              onHover={setHover}
              onOpen={(a: Arc) => setSegs(a.key.split("\\"))}
              onUp={up}
              onContextMenu={(a, x, y) => setMenu({ x, y, path: folderPath(device.lastMountPoint, a.key.split("\\")), folder: a.kind === "folder" })}
            />
          )}
        </div>

        <aside className="space-side" aria-label="Details">
          <div className="space-tabs">
            <div className="segctl is-small" role="tablist" aria-label="Show" data-index={TABS.findIndex((t) => t.id === tab)}>
              <span className="segctl-thumb" aria-hidden="true" />
              {TABS.map((t) => (
                <button key={t.id} role="tab" aria-selected={tab === t.id} className="segment" onClick={() => setTab(t.id)}>
                  {t.label}
                </button>
              ))}
            </div>
          </div>
          <div className="space-panel">
            {tab === "contents" &&
              (node ? (
                <SpaceList
                  node={node}
                  nodeKey={nodeKey}
                  hues={hues}
                  hoverKey={hover}
                  selected={selected}
                  onSelect={setSelected}
                  onHover={setHover}
                  onOpen={(c) => open(c.name)}
                  onContextMenu={menuFor}
                />
              ) : (
                <LargestList files={null} base={path} onReveal={null} onContextMenu={() => {}} />
              ))}
            {tab === "largest" && (
              <LargestList
                files={largest.data && largest.dataKey === key ? largest.data : null}
                base={path}
                onReveal={canReveal ? (f) => void api.revealPath(f.fullPath) : null}
                onContextMenu={(f, x, y) => setMenu({ x, y, path: f.fullPath, folder: false })}
              />
            )}
            {tab === "types" && <TypeList types={types.data && types.dataKey === key ? types.data : null} />}
          </div>
        </aside>
      </div>

      {menu && (
        <ContextMenu x={menu.x} y={menu.y} items={menuItems(menu)} label="Item actions" onClose={() => setMenu(null)} />
      )}
    </div>
  );
}
