import { useCallback, useEffect, useState } from "react";
import { useApi } from "../api/context";
import type { Device, Peers, Status } from "../api/types";

export function useDevices() {
  const api = useApi();
  const [devices, setDevices] = useState<Device[]>([]);
  const [status, setStatus] = useState<Status | null>(null);
  const [progress, setProgress] = useState<Record<string, number>>({});
  /** `null` while unknown or when the service has sync turned off. */
  const [peers, setPeers] = useState<Peers | null>(null);

  const refreshPeers = useCallback(async () => {
    try {
      setPeers(await api.listPeers());
    } catch {
      setPeers(null);
    }
  }, [api]);

  const refresh = useCallback(async () => {
    try {
      const [d, s] = await Promise.all([api.listDevices(), api.status()]);
      setDevices(d);
      setStatus(s);
      setProgress((p) => {
        const next = { ...p };
        for (const i of s.indexing) next[i.deviceId] = i.scanned;
        return next;
      });
    } catch {
      /* service not reachable; the service-state screen handles it */
    }
  }, [api]);

  useEffect(() => {
    void refresh();
    void refreshPeers();
    return api.onEvent((e) => {
      if (e.type === "devicesChanged") void refresh();
      else if (e.type === "peersChanged") void refreshPeers();
      else if (e.type === "indexProgress") {
        setProgress((p) => ({ ...p, [e.deviceId]: e.scanned }));
        if (e.done) void refresh();
      } else if (e.type === "serviceState" && e.state === "connected") {
        void refresh();
        void refreshPeers();
      }
    });
  }, [api, refresh, refreshPeers]);

  return { devices, status, progress, peers, refresh, refreshPeers };
}
