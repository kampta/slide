export const APP_SHELL = [
  "/",
  "/manifest.webmanifest",
  "/favicon.svg",
  "/apple-touch-icon.png",
  "/icon-192.png",
  "/icon-512.png",
  "/icon-512-maskable.png",
] as const;

export const MAX_CACHED_ASSETS = 12;

const APP_SHELL_PATHS = new Set<string>(APP_SHELL);

export function isDaemonPath(pathname: string): boolean {
  return ["/api", "/ws"].some(
    (prefix) => pathname === prefix || pathname.startsWith(`${prefix}/`),
  );
}

export function staticRequestStrategy(
  pathname: string,
  navigation: boolean,
): "network-only" | "network-first" | "cache-first" {
  // API/WS paths win even if a caller mislabels them as navigations. The
  // worker must never answer daemon traffic from the static app-shell cache.
  if (isDaemonPath(pathname)) return "network-only";
  if (navigation || APP_SHELL_PATHS.has(pathname)) return "network-first";
  if (pathname.startsWith("/assets/")) return "cache-first";
  return "network-only";
}

/** Keep the newest fingerprinted bundles and discard older release artifacts. */
export function assetPathsToPrune(
  cachedPaths: string[],
  limit = MAX_CACHED_ASSETS,
): string[] {
  const assets = cachedPaths.filter((path) => path.startsWith("/assets/"));
  return assets.slice(0, Math.max(0, assets.length - limit));
}
