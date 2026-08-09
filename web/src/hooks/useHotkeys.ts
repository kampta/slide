import { useEffect, useRef } from "react";

type Handler = (e: KeyboardEvent) => void;
type Bindings = Record<string, Handler>;

/** Key format: "alt+j", "alt+shift+w", "ctrl+k". Case-insensitive. */
export function useHotkeys(bindings: Bindings) {
  // Callers typically pass a fresh object literal each render. Reading the
  // latest map through a ref lets us attach the listener exactly once instead
  // of tearing it down and rebuilding it on every render.
  const ref = useRef(bindings);
  ref.current = bindings;

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const key = e.key.toLowerCase();
      const parts: string[] = [];
      if (e.ctrlKey) parts.push("ctrl");
      if (e.altKey) parts.push("alt");
      if (e.metaKey) parts.push("meta");
      if (e.shiftKey) parts.push("shift");
      parts.push(key);
      const combo = parts.join("+");
      const handler = ref.current[combo];
      if (handler) {
        e.preventDefault();
        e.stopPropagation();
        handler(e);
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, []);
}
