import { useState, type KeyboardEvent, type MouseEvent } from "react";
import {
  ChevronDown,
  ChevronRight,
  FolderSync,
  Link2,
  Link2Off,
  PanelLeftClose,
  PanelLeftOpen,
  Pencil,
  Plus,
  RefreshCw,
  Settings,
  Share2,
  Trash2,
  TriangleAlert,
} from "lucide-react";
import { useApi } from "../api/context";
import type { Device, PcSummary, Peers } from "../api/types";
import { formatRelative } from "../lib/format";
import { errText, hasPeers } from "../lib/pcs";
import { driveToken, hasToken } from "../lib/query";
import { ContextMenu, type MenuItem } from "./ContextMenu";
import { Dialog } from "./Dialog";
import { DeviceItem } from "./DeviceItem";

interface Props {
  devices: Device[];
  progress: Record<string, number>;
  peers: Peers | null;
  query: string;
  collapsed: boolean;
  onCollapse: () => void;
  onToggleDevice: (token: string) => void;
  onAddShare: () => void;
  onLinkPc: () => void;
  onOpenSettings: () => void;
  onChanged: () => void;
  notify: (message: string) => void;
}

interface Group {
  pc: PcSummary;
  self: boolean;
  devices: Device[];
}

type Menu = { x: number; y: number } & ({ device: Device } | { pc: PcSummary });

export function syncedText(pc: PcSummary): string {
  if (pc.needsRelink) return "Needs re-linking";
  if (pc.lastSyncMs === 0) return pc.online ? "Connected · not synced yet" : "Not connected yet";
  return `${pc.online ? "Synced" : "Offline · synced"} ${formatRelative(pc.lastSyncMs)}`;
}

/** Drives by device (PC): this device first, then each linked one (null when none is linked: a flat list). */
function groupDevices(devices: Device[], peers: Peers | null): Group[] | null {
  if (!hasPeers(peers)) return null;
  const linked = new Set(peers.peers.map((p) => p.hostId));
  return [
    { pc: peers.thisPc, self: true, devices: devices.filter((d) => !linked.has(d.indexedByHost)) },
    ...peers.peers.map((pc) => ({ pc, self: false, devices: devices.filter((d) => d.indexedByHost === pc.hostId) })),
  ];
}

