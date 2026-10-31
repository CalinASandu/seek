import type { Device, HubStatus, PcSummary, Peers } from "../api/types";
import { formatRelative } from "./format";

/** True when other PCs are linked: the UI then groups devices by PC. */
export const hasPeers = (peers: Peers | null): peers is Peers => !!peers && peers.peers.length > 0;

/** Name for drives of a PC that is not linked (a former LAN PC that never joined the hub). */
export const OTHER_DEVICES = "Other devices";

/** Device id → name of the linked PC it belongs to; this PC's devices are absent. */
export function remotePcNames(devices: Device[], peers: Peers | null): Map<string, string> {
  const out = new Map<string, string>();
  if (!peers) return out;
  const names = new Map(peers.peers.map((p) => [p.hostId, p.name]));
  for (const d of devices) {
    if (d.indexedByHost !== peers.thisPc.hostId) out.set(d.id, names.get(d.indexedByHost) ?? OTHER_DEVICES);
  }
  return out;
}

/** Most recent exchange with any linked PC; 0 = never. */
export const lastSync = (peers: PcSummary[]): number => peers.reduce((m, p) => Math.max(m, p.lastSyncMs), 0);

/** Text of a rejected API call (Tauri rejects with a string, the mock with an Error). */
export const errText = (e: unknown): string => (e instanceof Error ? e.message : String(e));

/** `482913` → `482 913`, `12345678` → `1234 5678` */
export function spacedCode(code: string): string {
  const half = code.length / 2;
  return code.length === 6 || code.length === 8 ? `${code.slice(0, half)} ${code.slice(half)}` : code;
}

/** Prefixes the service puts on the error of `linkHub` when the cause is known (see `HUB_ERR_*` in seek-ipc). */
export const HUB_ERR_WRONG_CODE = "[hub:wrong-code] ";
export const HUB_ERR_NO_WINDOW = "[hub:no-window] ";
export const HUB_ERR_UNTRUSTED = "[hub:untrusted] ";

/** What to tell the user about a failed `linkHub`; errors without a known kind are shown as they are. */
export function hubLinkError(e: unknown): string {
  const text = errText(e);
  if (text.startsWith(HUB_ERR_WRONG_CODE)) return "That code didn't match. Run `seek-hub link` again for a new code.";
  if (text.startsWith(HUB_ERR_NO_WINDOW)) return "The hub isn't accepting links right now. Run `seek-hub link` on the hub.";
  if (text.startsWith(HUB_ERR_UNTRUSTED)) return "The hub's certificate isn't valid for that address.";
  return text;
}

/** The hub notice means the link is gone: this PC is back in LAN mode. */
export const hubUnlinked = (hub: HubStatus | null): boolean => hub?.unlinked === true;

/** Hub mode: this PC syncs through a hub (and not by direct links). */
export const inHubMode = (hub: HubStatus | null): hub is HubStatus => !!hub && !hubUnlinked(hub);

/** `synced 20 s ago` */
export const hubSyncedText = (hub: HubStatus): string =>
  hub.lastSyncMs ? `synced ${formatRelative(hub.lastSyncMs)}` : "not synced yet";
