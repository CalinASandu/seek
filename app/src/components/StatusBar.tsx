import type { Device, Peers, Status } from "../api/types";
import { formatCount, formatInt, formatRelative } from "../lib/format";
import { hasPeers, lastSync } from "../lib/pcs";

interface Props {
  total: number | null;
  elapsedMs: number;
  loading: boolean;
  /** The index changed since the results were computed. */
  stale: boolean;
  onRefresh: () => void;
  status: Status | null;
  devices: Device[];
  progress: Record<string, number>;
  peers?: Peers | null;
}

export function StatusBar({ total, elapsedMs, loading, stale, onRefresh, status, devices, progress, peers = null }: Props) {
  const scanning = devices.filter((d) => d.state === "scanning");
  const first = scanning[0];
  return (
    <footer className="statusbar" role="status" aria-live="polite">
      <span className="status-seg">
        {total === null ? (
          <span className="status-muted">Ready</span>
        ) : (
          <>
            <strong>{formatInt(total)}</strong> {total === 1 ? "result" : "results"}
            <span className="status-muted tnum"> in {elapsedMs < 10 ? elapsedMs.toFixed(1) : Math.round(elapsedMs)} ms</span>
            {loading && <span className="status-spin" aria-label="Searching" />}
            {stale && !loading && (
              <button type="button" className="status-refresh" onClick={onRefresh} title="Search again (F5)">
                Index updated · Refresh
              </button>
            )}
          </>
        )}
      </span>
      <span className="status-sep" />
      {/* drives (disks, sticks, shares); "devices" are PCs */}
      <span className="status-seg">
        <span className="dot dot-online" /> {status?.devicesOnline ?? 0}{" "}
        {(status?.devicesOnline ?? 0) === 1 ? "drive" : "drives"} online
        <span className="status-gap" />
        <span className="dot dot-offline" /> {status?.devicesOffline ?? 0} offline
      </span>
      {hasPeers(peers) && (
        <>
          <span className="status-sep" />
          <span
            className="status-seg"
            title={peers.peers.map((pc) => `${pc.name}: ${pc.online ? "connected" : "offline"}`).join(", ")}
          >
            {peers.peers.length + 1} devices
            <span className="status-muted">
              {" "}
              · {lastSync(peers.peers) ? `synced ${formatRelative(lastSync(peers.peers))}` : "not synced yet"}
            </span>
          </span>
        </>
      )}
      <span className="status-fill" />
      {first && (
        <span className="status-seg status-index">
          <span className="progress" aria-hidden="true">
            <span />
          </span>
          Indexing {first.displayName}
          {scanning.length > 1 ? ` +${scanning.length - 1}` : ""}
          <span className="status-muted tnum"> {formatCount(progress[first.id] ?? first.entryCount)} files</span>
        </span>
      )}
    </footer>
  );
}
