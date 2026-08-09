import { describe, expect, it } from "vitest";
import { parentPath } from "../state/path";

describe("parentPath", () => {
  it("walks Unix paths without escaping root", () => {
    expect(parentPath("/one/two/")).toBe("/one");
    expect(parentPath("/one")).toBe("/");
    expect(parentPath("/")).toBeNull();
    expect(parentPath("")).toBeNull();
  });
});
