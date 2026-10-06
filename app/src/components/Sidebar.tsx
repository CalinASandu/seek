import { useState } from "react";
import { FolderSync, PanelLeftClose, PanelLeftOpen, Pencil, Plus, Settings, Trash2 } from "lucide-react";
import { useApi } from "../api/context";
import type { Device } from "../api/types";
import { deviceToken, hasToken } from "../lib/query";
import { ContextMenu } from "./ContextMenu";
import { Dialog } from "./Dialog";
import { DeviceItem } from "./DeviceItem";

interface Props {
  devices: Device[];
  progress: Record<string, number>;
  query: string;
  collapsed: boolean;
  onCollapse: () => void;
  onToggleDevice: (token: string) => void;
  onAddShare: () => void;
  onOpenSettings: () => void;
  onChanged: () => void;
}

export function Sidebar(p: Props) {
  const api = useApi();
  const [menu, setMenu] = useState<{ x: number; y: number; device: Device } | null>(null);
  const [renaming, setRenaming] = useState<string | null>(null);
  const [forgetting, setForgetting] = useState<Device | null>(null);

  const commitRename = async (d: Device, name: string) => {
    setRenaming(null);
    if (name && name !== d.displayName) {
      await api.renameDevice(d.id, name);
      p.onChanged();
    }
  };

  return (
    <nav className={`sidebar${p.collapsed ? " is-collapsed" : ""}`} aria-label="Devices">
      <div className="sidebar-head">
        {!p.collapsed && <h2>Devices</h2>}
        <button
          className="icon-btn"
          aria-label={p.collapsed ? "Expand sidebar" : "Collapse sidebar"}
          title={p.collapsed ? "Expand sidebar" : "Collapse sidebar"}
          onClick={p.onCollapse}
        >
          {p.collapsed ? <PanelLeftOpen size={16} /> : <PanelLeftClose size={16} />}
        </button>
      </div>

      <div className="device-list">
        {p.devices.map((d) => (
          <DeviceItem
            key={d.id}
            device={d}
            scanned={p.progress[d.id]}
            collapsed={p.collapsed}
            active={hasToken(p.query, deviceToken(d.displayName))}
            renaming={renaming === d.id}
            onToggle={() => p.onToggleDevice(deviceToken(d.displayName))}
            onContextMenu={(pt) => setMenu({ ...pt, device: d })}
            onRename={() => setRenaming(d.id)}
            onRenameCommit={(n) => void commitRename(d, n)}
            onRenameCancel={() => setRenaming(null)}
          />
        ))}
      </div>

      <div className="sidebar-foot">
        <button className="side-btn" aria-label="Add network share" title="Add network share" onClick={p.onAddShare}>
          <Plus size={18} strokeWidth={1.75} aria-hidden="true" />
          {!p.collapsed && <span>Add network share</span>}
        </button>
        <button className="side-btn" aria-label="Settings" title="Settings" onClick={p.onOpenSettings}>
          <Settings size={18} strokeWidth={1.75} aria-hidden="true" />
          {!p.collapsed && <span>Settings</span>}
        </button>
      </div>

      {menu && (
        <ContextMenu
          x={menu.x}
          y={menu.y}
          label={`${menu.device.displayName} actions`}
          onClose={() => setMenu(null)}
          items={[
            { id: "rename", label: "Rename", shortcut: "F2", icon: <Pencil size={14} />, onSelect: () => setRenaming(menu.device.id) },
            {
              id: "rescan",
              label: "Rescan",
              icon: <FolderSync size={14} />,
              disabled: menu.device.state === "offline",
              title: menu.device.state === "offline" ? "Connect the device to rescan it" : undefined,
              onSelect: () => void api.rescanDevice(menu.device.id).then(p.onChanged),
            },
            {
              id: "forget",
              label: "Forget device",
              icon: <Trash2 size={14} />,
              danger: true,
              separatorBefore: true,
              disabled: menu.device.state !== "offline",
              title: menu.device.state !== "offline" ? "Only offline devices can be forgotten" : undefined,
              onSelect: () => setForgetting(menu.device),
            },
          ]}
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
                Forget device
              </button>
            </>
          }
        >
          <p className="dialog-lead">
            Seek will remove its saved index of this device. Its files stop appearing in search until you plug it in and
            Seek scans it again.
          </p>
        </Dialog>
      )}
    </nav>
  );
}
