import { describe, expect, it, vi } from "vitest";
import { registerServiceWorker, type ServiceWorkerRegistrar } from "./pwa";

describe("PWA registration", () => {
  it("registers the root-scoped worker", () => {
    const register = vi.fn().mockResolvedValue(undefined);

    registerServiceWorker({ register } as ServiceWorkerRegistrar);

    expect(register).toHaveBeenCalledWith("/sw.js", {
      scope: "/",
    });
  });

  it("does nothing when service workers are unavailable", () => {
    expect(() => registerServiceWorker(null)).not.toThrow();
  });

  it("does not surface registration failures as unhandled rejections", async () => {
    const register = vi.fn().mockRejectedValue(new Error("insecure context"));

    registerServiceWorker({ register } as ServiceWorkerRegistrar);
    await Promise.resolve();

    expect(register).toHaveBeenCalledOnce();
  });
});
