import { describe, expect, it, vi } from "vitest";
import {
  attachTouchNavigation,
  attachVisibleReconnect,
  clipboardAction,
  queueTerminalReset,
  tmuxWheelInput,
} from "./terminalInteractions";
import type { Terminal } from "@xterm/xterm";

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

describe("terminal stream helpers", () => {
  it("orders a hard reset behind pending terminal input", async () => {
    const canvas = vi
      .spyOn(HTMLCanvasElement.prototype, "getContext")
      .mockReturnValue(null);
    const { Terminal } = await import("@xterm/xterm");
    const term = new Terminal({ cols: 20, rows: 4 });
    await new Promise<void>((resolve) => {
      term.write("stale");
      queueTerminalReset(term);
      term.write("fresh", resolve);
    });

    const contents = Array.from(
      { length: term.buffer.active.length },
      (_, row) => term.buffer.active.getLine(row)?.translateToString(true) ?? "",
    ).join("\n");
    expect(contents).toContain("fresh");
    expect(contents).not.toContain("stale");
    term.dispose();
    canvas.mockRestore();
  });

  it("encodes tmux wheel events with one-based SGR coordinates", () => {
    expect(tmuxWheelInput(true, 7, 11)).toBe("\x1b[<64;7;11M");
    expect(tmuxWheelInput(false, 7, 11)).toBe("\x1b[<65;7;11M");
  });

  it("routes alternate-buffer swipes through the active supervisor", () => {
    const host = document.createElement("div");
    const screen = document.createElement("div");
    screen.className = "xterm-screen";
    screen.getBoundingClientRect = () =>
      ({ left: 0, top: 0, width: 100, height: 50 }) as DOMRect;
    host.appendChild(screen);
    const send = vi.fn();
    const socket = () =>
      ({ readyState: WebSocket.OPEN, send }) as unknown as WebSocket;
    const term = {
      cols: 10,
      rows: 5,
      buffer: { active: { type: "alternate", viewportY: 0 } },
      focus: vi.fn(),
    } as unknown as Terminal;
    const touch = (type: string, x: number, y: number) => {
      const event = new Event(type, { bubbles: true, cancelable: true });
      Object.defineProperty(event, "touches", {
        value: type === "touchend" ? [] : [{ clientX: x, clientY: y }],
      });
      return event;
    };

    const detachTmux = attachTouchNavigation(host, term, socket, "tmux");
    host.dispatchEvent(touch("touchstart", 25, 20));
    host.dispatchEvent(touch("touchmove", 25, 68));
    expect(new TextDecoder().decode(send.mock.calls[0][0])).toBe(
      "\x1b[<64;3;5M\x1b[<64;3;5M",
    );
    detachTmux();

    send.mockClear();
    const detachDirect = attachTouchNavigation(host, term, socket, "direct");
    host.dispatchEvent(touch("touchstart", 25, 68));
    host.dispatchEvent(touch("touchmove", 25, 20));
    expect(new TextDecoder().decode(send.mock.calls[0][0])).toBe("\x1b[B\x1b[B");

    (term.buffer.active as { type: string }).type = "normal";
    send.mockClear();
    host.dispatchEvent(touch("touchstart", 25, 20));
    host.dispatchEvent(touch("touchmove", 25, 68));
    expect(send).not.toHaveBeenCalled();
    detachDirect();

    (term.buffer.active as { type: string }).type = "alternate";
    host.dispatchEvent(touch("touchstart", 25, 20));
    host.dispatchEvent(touch("touchmove", 25, 68));
    expect(send).not.toHaveBeenCalled();
  });
});

describe("attachVisibleReconnect", () => {
  it("reconnects only when the document becomes visible and detaches cleanly", () => {
    const visibility = vi
      .spyOn(document, "visibilityState", "get")
      .mockReturnValue("hidden");
    const reconnect = vi.fn();
    const detach = attachVisibleReconnect(reconnect);

    document.dispatchEvent(new Event("visibilitychange"));
    expect(reconnect).not.toHaveBeenCalled();

    visibility.mockReturnValue("visible");
    document.dispatchEvent(new Event("visibilitychange"));
    expect(reconnect).toHaveBeenCalledOnce();

    detach();
    document.dispatchEvent(new Event("visibilitychange"));
    expect(reconnect).toHaveBeenCalledOnce();
    visibility.mockRestore();
  });
});
