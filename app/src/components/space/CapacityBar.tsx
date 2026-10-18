import type { Device } from "../../api/types";
import { formatSize } from "../../lib/format";
import type { Arc } from "../../lib/sunburst";
import { fillOf, itemLabel, liveHover } from "./Sunburst";

interface Props {
  device: Device;
  /** First-ring arcs of the drive root: the bar uses the chart's own colours, so a colour means one thing. */
  arcs: Arc[];
  /** Bytes Seek has indexed on the drive. */
  indexed: number;
  hoverKey: string | null;
  onHover: (key: string | null) => void;
}

/** The drive's capacity, like macOS Storage settings: what fills it (as in the chart), space Seek has not
 *  indexed (system areas it cannot read), and free space. */
export function CapacityBar({ device, arcs, indexed, hoverKey: rawHover, onHover }: Props) {
  const hoverKey = liveHover(rawHover, arcs);
  const total = device.totalBytes;
  if (total <= 1) return null;
  const used = Math.max(0, total - device.freeBytes);
  const rest = Math.max(0, used - indexed);
  const pct = (b: number) => `${(Math.min(b, total) / total) * 100}%`;
  return (
    <div className="capbar">
      <div
        className="capbar-track"
        role="img"
        aria-label={`${formatSize(used)} of ${formatSize(total)} used`}
        onPointerLeave={() => onHover(null)}
      >
        {arcs.map((a) => (
          <span
            key={a.key}
            className="capbar-seg"
            data-dim={hoverKey ? String(hoverKey !== a.key && !hoverKey.startsWith(`${a.key}\\`)) : undefined}
            style={{ width: pct(a.node.bytes), background: fillOf(a, false) }}
            title={`${itemLabel(a.node)}: ${formatSize(a.node.bytes)}`}
            onPointerEnter={() => onHover(a.key)}
          />
        ))}
        {rest > 0 && (
          <span
            className="capbar-seg is-rest"
            style={{ width: pct(rest) }}
            title={`Not indexed (system and protected areas): ${formatSize(rest)}`}
            onPointerEnter={() => onHover(null)}
          />
        )}
      </div>
      <div className="capbar-caption tnum">
        <span>
          <strong>{formatSize(used)}</strong> of {formatSize(total)} used
        </span>
        <span>{formatSize(Math.max(0, device.freeBytes))} free</span>
      </div>
    </div>
  );
}
