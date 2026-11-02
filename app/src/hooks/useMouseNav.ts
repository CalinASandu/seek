import { useEffect, useRef } from "react";

const BACK = 3;
const FORWARD = 4;

/** The mouse's side buttons, anywhere in the window. The default is stopped so the webview never navigates itself. */
export function useMouseNav(onBack: () => void, onForward: () => void): void {
  const ref = useRef({ onBack, onForward });
  ref.current = { onBack, onForward };
  useEffect(() => {
    const swallow = (e: MouseEvent) => {
      if (e.button === BACK || e.button === FORWARD) e.preventDefault();
    };
    const onUp = (e: MouseEvent) => {
      swallow(e);
      if (e.button === BACK) ref.current.onBack();
      else if (e.button === FORWARD) ref.current.onForward();
    };
    window.addEventListener("mousedown", swallow);
    window.addEventListener("mouseup", onUp);
    return () => {
      window.removeEventListener("mousedown", swallow);
      window.removeEventListener("mouseup", onUp);
    };
  }, []);
}
