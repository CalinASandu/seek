import { useLayoutEffect, useState, type RefObject } from "react";
import { CircleAlert, Search, X } from "lucide-react";

interface Props {
  value: string;
  onChange: (v: string) => void;
  error: { message: string; position: number } | null;
  inputRef: RefObject<HTMLInputElement | null>;
  onArrowDown: () => void;
}

/** Pixel offset of the caret under the input for a character position. */
function measure(input: HTMLInputElement, text: string): number {
  const ctx = document.createElement("canvas").getContext?.("2d");
  if (!ctx) return text.length * 7.5;
  const cs = getComputedStyle(input);
  ctx.font = `${cs.fontWeight} ${cs.fontSize} ${cs.fontFamily}`;
  return ctx.measureText(text).width;
}

export function SearchBar({ value, onChange, error, inputRef, onArrowDown }: Props) {
  const [caretX, setCaretX] = useState(0);

  useLayoutEffect(() => {
    const input = inputRef.current;
    if (error && input) setCaretX(measure(input, value.slice(0, error.position)));
  }, [error, value, inputRef]);

  return (
    <div className="searchbar">
      <div className={`search-field${error ? " is-invalid" : ""}`}>
        <Search size={18} className="search-icon" aria-hidden="true" />
        <input
          ref={inputRef}
          type="text"
          role="searchbox"
          aria-label="Search files"
          aria-invalid={error ? true : undefined}
          aria-describedby={error ? "search-error" : undefined}
          placeholder="Search files and folders"
          spellCheck={false}
          autoComplete="off"
          autoFocus
          value={value}
          onChange={(e) => onChange(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Escape" && value) {
              onChange("");
              e.stopPropagation();
            } else if (e.key === "ArrowDown") {
              e.preventDefault();
              onArrowDown();
            }
          }}
        />
        {value ? (
          <button className="icon-btn" aria-label="Clear search" onClick={() => { onChange(""); inputRef.current?.focus(); }}>
            <X size={16} />
          </button>
        ) : (
          <kbd className="kbd-hint" aria-hidden="true">Ctrl F</kbd>
        )}
      </div>
      {error && (
        <div id="search-error" className="search-error" role="alert">
          <span className="search-caret" style={{ left: 42 + caretX }} aria-hidden="true" />
          <CircleAlert size={14} aria-hidden="true" />
          <span>{error.message}</span>
        </div>
      )}
    </div>
  );
}
