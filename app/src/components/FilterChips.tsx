import { File, Folder } from "lucide-react";
import { hasToken } from "../lib/query";

interface Props {
  query: string;
  onToggle: (token: string, exclusive?: string[]) => void;
}

export function FilterChips({ query, onToggle }: Props) {
  const chips = [
    { token: "file:", label: "Files", icon: <File size={14} />, exclusive: ["folder:"] },
    { token: "folder:", label: "Folders", icon: <Folder size={14} />, exclusive: ["file:"] },
    { token: "online:", label: "Online", icon: <span className="dot dot-online" />, exclusive: ["offline:"] },
    { token: "offline:", label: "Offline", icon: <span className="dot dot-offline" />, exclusive: ["online:"] },
  ];
  return (
    <div className="chips" role="group" aria-label="Filters">
      {chips.map((c) => {
        const on = hasToken(query, c.token);
        return (
          <button
            key={c.token}
            className={`chip${on ? " is-on" : ""}`}
            aria-pressed={on}
            onClick={() => onToggle(c.token, c.exclusive)}
          >
            {c.icon}
            {c.label}
          </button>
        );
      })}
    </div>
  );
}
