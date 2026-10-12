import { useState } from "react";
import { ShieldCheck, TriangleAlert } from "lucide-react";
import { useApi } from "../api/context";
import type { ServiceState } from "../api/types";
import { AppMark } from "./TitleBar";

export function ServiceSetup({ state }: { state: Exclude<ServiceState, "connected"> }) {
  const api = useApi();
  const [busy, setBusy] = useState(false);

  if (state === "connecting") {
    return (
      <div className="setup" role="status">
        <div className="spinner" aria-hidden="true" />
        <p className="setup-connecting">Connecting to the Seek service…</p>
      </div>
    );
  }

  const failed = state === "error";
  const install = async () => {
    setBusy(true);
    try {
      await api.installService();
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="setup">
      <section className="setup-card" aria-labelledby="setup-title">
        <div className={`setup-icon${failed ? " is-warn" : ""}`} aria-hidden="true">
          {failed ? <TriangleAlert size={26} /> : <AppMark size={30} />}
        </div>
        <h1 id="setup-title">{failed ? "The Seek service isn't responding" : "Set up Seek"}</h1>
        <p>
          {failed
            ? "Seek can't reach its background service. Reinstalling restarts it and keeps your saved drives."
            : "Seek keeps a live index of your drives through a small background service, so searches stay instant."}
        </p>
        <div className="setup-note">
          <ShieldCheck size={16} aria-hidden="true" />
          <span>
            Installing needs administrator permission once. Windows will show a single prompt, and Seek never asks again.
          </span>
        </div>
        <button className="btn btn-primary btn-lg" onClick={() => void install()} disabled={busy}>
          {busy ? "Waiting for Windows…" : failed ? "Reinstall Seek service" : "Install Seek service"}
        </button>
      </section>
    </div>
  );
}
