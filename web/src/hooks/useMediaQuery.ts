import { useSyncExternalStore } from "react";

export const MOBILE_BP = 768;

/// matchMedia-driven media query hook. Re-renders when the query's match
/// state flips. Returns `false` when matchMedia is unavailable so SSR or
/// Node-only test environments don't crash.
export function useMediaQuery(query: string): boolean {
  return useSyncExternalStore(
    (callback) => {
      if (typeof window === "undefined" || !window.matchMedia) {
        return () => {};
      }
      const mq = window.matchMedia(query);
      mq.addEventListener("change", callback);
      return () => mq.removeEventListener("change", callback);
    },
    () => {
      if (typeof window === "undefined" || !window.matchMedia) return false;
      return window.matchMedia(query).matches;
    },
    () => false,
  );
}

export function useIsMobile(): boolean {
  return useMediaQuery(`(max-width: ${MOBILE_BP}px)`);
}
