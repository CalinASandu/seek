import { useEffect, useRef, useState } from "react";
import { Link2, Monitor, Moon, Plus, Sun, X } from "lucide-react";
import type { Peers, Settings } from "../api/types";
import { syncedText } from "./Sidebar";

interface Props {
  settings: Settings;
  version: string | undefined;
  /** `null` when the service has sync turned off. */
  peers: Peers | null;
  onChange: (patch: Partial<Settings>) => void;
  onSharing: (enabled: boolean) => void;
  onLinkPc: () => void;
  onClose: () => void;
}

const THEMES: { id: Settings["theme"]; label: string; icon: typeof Sun }[] = [
  { id: "system", label: "System", icon: Monitor },
  { id: "light", label: "Light", icon: Sun },
  { id: "dark", label: "Dark", icon: Moon },
];

export function SettingsPanel({ settings, version, peers, onChange, onSharing, onLinkPc, onClose }: Props) {
  const [folder, setFolder] = useState("");
  const ref = useRef<HTMLElement>(null);

  useEffect(() => {
    const prev = document.activeElement as HTMLElement | null;
    ref.current?.querySelector<HTMLElement>("button")?.focus();
    return () => prev?.focus?.();
  }, []);

  const addFolder = () => {
    const f = folder.trim();
    if (!f || settings.excludeFolders.includes(f)) return;
    onChange({ excludeFolders: [...settings.excludeFolders, f] });
    setFolder("");
  };

  return (
    <div className="scrim scrim-side" onPointerDown={(e) => e.target === e.currentTarget && onClose()}>
      <aside
        ref={ref}
        className="panel"
        role="dialog"
        aria-modal="true"
        aria-label="Settings"
        onKeyDown={(e) => e.key === "Escape" && (e.stopPropagation(), onClose())}
      >
        <div className="panel-head">
          <h2>Settings</h2>
          <button className="icon-btn" aria-label="Close settings" onClick={onClose}>
            <X size={16} />
          </button>
        </div>

        <div className="panel-body">
          <section className="setting-group" aria-labelledby="set-theme">
            <h3 id="set-theme">Appearance</h3>
            <div className="segmented" role="radiogroup" aria-label="Theme">
              {THEMES.map(({ id, label, icon: Icon }) => (
                <button
                  key={id}
                  role="radio"
                  aria-checked={settings.theme === id}
                  className={settings.theme === id ? "is-on" : ""}
                  onClick={() => onChange({ theme: id })}
                >
                  <Icon size={14} aria-hidden="true" /> {label}
                </button>
              ))}
            </div>
          </section>

          <section className="setting-group" aria-labelledby="set-general">
            <h3 id="set-general">General</h3>
            <div className="setting-row">
              <div>
                <div className="setting-name" id="swl">Start with Windows</div>
                <div className="setting-desc">Open Seek in the tray when you sign in.</div>
              </div>
              <button
                role="switch"
                aria-checked={settings.startWithWindows}
                aria-labelledby="swl"
                className={`switch${settings.startWithWindows ? " is-on" : ""}`}
                onClick={() => onChange({ startWithWindows: !settings.startWithWindows })}
              >
                <span />
              </button>
            </div>
          </section>

          <section className="setting-group" aria-labelledby="set-pcs">
            <h3 id="set-pcs">Linked devices</h3>
            {peers ? (
              <>
                <div className="setting-row">
                  <div>
                    <div className="setting-name" id="share-pc">Share this device's files with linked devices</div>
                    <div className="setting-desc">Linked devices can search this device's file names, even while it is off.</div>
                  </div>
                  <button
                    role="switch"
                    aria-checked={peers.sharing}
                    aria-labelledby="share-pc"
                    className={`switch${peers.sharing ? " is-on" : ""}`}
                    onClick={() => onSharing(!peers.sharing)}
                  >
                    <span />
                  </button>
                </div>
                {peers.peers.length > 0 ? (
                  <ul className="peer-list">
                    {peers.peers.map((pc) => (
                      <li key={pc.hostId}>
                        <span className={`dot dot-${pc.online ? "online" : "offline"}`} aria-hidden="true" />
                        <span className="peer-name">{pc.name}</span>
                        <span className={`peer-sub${pc.needsRelink ? " is-warn" : ""}`}>{syncedText(pc)}</span>
                      </li>
                    ))}
                  </ul>
                ) : (
                  <p className="setting-desc">
                    Link your other PCs and laptops to search their drives from here. Files stay where they are; only
                    names are shared, encrypted, over your local network.
                  </p>
                )}
                <button className="btn" onClick={onLinkPc} disabled={peers.peers.length >= 4} title={peers.peers.length >= 4 ? "Up to 5 devices can be linked" : undefined}>
                  <Link2 size={14} aria-hidden="true" /> Link a device
                </button>
              </>
            ) : (
              <p className="setting-desc">Sync with other devices is turned off in the Seek service on this device.</p>
            )}
          </section>

          <section className="setting-group" aria-labelledby="set-excl">
            <h3 id="set-excl">Excluded folders</h3>
            <p className="setting-desc">Seek skips these folders and everything inside them.</p>
            {settings.excludeFolders.length > 0 ? (
              <ul className="excl-list">
                {settings.excludeFolders.map((f) => (
                  <li key={f}>
                    <span title={f}>{f}</span>
                    <button
                      className="icon-btn"
                      aria-label={`Remove ${f}`}
                      onClick={() => onChange({ excludeFolders: settings.excludeFolders.filter((x) => x !== f) })}
                    >
                      <X size={14} />
                    </button>
                  </li>
                ))}
              </ul>
            ) : (
              <p className="setting-desc">No folders excluded.</p>
            )}
            <form
              className="excl-add"
              onSubmit={(e) => {
                e.preventDefault();
                addFolder();
              }}
            >
              <div className="field">
                <input
                  value={folder}
                  placeholder={"C:\\Path\\to\\folder"}
                  aria-label="Folder to exclude"
                  spellCheck={false}
                  onChange={(e) => setFolder(e.target.value)}
                />
              </div>
              <button type="submit" className="btn" disabled={!folder.trim()}>
                <Plus size={14} aria-hidden="true" /> Add
              </button>
            </form>
          </section>
        </div>

        <div className="panel-foot">Seek {version ?? ""} · service connected</div>
      </aside>
    </div>
  );
}
