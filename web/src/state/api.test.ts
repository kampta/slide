import { beforeEach, describe, expect, it, vi } from "vitest";

const KEY = "slide.bootstrapToken";

function makeStorage(): Storage {
  const values = new Map<string, string>();
  return {
    get length() {
      return values.size;
    },
    clear: () => values.clear(),
    getItem: (key) => values.get(key) ?? null,
    key: (index) => Array.from(values.keys())[index] ?? null,
    removeItem: (key) => {
      values.delete(key);
    },
    setItem: (key, value) => {
      values.set(key, String(value));
    },
  };
}

vi.stubGlobal("localStorage", makeStorage());
vi.stubGlobal("sessionStorage", makeStorage());

const { api, getToken, prepareAuth, setToken, STALE_TOKEN_MESSAGE } =
  await import("./api");

describe("authentication", () => {
  beforeEach(() => {
    localStorage.clear();
    sessionStorage.clear();
    window.history.replaceState({}, "", "/");
    vi.restoreAllMocks();
  });

  it("keeps the local process token in sessionStorage only", () => {
    setToken("local");
    expect(getToken()).toBe("local");
    expect(sessionStorage.getItem(KEY)).toBe("local");
    expect(localStorage.length).toBe(0);
  });

  it("exchanges a pairing fragment without storing the device credential", async () => {
    window.history.replaceState({}, "", "/#pair=single-use");
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(new Response(null, { status: 204 })),
    );

    await prepareAuth();

    expect(window.location.hash).toBe("");
    expect(getToken()).toBe("");
    expect(localStorage.length).toBe(0);
    expect(fetch).toHaveBeenCalledWith(
      "/api/auth/pair",
      expect.objectContaining({
        method: "POST",
        credentials: "same-origin",
        body: JSON.stringify({ secret: "single-use" }),
      }),
    );
  });

  it("clears a rejected local token", async () => {
    setToken("expired");
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response("", { status: 401 })));

    await expect(api.getLog("session/id")).rejects.toThrow(STALE_TOKEN_MESSAGE);
    expect(getToken()).toBe("");
  });
});
