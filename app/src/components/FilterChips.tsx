import { File, Folder, X } from "lucide-react";
import type { Filter } from "../lib/query";

interface Props {
  filters: Filter[];
  isOn: (token: string) => boolean;
  onToggle: (filter: Filter, exclusive?: string[]) => void;
}

export function FilterChips({ filters, isOn, onToggle }: Props) {
  const chips = [
    { token: "file:", label: "Files", icon: <File size={14} />, exclusive: ["folder:"] },
    { token: "folder:", label: "Folders", icon: <Folder size={14} />, exclusive: ["file:"] },
    { token: "online:", label: "Online", icon: <span className="dot dot-online" />, exclusive: ["offline:"] },
    { token: "offline:", label: "Offline", icon: <span className="dot dot-offline" />, exclusive: ["online:"] },
  ];
  const pills = filters.filter((f) => f.kind !== "flag");
  return (
    <div className="chips" role="group" aria-label="Filters">
      {chips.map((c) => {
        const on = isOn(c.token);
        return (
          <button
            key={c.token}
            className={`chip${on ? " is-on" : ""}`}
            aria-pressed={on}
            onClick={() => onToggle({ token: c.token, kind: "flag", label: c.label }, c.exclusive)}
          >
            {c.icon}
            {c.label}
          </button>
        );
      })}
      {pills.length > 0 && <span className="chips-sep" aria-hidden="true" />}
      {pills.map((f) => (
        <button
          key={f.token}
          className="chip is-on filter-pill"
          aria-label={`Remove filter ${f.label}`}
          title={f.title}
          onClick={() => onToggle(f)}
        >
          <span>{f.label}</span>
          <X size={13} aria-hidden="true" />
        </button>
      ))}
    </div>
  );
}
