import { SearchX, Search } from "lucide-react";
import { formatCount } from "../lib/format";

const EXAMPLES: { q: string; hint: string }[] = [
  { q: "report ext:pdf", hint: "Name contains “report”, PDF files only" },
  { q: "*.png dm:today", hint: "Wildcards, modified today" },
  { q: '"annual budget"', hint: "Exact phrase" },
  { q: "invoice offline:", hint: "Only on drives that aren't connected" },
  { q: "folder: backup", hint: "Folders only" },
];

interface StartProps {
  totalEntries: number;
  devices: number;
  onPick: (q: string) => void;
}

export function EmptyStart({ totalEntries, devices, onPick }: StartProps) {
  return (
    <div className="empty">
      <div className="empty-glyph" aria-hidden="true">
        <Search size={26} />
      </div>
      <h1 className="empty-title">
        Search across {formatCount(totalEntries)} files on {devices} {devices === 1 ? "device" : "devices"}
      </h1>
      <p className="empty-sub">Start typing. Results update as you type, including drives that are unplugged.</p>
      <ul className="examples" aria-label="Search examples">
        {EXAMPLES.map((e) => (
          <li key={e.q}>
            <button className="example" onClick={() => onPick(e.q)}>
              <code>{e.q}</code>
              <span>{e.hint}</span>
            </button>
          </li>
        ))}
      </ul>
    </div>
  );
}

export function EmptyNone({ query }: { query: string }) {
  return (
    <div className="empty">
      <div className="empty-glyph" aria-hidden="true">
        <SearchX size={26} />
      </div>
      <h1 className="empty-title">No files match “{query.trim()}”</h1>
      <p className="empty-sub">Check the spelling, or remove a filter to widen the search.</p>
    </div>
  );
}
