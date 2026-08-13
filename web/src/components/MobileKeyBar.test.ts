import { describe, expect, it } from "vitest";
import { shouldSendClick } from "./MobileKeyBar";

describe("mobile key activation", () => {
  it("accepts keyboard clicks without duplicating pointer input", () => {
    expect(shouldSendClick(0)).toBe(true);
    expect(shouldSendClick(1)).toBe(false);
  });
});
