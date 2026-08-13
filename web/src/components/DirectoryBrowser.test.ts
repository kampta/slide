import { describe, expect, it } from "vitest";
import { isLatestRequest } from "./DirectoryBrowser";
import { parentPath } from "../state/path";

describe("parentPath", () => {
  it("walks Unix paths without escaping root", () => {
    expect(parentPath("/one/two/")).toBe("/one");
    expect(parentPath("/one")).toBe("/");
    expect(parentPath("/")).toBeNull();
    expect(parentPath("")).toBeNull();
  });
});

describe("directory request ordering", () => {
  it("accepts only the newest navigation response", () => {
    expect(isLatestRequest(2, 2)).toBe(true);
    expect(isLatestRequest(1, 2)).toBe(false);
  });
});
