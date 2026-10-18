import { useState, type FormEvent } from "react";
import { Server } from "lucide-react";
import { useApi } from "../api/context";
import { Dialog } from "./Dialog";

export const UNC_PATTERN = /^\\\\[^\\]+\\[^\\]+/;

export function AddShareDialog({ onClose, onAdded }: { onClose: () => void; onAdded: (name: string) => void }) {
  const api = useApi();
  const [value, setValue] = useState("");
  const [touched, setTouched] = useState(false);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);
  const valid = UNC_PATTERN.test(value.trim());
  const showError = touched && !valid;

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setTouched(true);
    if (!valid || busy) return;
    setBusy(true);
    setFailure(null);
    try {
      const d = await api.addNetworkShare(value.trim());
      onAdded(d.displayName);
      onClose();
    } catch (err) {
      setFailure(String(err));
      setBusy(false);
    }
  };

  return (
    <Dialog
      title="Add network share"
      onClose={onClose}
      footer={
        <>
          <button type="button" className="btn" onClick={onClose}>
            Cancel
          </button>
          <button type="submit" form="add-share-form" className="btn btn-primary" disabled={busy}>
            {busy ? "Adding…" : "Add share"}
          </button>
        </>
      }
    >
      <form id="add-share-form" onSubmit={submit} noValidate>
        <p className="dialog-lead">
          Seek indexes the share and keeps its files searchable, even when the network is unreachable.
        </p>
        <label className="field-label" htmlFor="unc-input">
          Network path
        </label>
        <div className={`field${showError ? " is-invalid" : ""}`}>
          <Server size={16} aria-hidden="true" />
          <input
            id="unc-input"
            data-autofocus
            value={value}
            placeholder={"\\\\server\\share"}
            spellCheck={false}
            autoComplete="off"
            aria-invalid={showError}
            aria-describedby="unc-help"
            onChange={(e) => setValue(e.target.value)}
            onBlur={() => setTouched(true)}
          />
        </div>
        <div
          id="unc-help"
          className={`field-help${showError || failure ? " is-error" : ""}`}
          role={showError ? "alert" : undefined}
        >
          {failure ?? (showError ? "Enter a path like \\\\server\\share." : "Example: \\\\nas\\media")}
        </div>
      </form>
    </Dialog>
  );
}
