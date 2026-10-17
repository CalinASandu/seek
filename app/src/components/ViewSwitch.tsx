import { useRef, type KeyboardEvent } from "react";

export type View = "search" | "space";

const VIEWS: { id: View; label: string; hint: string }[] = [
  { id: "search", label: "Search", hint: "Find files by name (Ctrl+Shift+S)" },
  { id: "space", label: "Space", hint: "See what fills each drive (Ctrl+Shift+S)" },
];

/** Segmented control in the title bar (a macOS toolbar's view switcher). */
export function ViewSwitch({ view, onChange }: { view: View; onChange: (v: View) => void }) {
  const refs = useRef<(HTMLButtonElement | null)[]>([]);
  const onKey = (e: KeyboardEvent, i: number) => {
    if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
    e.preventDefault();
    const next = (i + (e.key === "ArrowRight" ? 1 : VIEWS.length - 1)) % VIEWS.length;
    onChange(VIEWS[next]!.id);
    refs.current[next]?.focus();
  };
  return (
    <div className="segctl view-switch" role="radiogroup" aria-label="View" data-index={VIEWS.findIndex((v) => v.id === view)}>
      <span className="segctl-thumb" aria-hidden="true" />
      {VIEWS.map((v, i) => (
        <button
          key={v.id}
          ref={(el) => {
            refs.current[i] = el;
          }}
          role="radio"
          aria-checked={view === v.id}
          tabIndex={view === v.id ? 0 : -1}
          title={v.hint}
          className="segment"
          onClick={() => onChange(v.id)}
          onKeyDown={(e) => onKey(e, i)}
        >
          {v.label}
        </button>
      ))}
    </div>
  );
}
