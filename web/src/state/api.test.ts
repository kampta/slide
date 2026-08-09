import { beforeEach, describe, expect, it, vi } from "vitest";

const KEY = "slide.token";

// vitest's jsdom environment in this repo doesn't provide a working
// `Storage` implementation (.removeItem is missing), so install a tiny
// Map-backed shim before importing the module under test.
function makeStorage(): Storage {
  const m = new Map<string, string>();
  return {
    get length() {
      return m.size;
    },
    clear: () => m.clear(),
    getItem: (k: string) => (m.has(k) ? m.get(k)! : null),
    key: (i: number) => Array.from(m.keys())[i] ?? null,
    removeItem: (k: string) => {
      m.delete(k);
    },
    setItem: (k: string, v: string) => {
      m.set(k, String(v));
    },
  };
}

vi.stubGlobal("localStorage", makeStorage());
vi.stubGlobal("sessionStorage", makeStorage());

const { api, getToken, setToken, STALE_TOKEN_MESSAGE } = await import("./api");

describe("token storage", () => {
  beforeEach(() => {
    localStorage.removeItem(KEY);
    sessionStorage.removeItem(KEY);
    window.history.replaceState({}, "", "/");
  });

  it("returns empty string with no token in URL or storage", () => {
    expect(getToken()).toBe("");
  });

  it("returns the localStorage token when no URL token is present", () => {
    setToken("stored");
    expect(getToken()).toBe("stored");
  });

  it("URL token wins over stored token (post-rotation re-pair)", () => {
    setToken("old");
    window.history.replaceState({}, "", "/?token=new");
    expect(getToken()).toBe("new");
    expect(localStorage.getItem(KEY)).toBe("new");
  });

  it("setToken('') clears the stored token", () => {
    setToken("foo");
    setToken("");
    expect(localStorage.getItem(KEY)).toBeNull();
    expect(getToken()).toBe("");
  });

  it("ignores URL params that don't include token=", () => {
    setToken("stored");
    window.history.replaceState({}, "", "/?other=value");
    expect(getToken()).toBe("stored");
  });

  it("clears a rejected token for log requests too", async () => {
    setToken("expired");
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response("", { status: 401 })));

    await expect(api.getLog("session/id")).rejects.toThrow(STALE_TOKEN_MESSAGE);
    expect(localStorage.getItem(KEY)).toBeNull();
    expect(fetch).toHaveBeenCalledWith(
      "/api/sessions/session%2Fid/log",
      expect.any(Object),
    );
  });
});
