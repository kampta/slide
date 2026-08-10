import { describe, expect, it } from "vitest";
import { clipboardAction } from "./terminalInteractions";

function key(
  value: string,
  overrides: Partial<KeyboardEvent> = {},
): Pick<KeyboardEvent, "type" | "key" | "metaKey" | "ctrlKey" | "shiftKey"> {
  return {
    type: "keydown",
    key: value,
    metaKey: false,
    ctrlKey: false,
    shiftKey: false,
    ...overrides,
  };
}

describe("clipboardAction", () => {
  it("leaves paste to the browser and xterm native paste event", () => {
    expect(clipboardAction(key("v", { metaKey: true }), true, false)).toBe(
      "native-paste",
    );
    expect(
      clipboardAction(
        key("V", { ctrlKey: true, shiftKey: true }),
        false,
        false,
      ),
    ).toBe("native-paste");
  });

  it("copies only when the terminal has a selection", () => {
    const event = key("c", { metaKey: true });
    expect(clipboardAction(event, true, true)).toBe("copy");
    expect(clipboardAction(event, true, false)).toBeNull();
  });

  it("ignores unrelated modifiers and non-keydown events", () => {
    expect(clipboardAction(key("v", { ctrlKey: true }), false, false)).toBeNull();
    expect(
      clipboardAction({ ...key("v", { metaKey: true }), type: "keyup" }, true, false),
    ).toBeNull();
  });
});
