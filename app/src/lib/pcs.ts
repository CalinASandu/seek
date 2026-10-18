import type { Device, PcSummary, Peers } from "../api/types";

/** True when other PCs are linked: the UI then groups devices by PC. */
export const hasPeers = (peers: Peers | null): peers is Peers => !!peers && peers.peers.length > 0;

/** Device id → name of the linked PC it belongs to; this PC's devices are absent. */
export function remotePcNames(devices: Device[], peers: Peers | null): Map<string, string> {
  const out = new Map<string, string>();
  if (!peers) return out;
  const names = new Map(peers.peers.map((p) => [p.hostId, p.name]));
  for (const d of devices) {
    if (d.indexedByHost !== peers.thisPc.hostId) out.set(d.id, names.get(d.indexedByHost) ?? "Linked device");
  }
  return out;
}

/** Most recent exchange with any linked PC; 0 = never. */
export const lastSync = (peers: PcSummary[]): number => peers.reduce((m, p) => Math.max(m, p.lastSyncMs), 0);

/** Text of a rejected API call (Tauri rejects with a string, the mock with an Error). */
export const errText = (e: unknown): string => (e instanceof Error ? e.message : String(e));

/** `482913` → `482 913` */
export const spacedCode = (code: string): string => (code.length === 6 ? `${code.slice(0, 3)} ${code.slice(3)}` : code);
