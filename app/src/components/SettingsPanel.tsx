import { useEffect, useRef, useState } from "react";
import { Link2, Link2Off, Monitor, Moon, Plus, Sun, X } from "lucide-react";
import type { HubStatus, Peers, Settings } from "../api/types";
import { hubSyncedText, hubUnlinked, inHubMode } from "../lib/pcs";
import { Dialog } from "./Dialog";
import { Segmented, type SegmentedItem } from "./Segmented";
import { syncedText } from "./Sidebar";

interface Props {
  settings: Settings;
  version: string | undefined;
  /** `null` when the service has sync turned off. */
  peers: Peers | null;
  /** The link to a hub, or the notice that the hub unlinked this PC; `null` without one. */
  hub: HubStatus | null;
  /** Why the saved settings can't be used (so the protected list can't be read), or null. */
  problem: string | null;
  /** A function form works out its change from the settings as they are when it is saved. */
  onChange: (patch: Partial<Settings> | ((s: Settings) => Partial<Settings>)) => void;
  onSharing: (enabled: boolean) => void;
  onLinkPc: () => void;
  /** Unlinks from the hub, or dismisses the notice. `done` is shown as a toast when it worked. */
  onUnlinkHub: (done?: string) => void;
  onClose: () => void;
}

const THEMES: SegmentedItem<Settings["theme"]>[] = [
  { id: "system", label: "System", icon: Monitor },
  { id: "light", label: "Light", icon: Sun },
  { id: "dark", label: "Dark", icon: Moon },
];

export function SettingsPanel({ settings, version, peers, hub, problem, onChange, onSharing, onLinkPc, onUnlinkHub, onClose }: Props) {
  const [folder, setFolder] = useState("");
  const [unlinkingHub, setUnlinkingHub] = useState(false);
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
            <Segmented items={THEMES} value={settings.theme} onChange={(theme) => onChange({ theme })} label="Theme" kind="radio" className="is-block" />
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
            {hub && (
              <div className={`hub-card${hubUnlinked(hub) ? " is-notice" : ""}`}>
                <div className="hub-head">
                  <span className={`dot dot-${hub.online ? "online" : "offline"}`} aria-hidden="true" />
                  <span className="peer-name">Hub · {hub.name}</span>
                  {!hubUnlinked(hub) && <span className="peer-sub">{hubSyncedText(hub)}</span>}
                </div>
                {hub.lastError && <p className="hub-error">{hub.lastError}</p>}
                {hub.pcs.length > 0 && (
                  <ul className="peer-list">
                    {hub.pcs.map((pc) => (
                      <li key={pc.hostId}>
                        <span className={`dot dot-${pc.online ? "online" : "offline"}`} aria-hidden="true" />
                        <span className="peer-name">{pc.name}</span>
                        <span className="peer-sub">{syncedText(pc)}</span>
                      </li>
                    ))}
                  </ul>
                )}
                {hubUnlinked(hub) ? (
                  <button className="btn" onClick={() => onUnlinkHub()}>
                    Dismiss
                  </button>
                ) : (
                  <button className="btn" onClick={() => setUnlinkingHub(true)}>
                    <Link2Off size={14} aria-hidden="true" /> Unlink hub
                  </button>
                )}
              </div>
            )}
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
                {inHubMode(hub) ? null : peers.peers.length > 0 ? (
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
                {!inHubMode(hub) && (
                  <button className="btn" onClick={onLinkPc} disabled={peers.peers.length >= 4} title={peers.peers.length >= 4 ? "Up to 5 devices can be linked" : undefined}>
                    <Link2 size={14} aria-hidden="true" /> Link a device
                  </button>
                )}
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

          <section className="setting-group" aria-labelledby="set-prot">
            <h3 id="set-prot">Protected from Cleanup</h3>
            <p className="setting-desc">Cleanup never moves these files or folders, or anything inside them.</p>
            {problem && (
              <p className="setting-desc review-warn" role="alert">
                {problem} The list couldn't be read, so removals are off until the file is fixed or deleted.
              </p>
            )}
            {settings.protectedPaths.length > 0 ? (
              <ul className="excl-list">
                {settings.protectedPaths.map((f) => (
                  <li key={f}>
                    <span title={f}>{f}</span>
                    <button
                      className="icon-btn"
                      aria-label={`Stop protecting ${f}`}
                      onClick={() => onChange((s) => ({ protectedPaths: s.protectedPaths.filter((x) => x !== f) }))}
                    >
                      <X size={14} />
                    </button>
                  </li>
                ))}
              </ul>
            ) : (
              <p className="setting-desc">Nothing is protected yet. In Cleanup, right-click a row or a review item and choose Always protect on anything you want kept.</p>
            )}
          </section>
        </div>

        <div className="panel-foot">Seek {version ?? ""} · service connected</div>
      </aside>
      {unlinkingHub && hub && (
        <Dialog
          title={`Unlink ${hub.name}?`}
          onClose={() => setUnlinkingHub(false)}
          footer={
            <>
              <button className="btn" onClick={() => setUnlinkingHub(false)}>
                Cancel
              </button>
              <button
                className="btn btn-danger"
                data-autofocus
                onClick={() => {
                  setUnlinkingHub(false);
                  onUnlinkHub(`Unlinked from ${hub.name}`);
                }}
              >
                Unlink hub
              </button>
            </>
          }
        >
          <p className="dialog-lead">
            This device stops syncing through {hub.name}, and the other PCs' files stop appearing in search here. You can link
            the hub again at any time with a new code.
          </p>
        </Dialog>
      )}
    </div>
  );
}
