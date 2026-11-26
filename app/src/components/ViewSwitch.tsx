import { Segmented } from "./Segmented";

export type View = "search" | "space" | "clean";

const VIEWS: { id: View; label: string; hint: string }[] = [
  { id: "search", label: "Search", hint: "Find files by name (Ctrl+Shift+S)" },
  { id: "space", label: "Space", hint: "See what fills each drive (Ctrl+Shift+S)" },
  { id: "clean", label: "Decimator", hint: "Cleans up files (Ctrl+Shift+D)" },
];

/** The title bar's switch between the two views. */
export function ViewSwitch({ view, onChange }: { view: View; onChange: (v: View) => void }) {
  return <Segmented items={VIEWS} value={view} onChange={onChange} label="View" kind="radio" className="view-switch" />;
}
