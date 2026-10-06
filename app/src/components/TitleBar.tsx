import { windowAction } from "../lib/window";

export function AppMark({ size = 16 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 32 32" aria-hidden="true">
      <rect width="32" height="32" rx="8" fill="var(--accent)" />
      <circle cx="14" cy="14" r="6" fill="none" stroke="#fff" strokeWidth="3" />
      <path d="M19 19l6 6" stroke="#fff" strokeWidth="3" strokeLinecap="round" />
    </svg>
  );
}

export function TitleBar() {
  return (
    <header className="titlebar" data-tauri-drag-region onDoubleClick={() => void windowAction("toggleMaximize")}>
      <div className="titlebar-brand" data-tauri-drag-region>
        <AppMark />
        <span data-tauri-drag-region>Seek</span>
      </div>
      <div className="titlebar-controls" onDoubleClick={(e) => e.stopPropagation()}>
        <button className="wc" aria-label="Minimize" onClick={() => void windowAction("minimize")}>
          <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
            <path d="M0 5h10" stroke="currentColor" strokeWidth="1" />
          </svg>
        </button>
        <button className="wc" aria-label="Maximize" onClick={() => void windowAction("toggleMaximize")}>
          <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
            <rect x="0.5" y="0.5" width="9" height="9" rx="1.5" fill="none" stroke="currentColor" strokeWidth="1" />
          </svg>
        </button>
        <button className="wc wc-close" aria-label="Close" onClick={() => void windowAction("close")}>
          <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
            <path d="M0.5 0.5l9 9M9.5 0.5l-9 9" stroke="currentColor" strokeWidth="1" />
          </svg>
        </button>
      </div>
    </header>
  );
}
