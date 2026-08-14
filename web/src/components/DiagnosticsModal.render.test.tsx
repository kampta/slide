import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { api, type RuntimeDiagnosticsSnapshot } from "../state/api";
import { DiagnosticsModal } from "./DiagnosticsModal";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const resetAt = Date.UTC(2026, 7, 13, 19, 45, 30);
const snapshot: RuntimeDiagnosticsSnapshot = {
  target: "local",
  checked_at: resetAt,
  tmux: {
    available: true,
    required: true,
    version: "3.5",
    message: "Ready",
    action: null,
  },
  backends: [
    {
      backend: "codex",
      label: "Codex",
      status: "ready",
      available: true,
      installed: true,
      authenticated: true,
      version: "1.2.3",
      message: "Ready",
      action: null,
      last_error: null,
      rate_limits: [
        {
          label: "Weekly",
          used_percent: 90,
          window_minutes: 10_080,
          resets_at: resetAt,
        },
        {
          label: "Burst",
          used_percent: -12,
          window_minutes: null,
          resets_at: null,
        },
        {
          label: "Five-hour",
          used_percent: 75,
          window_minutes: 300,
          resets_at: null,
        },
      ],
    },
    {
      backend: "grok",
      label: "Grok",
      status: "ready",
      available: true,
      installed: true,
      authenticated: true,
      version: null,
      message: "Ready",
      action: null,
      last_error: null,
    },
  ],
};

describe("DiagnosticsModal usage limits", () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    vi.spyOn(api, "listSshHosts").mockResolvedValue([]);
    vi.spyOn(api, "getRuntimeDiagnostics").mockResolvedValue(snapshot);
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
    vi.restoreAllMocks();
  });

  it("renders clamped meters and absolute reset metadata inside provider cards", async () => {
    await act(async () => {
      root.render(<DiagnosticsModal open onClose={() => {}} />);
    });

    const cards = Array.from(container.querySelectorAll<HTMLElement>("article"));
    const codex = cards.find((card) => card.textContent?.includes("Codex"));
    const grok = cards.find((card) => card.textContent?.includes("Grok"));
    const meters = codex?.querySelectorAll<HTMLElement>('[role="progressbar"]');

    expect(codex?.querySelector('.diagnostic-rate-limits[aria-label="Codex usage limits"]')).not.toBeNull();
    expect(meters).toHaveLength(3);
    expect(meters?.[0].getAttribute("aria-valuenow")).toBe("90");
    expect(meters?.[0].getAttribute("aria-label")).toBe("Codex Weekly: 90% used");
    expect(meters?.[0].classList.contains("diagnostic-rate-limit-meter-danger")).toBe(true);
    expect(meters?.[0].querySelector<HTMLElement>("span")?.style.width).toBe("90%");
    expect(meters?.[1].getAttribute("aria-valuenow")).toBe("0");
    expect(meters?.[2].getAttribute("aria-valuenow")).toBe("75");
    expect(meters?.[2].classList.contains("diagnostic-rate-limit-meter-warn")).toBe(true);
    expect(codex?.textContent).toContain("90% used");
    expect(codex?.textContent).toContain("0% used");
    expect(codex?.textContent).toContain("7d window");
    expect(codex?.textContent).toContain("Window unavailable");
    expect(codex?.textContent).toContain("Reset unavailable");

    const reset = codex?.querySelector<HTMLTimeElement>("time");
    expect(reset?.dateTime).toBe("2026-08-13T19:45:30.000Z");
    expect(reset?.title).toContain("2026");
    expect(reset?.textContent).toMatch(/^Resets .+/);
    expect(grok?.querySelector(".diagnostic-rate-limits")).toBeNull();
  });
});
