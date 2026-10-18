import { useCallback, useEffect, useRef, useState } from "react";
import { CircleCheck, Monitor } from "lucide-react";
import { useApi } from "../api/context";
import type { PairingCode, PcInfo, Peers } from "../api/types";
import { errText, spacedCode } from "../lib/pcs";
import { Dialog } from "./Dialog";

interface Props {
  peers: Peers | null;
  onClose: () => void;
  onLinked: (name: string) => void;
}

type Tab = "show" | "enter";

export function LinkPcDialog({ peers, onClose, onLinked }: Props) {
  const [tab, setTab] = useState<Tab>("show");
  const [linked, setLinked] = useState<string | null>(null);
  const known = useRef(new Set((peers?.peers ?? []).map((p) => p.hostId)));

  // The device that typed our code shows up in the peer list.
  useEffect(() => {
    if (linked || tab !== "show") return;
    const fresh = peers?.peers.find((p) => !known.current.has(p.hostId));
    if (fresh) {
      setLinked(fresh.name);
      onLinked(fresh.name);
    }
  }, [peers, linked, tab, onLinked]);

  const done = (name: string) => {
    setLinked(name);
    onLinked(name);
  };

  return (
    <Dialog
      title="Link a device"
      onClose={onClose}
      width={460}
      footer={
        <button className={`btn${linked ? " btn-primary" : ""}`} onClick={onClose} data-autofocus={linked ? true : undefined}>
          {linked ? "Done" : "Cancel"}
        </button>
      }
    >
      {linked ? (
        <div className="link-done" role="status">
          <CircleCheck size={28} aria-hidden="true" />
          <div>
            <strong>Linked with {linked}</strong>
            <p className="dialog-lead">Its files will appear in search here in a moment, and this device's files there.</p>
          </div>
        </div>
      ) : (
        <>
          <div className="segmented link-tabs" role="tablist" aria-label="How to link">
            <button role="tab" aria-selected={tab === "show"} className={tab === "show" ? "is-on" : ""} onClick={() => setTab("show")}>
              Show a code
            </button>
            <button role="tab" aria-selected={tab === "enter"} className={tab === "enter" ? "is-on" : ""} onClick={() => setTab("enter")}>
              Enter a code
            </button>
          </div>
          {tab === "show" ? <ShowCode /> : <EnterCode onLinked={done} />}
        </>
      )}
    </Dialog>
  );
}

function ShowCode() {
  const api = useApi();
  const [code, setCode] = useState<PairingCode | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [now, setNow] = useState(Date.now());

  const fresh = useCallback(() => {
    setError(null);
    api.startPairing().then(setCode, (e: unknown) => setError(errText(e)));
  }, [api]);

  useEffect(fresh, [fresh]);
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, []);

  const left = code ? Math.max(0, Math.ceil((code.expiresMs - now) / 1000)) : 0;
  const expired = !!code && left === 0;

  return (
    <div role="tabpanel" aria-label="Show a code" className="link-panel">
      <p className="dialog-lead">On the other device, open Seek → Link a device → Enter a code, and type:</p>
      {error ? (
        <p className="field-help is-error" role="alert">
          {error}
        </p>
      ) : (
        <div className={`pair-code${expired ? " is-expired" : ""}`} aria-label={code ? `Code ${code.code.split("").join(" ")}` : "Getting a code"}>
          {code ? spacedCode(code.code) : "··· ···"}
        </div>
      )}
      <div className="pair-meta">
        {expired ? (
          <>
            <span>The code expired.</span>
            <button className="btn" onClick={fresh}>
              New code
            </button>
          </>
        ) : code ? (
          <span className="tnum">
            Valid for {Math.floor(left / 60)}:{String(left % 60).padStart(2, "0")} · works once
          </span>
        ) : null}
      </div>
    </div>
  );
}

function EnterCode({ onLinked }: { onLinked: (name: string) => void }) {
  const api = useApi();
  const [pcs, setPcs] = useState<PcInfo[]>([]);
  const [addr, setAddr] = useState("");
  const [code, setCode] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    const load = () =>
      api.discoveredPcs().then(
        (list) => live && setPcs(list),
        () => {},
      );
    void load();
    const t = setInterval(load, 2000);
    return () => {
      live = false;
      clearInterval(t);
    };
  }, [api]);

  const digits = code.replace(/\D/g, "");
  const ready = addr.trim() !== "" && digits.length === 6 && !busy;

  const link = async () => {
    if (!ready) return;
    setBusy(true);
    setError(null);
    try {
      await api.joinPairing(addr.trim(), digits);
      onLinked(pcs.find((p) => p.addr === addr.trim())?.name ?? addr.trim());
    } catch (e) {
      setError(errText(e));
      setBusy(false);
    }
  };

  return (
    <form
      role="tabpanel"
      aria-label="Enter a code"
      className="link-panel"
      onSubmit={(e) => {
        e.preventDefault();
        void link();
      }}
    >
      <div className="field-label" id="pc-pick">
        Device showing the code
      </div>
      {pcs.length > 0 ? (
        <ul className="pc-pick" aria-labelledby="pc-pick">
          {pcs.map((pc) => (
            <li key={pc.hostId}>
              <button
                type="button"
                aria-pressed={addr === pc.addr}
                className={addr === pc.addr ? "is-on" : ""}
                onClick={() => setAddr(pc.addr)}
              >
                <Monitor size={16} aria-hidden="true" />
                <span className="pc-pick-name">{pc.name}</span>
                <span className="pc-pick-addr">{pc.addr}</span>
              </button>
            </li>
          ))}
        </ul>
      ) : (
        <p className="setting-desc">Looking for devices running Seek on this network… You can also type the address below.</p>
      )}
      <div className="field">
        <input
          value={addr}
          placeholder="192.168.1.20:47321"
          aria-label="Address of the other device"
          spellCheck={false}
          onChange={(e) => setAddr(e.target.value)}
        />
      </div>

      <label className="field-label link-code-label" htmlFor="pair-input">
        6-digit code
      </label>
      <div className={`field field-code${error ? " is-invalid" : ""}`}>
        <input
          id="pair-input"
          value={code}
          inputMode="numeric"
          autoComplete="one-time-code"
          maxLength={7}
          placeholder="000 000"
          spellCheck={false}
          onChange={(e) => setCode(e.target.value)}
        />
      </div>
      <div className="link-actions">
        <p className={`field-help${error ? " is-error" : ""}`} role={error ? "alert" : undefined}>
          {error ?? "The code is shown on the other device for 2 minutes."}
        </p>
        <button type="submit" className="btn btn-primary" disabled={!ready}>
          {busy ? "Linking…" : "Link"}
        </button>
      </div>
    </form>
  );
}