export function Sidebar(p: Props) {
  const api = useApi();
  const [menu, setMenu] = useState<Menu | null>(null);
  const [renaming, setRenaming] = useState<string | null>(null);
  const [forgetting, setForgetting] = useState<Device | null>(null);
  const [folded, setFolded] = useState<Set<string>>(() => new Set());
  const [renamingPc, setRenamingPc] = useState<PcSummary | null>(null);
  const [unlinking, setUnlinking] = useState<PcSummary | null>(null);

  const groups = groupDevices(p.devices, p.peers);
  const thisHost = p.peers?.thisPc.hostId;
  const isRemote = (d: Device) => !!thisHost && d.indexedByHost !== thisHost;
  const unshared = new Set(p.peers?.unshared ?? []);
  const sharingOn = p.peers?.sharing ?? true;

  // A name several drives share (every PC has a "Local Disk (C:)") is filtered together with its PC's name.
  const pcNames = new Map((p.peers ? [p.peers.thisPc, ...p.peers.peers] : []).map((pc) => [pc.hostId, pc.name]));
  const nameCount = new Map<string, number>();
  for (const d of p.devices) {
    const k = d.displayName.toLowerCase();
    nameCount.set(k, (nameCount.get(k) ?? 0) + 1);
  }
  const tokenFor = (d: Device) =>
    driveToken(
      d.displayName,
      (nameCount.get(d.displayName.toLowerCase()) ?? 0) > 1 ? pcNames.get(d.indexedByHost) : undefined,
    );

  const commitRename = async (d: Device, name: string) => {
    setRenaming(null);
    if (name && name !== d.displayName) {
      await api.renameDevice(d.id, name);
      p.onChanged();
    }
  };

  const syncNow = () => {
    api.syncNow().then(
      () => p.notify("Syncing with linked devices"),
      (e: unknown) => p.notify(errText(e)),
    );
  };

  const toggleFold = (host: string) =>
    setFolded((f) => {
      const n = new Set(f);
      if (n.has(host)) n.delete(host);
      else n.add(host);
      return n;
    });

  const deviceNote = (d: Device): string | undefined => {
    if (!hasPeers(p.peers) || isRemote(d)) return undefined;
    return !sharingOn || unshared.has(d.id) ? "not shared" : undefined;
  };

  const renderDevice = (d: Device) => (
    <DeviceItem
      key={d.id}
      device={d}
      scanned={p.progress[d.id]}
      collapsed={p.collapsed}
      active={hasToken(p.query, tokenFor(d))}
      renaming={renaming === d.id}
      note={deviceNote(d)}
      onToggle={() => p.onToggleDevice(tokenFor(d))}
      onContextMenu={(pt) => setMenu({ ...pt, device: d })}
      onRename={() => !isRemote(d) && setRenaming(d.id)}
      onRenameCommit={(n) => void commitRename(d, n)}
      onRenameCancel={() => setRenaming(null)}
    />
  );

  const deviceMenu = (d: Device): MenuItem[] => {
    if (isRemote(d)) {
      return [{ id: "sync", label: "Sync now", icon: <RefreshCw size={14} />, onSelect: syncNow }];
    }
    const items: MenuItem[] = [
      { id: "rename", label: "Rename", shortcut: "F2", icon: <Pencil size={14} />, onSelect: () => setRenaming(d.id) },
      {
        id: "rescan",
        label: "Rescan",
        icon: <FolderSync size={14} />,
        disabled: d.state === "offline",
        title: d.state === "offline" ? "Connect the drive to rescan it" : undefined,
        onSelect: () => void api.rescanDevice(d.id).then(p.onChanged),
      },
    ];
    if (hasPeers(p.peers)) {
      const shared = !unshared.has(d.id);
      items.push({
        id: "share",
        label: shared ? "Don't share with other devices" : "Share with other devices",
        icon: <Share2 size={14} />,
        disabled: !sharingOn,
        title: sharingOn ? undefined : "Sharing is turned off in Settings",
        onSelect: () => void api.setDeviceShared(d.id, !shared).catch((e: unknown) => p.notify(errText(e))),
      });
    }
    items.push({
      id: "forget",
      label: "Forget drive",
      icon: <Trash2 size={14} />,
      danger: true,
      separatorBefore: true,
      disabled: d.state !== "offline",
      title: d.state !== "offline" ? "Only offline drives can be forgotten" : undefined,
      onSelect: () => setForgetting(d),
    });
    return items;
  };

  const pcMenu = (pc: PcSummary): MenuItem[] => [
    { id: "sync", label: "Sync now", icon: <RefreshCw size={14} />, disabled: !pc.online, onSelect: syncNow },
    { id: "rename", label: "Rename…", icon: <Pencil size={14} />, onSelect: () => setRenamingPc(pc) },
    {
      id: "unlink",
      label: "Unlink…",
      icon: <Link2Off size={14} />,
      danger: true,
      separatorBefore: true,
      onSelect: () => setUnlinking(pc),
    },
  ];

  const renderGroup = (g: Group) => {
    const open = !folded.has(g.pc.hostId);
    if (p.collapsed) {
      return (
        <div key={g.pc.hostId} className="pc-group" role="group" aria-label={g.pc.name}>
          {!g.self && <div className="pc-sep" title={`${g.pc.name} · ${syncedText(g.pc)}`} />}
          {g.devices.map(renderDevice)}
        </div>
      );
    }
    const onContext = (e: MouseEvent) => {
      if (g.self) return;
      e.preventDefault();
      setMenu({ x: e.clientX, y: e.clientY, pc: g.pc });
    };
    const onKey = (e: KeyboardEvent) => {
      if (g.self) return;
      if (e.key === "ContextMenu" || (e.shiftKey && e.key === "F10")) {
        e.preventDefault();
        const r = e.currentTarget.getBoundingClientRect();
        setMenu({ x: r.left + 24, y: r.bottom - 4, pc: g.pc });
      }
    };
    return (
      <div key={g.pc.hostId} className="pc-group" role="group" aria-label={g.pc.name}>
        <button
          className="pc-head"
          aria-expanded={open}
          onClick={() => toggleFold(g.pc.hostId)}
          onContextMenu={onContext}
          onKeyDown={onKey}
        >
          {open ? <ChevronDown size={14} aria-hidden="true" /> : <ChevronRight size={14} aria-hidden="true" />}
          <span className="pc-name">{g.pc.name}</span>
          {g.self ? (
            <span className="pc-tag">This device</span>
          ) : g.pc.needsRelink ? (
            <span className="pc-warn" title="Seek was reinstalled on this device. Unlink it and link it again.">
              <TriangleAlert size={12} aria-hidden="true" /> Needs re-linking
            </span>
          ) : (
            <span className={`dot dot-${g.pc.online ? "online" : "offline"}`} aria-label={g.pc.online ? "online" : "offline"} />
          )}
        </button>
        {!g.self && !g.pc.needsRelink && <div className="pc-sub">{syncedText(g.pc)}</div>}
        {open &&
          (g.devices.length > 0 ? (
            g.devices.map(renderDevice)
          ) : (
            <div className="pc-empty">{g.self ? "No drives" : "No shared drives"}</div>
          ))}
      </div>
    );
  };

  return (
    <nav className={`sidebar${p.collapsed ? " is-collapsed" : ""}`} aria-label={groups ? "Devices" : "Drives"}>
      <div className="sidebar-head">
        {!p.collapsed && <h2>{groups ? "Devices" : "Drives"}</h2>}
        <span className="sidebar-actions">
          {!p.collapsed && hasPeers(p.peers) && (
            <button className="icon-btn" aria-label="Sync now" title="Sync with linked devices now" onClick={syncNow}>
              <RefreshCw size={15} />
            </button>
          )}
          <button
            className="icon-btn"
            aria-label={p.collapsed ? "Expand sidebar" : "Collapse sidebar"}
            title={p.collapsed ? "Expand sidebar" : "Collapse sidebar"}
            onClick={p.onCollapse}
          >
            {p.collapsed ? <PanelLeftOpen size={16} /> : <PanelLeftClose size={16} />}
          </button>
        </span>
      </div>

      <div className="device-list">{groups ? groups.map(renderGroup) : p.devices.map(renderDevice)}</div>

      <div className="sidebar-foot">
        <button className="side-btn" aria-label="Add network share" title="Add network share" onClick={p.onAddShare}>
          <Plus size={18} strokeWidth={1.75} aria-hidden="true" />
          {!p.collapsed && <span>Add network share</span>}
        </button>
        {p.peers && (
          <button
            className="side-btn"
            aria-label="Link a device"
            title={p.peers.peers.length >= 4 ? "Up to 5 devices can be linked" : "Link another PC to search its drives here"}
            disabled={p.peers.peers.length >= 4}
            onClick={p.onLinkPc}
          >
            <Link2 size={18} strokeWidth={1.75} aria-hidden="true" />
            {!p.collapsed && <span>Link a device</span>}
          </button>
        )}
        <button className="side-btn" aria-label="Settings" title="Settings" onClick={p.onOpenSettings}>
          <Settings size={18} strokeWidth={1.75} aria-hidden="true" />
          {!p.collapsed && <span>Settings</span>}
        </button>
      </div>

      {menu && (
        <ContextMenu
          x={menu.x}
          y={menu.y}
          label={"device" in menu ? `${menu.device.displayName} actions` : `${menu.pc.name} actions`}
          onClose={() => setMenu(null)}
          items={"device" in menu ? deviceMenu(menu.device) : pcMenu(menu.pc)}
        />
      )}

      {forgetting && (
        <Dialog
          title={`Forget ${forgetting.displayName}?`}
          onClose={() => setForgetting(null)}
          footer={
            <>
              <button className="btn" onClick={() => setForgetting(null)}>
                Cancel
              </button>
              <button
                className="btn btn-danger"
                data-autofocus
                onClick={() => {
                  const id = forgetting.id;
                  setForgetting(null);
                  void api.forgetDevice(id).then(p.onChanged);
                }}
              >
                Forget drive
              </button>
            </>
          }
        >
          <p className="dialog-lead">
            Seek will remove its saved index of this drive. Its files stop appearing in search until you plug it in and
            Seek scans it again.
          </p>
        </Dialog>
      )}

      {renamingPc && (
        <RenamePcDialog
          pc={renamingPc}
          onClose={() => setRenamingPc(null)}
          onSave={(name) => {
            const pc = renamingPc;
            setRenamingPc(null);
            api.renamePeer(pc.hostId, name).catch((e: unknown) => p.notify(errText(e)));
          }}
        />
      )}

      {unlinking && (
        <Dialog
          title={`Unlink ${unlinking.name}?`}
          onClose={() => setUnlinking(null)}
          footer={
            <>
              <button className="btn" onClick={() => setUnlinking(null)}>
                Cancel
              </button>
              <button
                className="btn btn-danger"
                data-autofocus
                onClick={() => {
                  const pc = unlinking;
                  setUnlinking(null);
                  api.unlink(pc.hostId).then(
                    () => p.notify(`Unlinked ${pc.name}`),
                    (e: unknown) => p.notify(errText(e)),
                  );
                }}
              >
                Unlink
              </button>
            </>
          }
        >
          <p className="dialog-lead">
            {unlinking.name}'s files stop appearing in search here, and this device's files stop appearing there. You can
            link the two devices again at any time with a new code.
          </p>
        </Dialog>
      )}
    </nav>
  );
}

function RenamePcDialog({ pc, onClose, onSave }: { pc: PcSummary; onClose: () => void; onSave: (name: string) => void }) {
  const [name, setName] = useState(pc.name);
  const valid = name.trim() !== "";
  return (
    <Dialog
      title={`Rename ${pc.name}`}
      onClose={onClose}
      footer={
        <>
          <button className="btn" onClick={onClose}>
            Cancel
          </button>
          <button className="btn btn-primary" disabled={!valid} onClick={() => onSave(name.trim())}>
            Save
          </button>
        </>
      }
    >
      <form
        onSubmit={(e) => {
          e.preventDefault();
          if (valid) onSave(name.trim());
        }}
      >
        <label className="field-label" htmlFor="pc-name">
          Name on this device
        </label>
        <div className="field">
          <input id="pc-name" data-autofocus value={name} spellCheck={false} onChange={(e) => setName(e.target.value)} />
        </div>
        <p className="field-help">Used in the sidebar and by the device: filter, e.g. device:{name.trim().split(" ")[0] || "laptop"}</p>
      </form>
    </Dialog>
  );
}
