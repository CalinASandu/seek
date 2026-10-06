import type { Device, Status } from "../api/types";
import { formatCount, formatInt } from "../lib/format";

interface Props {
  total: number | null;
  elapsedMs: number;
  loading: boolean;
  status: Status | null;
  devices: Device[];
  progress: Record<string, number>;
}

export function StatusBar({ total, elapsedMs, loading, status, devices, progress }: Props) {
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
          </>
        )}
      </span>
      <span className="status-sep" />
      <span className="status-seg">
        <span className="dot dot-online" /> {status?.devicesOnline ?? 0} online
        <span className="status-gap" />
        <span className="dot dot-offline" /> {status?.devicesOffline ?? 0} offline
      </span>
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
