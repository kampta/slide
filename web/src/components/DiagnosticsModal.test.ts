import { describe, expect, it } from "vitest";
import {
  clampPercent,
  formatResetTime,
  formatWindowDuration,
  runtimeLabel,
  runtimeTone,
} from "./DiagnosticsModal";

describe("DiagnosticsModal helpers", () => {
  it("maps runtime status to stable visual tones", () => {
    expect(runtimeTone("ready")).toBe("ok");
    expect(runtimeTone("missing")).toBe("warn");
    expect(runtimeTone("unauthenticated")).toBe("warn");
    expect(runtimeTone("broken")).toBe("danger");
  });

  it("uses actionable human labels", () => {
    expect(runtimeLabel("ready")).toBe("Ready");
    expect(runtimeLabel("missing")).toBe("Missing");
    expect(runtimeLabel("unauthenticated")).toBe("Sign-in required");
    expect(runtimeLabel("broken")).toBe("Probe failed");
  });

  it("clamps malformed usage percentages", () => {
    expect(clampPercent(-1)).toBe(0);
    expect(clampPercent(42.5)).toBe(42.5);
    expect(clampPercent(101)).toBe(100);
    expect(clampPercent(Number.NaN)).toBe(0);
  });

  it("formats usage windows without losing partial units", () => {
    expect(formatWindowDuration(5)).toBe("5m");
    expect(formatWindowDuration(90)).toBe("1h 30m");
    expect(formatWindowDuration(1_500)).toBe("1d 1h");
    expect(formatWindowDuration(10_080)).toBe("7d");
    expect(formatWindowDuration(null)).toBeNull();
    expect(formatWindowDuration(0)).toBeNull();
  });

  it("provides machine and human-readable absolute reset times", () => {
    const timestamp = Date.UTC(2026, 7, 13, 19, 45, 30);
    const reset = formatResetTime(timestamp);
    expect(reset?.iso).toBe("2026-08-13T19:45:30.000Z");
    expect(reset?.short).toBeTruthy();
    expect(reset?.full).toContain("2026");
    expect(formatResetTime(null)).toBeNull();
    expect(formatResetTime(Number.NaN)).toBeNull();
  });
});
