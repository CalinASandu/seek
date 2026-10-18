import { ChevronRight, HardDrive, Network, Usb } from "lucide-react";
import type { Device } from "../../api/types";

interface Props {
  device: Device;
  segs: string[];
  onGo: (depth: number) => void;
}

const DRIVE_ICON = { fixed: HardDrive, removable: Usb, network: Network } as const;

/** Breadcrumbs like Finder's path bar: drive › folder › folder (the last one is where you are). */
export function PathBar({ device, segs, onGo }: Props) {
  const Icon = DRIVE_ICON[device.kind];
  const crumbs = [device.displayName, ...segs];
  return (
    <nav className="pathbar" aria-label="Folder path">
      <ol>
        {crumbs.map((name, i) => (
          <li key={`${i}-${name}`}>
            {i > 0 && <ChevronRight className="pathbar-sep" size={13} aria-hidden="true" />}
            <button
              className="pathbar-crumb"
              aria-current={i === crumbs.length - 1 ? "page" : undefined}
              onClick={() => onGo(i)}
            >
              {i === 0 && <Icon size={14} strokeWidth={1.75} aria-hidden="true" />}
              <span>{name}</span>
            </button>
          </li>
        ))}
      </ol>
    </nav>
  );
}
