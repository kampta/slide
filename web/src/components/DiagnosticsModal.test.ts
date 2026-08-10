import { describe, expect, it } from "vitest";
import { runtimeLabel, runtimeTone } from "./DiagnosticsModal";

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
});
