import { describe, expect, it } from "vitest";
import { isDaemonPath, staticRequestStrategy } from "./serviceWorkerPolicy";

describe("service worker request policy", () => {
  it("never intercepts daemon HTTP or WebSocket paths", () => {
    for (const path of ["/api", "/api/sessions", "/ws", "/ws/events"]) {
      expect(isDaemonPath(path)).toBe(true);
      expect(staticRequestStrategy(path, false)).toBe("network-only");
      expect(staticRequestStrategy(path, true)).toBe("network-only");
    }
    expect(isDaemonPath("/apiary")).toBe(false);
    expect(isDaemonPath("/ws-assets/icon.svg")).toBe(false);
  });

  it("uses cache-first only for fingerprinted assets", () => {
    expect(staticRequestStrategy("/assets/index-a1b2.js", false)).toBe(
      "cache-first",
    );
    expect(staticRequestStrategy("/manifest.webmanifest", false)).toBe(
      "network-first",
    );
    expect(staticRequestStrategy("/sessions/one", true)).toBe("network-first");
    expect(staticRequestStrategy("/unknown.txt", false)).toBe("network-only");
  });
});
