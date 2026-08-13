import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { App } from "./App";
import { api } from "./state/api";
import { useSessions } from "./state/sessionStore";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const originalConnect = useSessions.getState().connect;

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

describe("App status placement", () => {
  let container: HTMLDivElement;
  let root: Root | null;

  beforeEach(() => {
    vi.stubGlobal("localStorage", makeStorage());
    vi.spyOn(api, "listBackends").mockResolvedValue([]);
    vi.spyOn(api, "listSshHosts").mockResolvedValue([]);
    useSessions.setState({
      sessions: {},
      order: [],
      activeId: null,
      connected: true,
      authError: null,
      error: "action failed",
      connect: vi.fn(() => () => {}),
    });
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(() => {
    if (root) act(() => root?.unmount());
    root = null;
    container.remove();
    useSessions.setState({ connect: originalConnect, error: null });
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("keeps action errors visible when the desktop sidebar is collapsed", async () => {
    await act(async () => root?.render(<App />));

    await act(async () => {
      container
        .querySelector<HTMLButtonElement>('[aria-label="Hide session list"]')
        ?.click();
    });

    const banners = container.querySelector(".status-banners-floating");
    expect(banners?.textContent).toContain("action failed");
  });
});
