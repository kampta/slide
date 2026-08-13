import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { useSessions } from "../state/sessionStore";
import { StatusBanners } from "./StatusBanners";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

describe("StatusBanners", () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    useSessions.setState({
      connected: true,
      authError: null,
      error: null,
    });
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
  });

  it("renders disconnected and action errors in the floating app shell", () => {
    useSessions.setState({ connected: false, error: "stop failed" });
    act(() => root.render(<StatusBanners floating />));

    expect(container.textContent).toContain("Disconnected — retrying…");
    expect(container.textContent).toContain("stop failed");
    expect(container.querySelector(".status-banners-floating")).not.toBeNull();

    act(() => {
      container.querySelector<HTMLButtonElement>('[aria-label="Dismiss error"]')?.click();
    });
    expect(useSessions.getState().error).toBeNull();
    expect(container.textContent).not.toContain("stop failed");
  });

  it("keeps stale-token guidance visible until the token is replaced", () => {
    useSessions.setState({ authError: "token rotated" });
    act(() => root.render(<StatusBanners floating />));

    expect(container.textContent).toContain("token rotated");
    expect(container.textContent).not.toContain("retrying");
    expect(container.querySelector('[aria-label="Dismiss error"]')).toBeNull();
  });
});
