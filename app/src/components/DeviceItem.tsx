import { useEffect, useRef, useState, type KeyboardEvent, type MouseEvent } from "react";
import { HardDrive, Server, Usb } from "lucide-react";
import type { Device } from "../api/types";
import { formatCount, formatRelative, formatSize } from "../lib/format";

interface Props {
  device: Device;
  scanned: number | undefined;
  collapsed: boolean;
  active: boolean;
  renaming: boolean;
  /** Extra state appended to the second line (e.g. "not shared"). */
  note?: string;
  onToggle: () => void;
  onContextMenu: (e: { x: number; y: number }) => void;
  onRename: () => void;
  onRenameCommit: (name: string) => void;
  onRenameCancel: () => void;
}

const KIND_ICON = { fixed: HardDrive, removable: Usb, network: Server } as const;

export function StateDot({ state }: { state: Device["state"] }) {
  return <span className={`dot dot-${state}`} aria-hidden="true" />;
}

const STATE_LABEL: Record<Device["state"], string> = {
  online: "Online",
  offline: "Offline",
  scanning: "Scanning",
  error: "Error",
};

export function DeviceItem(p: Props) {
  const { device: d } = p;
  const Icon = KIND_ICON[d.kind];
  const [draft, setDraft] = useState(d.displayName);
  const input = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (p.renaming) {
      setDraft(d.displayName);
      input.current?.focus();
      input.current?.select();
    }
  }, [p.renaming, d.displayName]);

  const used = d.totalBytes > 0 ? Math.min(1, (d.totalBytes - d.freeBytes) / d.totalBytes) : 0;
  const offline = d.state === "offline";
  const secondary =
    d.state === "offline"
      ? `${formatCount(d.entryCount)} files · seen ${formatRelative(d.lastSeen)}`
      : d.state === "scanning"
        ? `Scanning · ${formatCount(p.scanned ?? d.entryCount)} files`
        : d.state === "error"
          ? (d.error ?? "Can't read this drive")
          : `${formatSize(d.freeBytes)} free · ${formatCount(d.entryCount)} files`;
  const sub = p.note ? `${secondary} · ${p.note}` : secondary;

  const onKey = (e: KeyboardEvent) => {
    if (p.renaming) return;
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      p.onToggle();
    } else if (e.key === "F2") p.onRename();
    else if (e.key === "ContextMenu" || (e.shiftKey && e.key === "F10")) {
      e.preventDefault();
      const r = e.currentTarget.getBoundingClientRect();
      p.onContextMenu({ x: r.left + 24, y: r.bottom - 8 });
    }
  };

  const ctx = (e: MouseEvent) => {
    e.preventDefault();
    p.onContextMenu({ x: e.clientX, y: e.clientY });
  };

  const label = `${d.displayName}, ${STATE_LABEL[d.state].toLowerCase()}`;

  return (
    <div
      role="button"
      tabIndex={0}
      aria-pressed={p.active}
      aria-label={label}
      title={p.collapsed ? `${label}\n${sub}` : undefined}
      className={`device${p.active ? " is-active" : ""}${offline ? " is-offline" : ""}`}
      onClick={() => !p.renaming && p.onToggle()}
      onContextMenu={ctx}
      onKeyDown={onKey}
    >
      <span className="device-icon">
        <Icon size={18} strokeWidth={1.75} aria-hidden="true" />
        {p.collapsed && <StateDot state={d.state} />}
      </span>
      {!p.collapsed && (
        <span className="device-main">
          <span className="device-top">
            {p.renaming ? (
              <input
                ref={input}
                className="rename-input"
                aria-label={`Rename ${d.displayName}`}
                value={draft}
                onChange={(e) => setDraft(e.target.value)}
                onClick={(e) => e.stopPropagation()}
                onKeyDown={(e) => {
                  e.stopPropagation();
                  if (e.key === "Enter") p.onRenameCommit(draft.trim());
                  else if (e.key === "Escape") p.onRenameCancel();
                }}
                onBlur={() => p.onRenameCancel()}
              />
            ) : (
              <span className="device-name">{d.displayName}</span>
            )}
            <StateDot state={d.state} />
          </span>
          <span className="cap" aria-hidden="true">
            <span className="cap-fill" style={{ width: `${Math.round(used * 100)}%` }} data-high={used > 0.9} />
          </span>
          <span className="device-sub">{sub}</span>
        </span>
      )}
    </div>
  );
}
