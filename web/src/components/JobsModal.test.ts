import { describe, expect, it } from "vitest";
import { localDateTimeValue, toIntervalSeconds } from "./JobsModal";

describe("scheduled job form helpers", () => {
  it("converts fixed intervals without floating point units", () => {
    expect(toIntervalSeconds(5, "minutes")).toBe(300);
    expect(toIntervalSeconds(2, "hours")).toBe(7200);
    expect(toIntervalSeconds(3, "days")).toBe(259200);
  });

  it("formats a local datetime input without a timezone suffix", () => {
    const value = localDateTimeValue(new Date(2026, 0, 2, 3, 4));
    expect(value).toBe("2026-01-02T03:04");
    expect(value).not.toContain("Z");
  });
});
