import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { TerminalView } from "./Terminal";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const mocks = vi.hoisted(() => ({
  getLog: vi.fn(),
  openSessionSocket: vi.fn(),
  sockets: [] as FakeSocket[],
  terminals: [] as Array<{ writes: Array<string | Uint8Array> }>,
}));

class FakeSocket {
  readyState: number = WebSocket.CONNECTING;
  sent: Array<string | ArrayBufferLike | Blob | ArrayBufferView> = [];
  onopen: (() => void) | null = null;
  onmessage: ((event: MessageEvent) => void) | null = null;
  onclose: ((event: CloseEvent) => void) | null = null;
  onerror: (() => void) | null = null;

  send(data: string | ArrayBufferLike | Blob | ArrayBufferView) {
    this.sent.push(data);
  }

  close() {
    if (this.readyState === WebSocket.CLOSED) return;
    this.readyState = WebSocket.CLOSED;
    this.onclose?.({ code: 1000 } as CloseEvent);
  }

  open() {
    this.readyState = WebSocket.OPEN;
    this.onopen?.();
  }

  message(data: string | ArrayBuffer) {
    this.onmessage?.({ data } as MessageEvent);
  }

  serverClose(code = 1006) {
    this.readyState = WebSocket.CLOSED;
    this.onclose?.({ code } as CloseEvent);
  }
}

vi.mock("../state/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../state/api")>();
  return {
    ...actual,
    api: { ...actual.api, getLog: mocks.getLog },
    openSessionSocket: mocks.openSessionSocket,
  };
});

vi.mock("@xterm/xterm", () => ({
  Terminal: class {
    cols = 80;
    rows = 24;
    options: Record<string, unknown> = {};
    writes: Array<string | Uint8Array> = [];
    buffer = { active: { type: "normal", viewportY: 0, length: 0 } };

    constructor(options: Record<string, unknown>) {
      this.options = options;
      mocks.terminals.push(this);
    }

    loadAddon() {}
    open(host: HTMLElement) {
      const screen = document.createElement("div");
      screen.className = "xterm-screen";
      host.appendChild(screen);
    }
    focus() {}
    dispose() {}
    hasSelection() {
      return false;
    }
    getSelection() {
      return "";
    }
    clearSelection() {}
    select() {}
    attachCustomKeyEventHandler() {}
    onData() {
      return { dispose() {} };
    }
    write(data: string | Uint8Array, callback?: () => void) {
      this.writes.push(data);
      callback?.();
    }
    writeln(data: string) {
      this.writes.push(data);
    }
  },
}));

vi.mock("@xterm/addon-fit", () => ({
  FitAddon: class {
    fit() {}
  },
}));

vi.mock("@xterm/addon-web-links", () => ({
  WebLinksAddon: class {},
}));

describe("TerminalView stream lifecycle", () => {
  let container: HTMLDivElement;
  let root: Root;
  let visibility: ReturnType<typeof vi.spyOn>;

  beforeEach(() => {
    mocks.getLog.mockReset();
    mocks.openSessionSocket.mockReset();
    mocks.sockets.length = 0;
    mocks.terminals.length = 0;
    mocks.openSessionSocket.mockImplementation(() => {
      const socket = new FakeSocket();
      mocks.sockets.push(socket);
      return socket;
    });
    vi.stubGlobal(
      "ResizeObserver",
      class {
        observe() {}
        disconnect() {}
      },
    );
    vi.stubGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => {
      callback(0);
      return 1;
    });
    vi.stubGlobal("cancelAnimationFrame", () => {});
    visibility = vi
      .spyOn(document, "visibilityState", "get")
      .mockReturnValue("hidden");
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
    visibility.mockRestore();
    vi.useRealTimers();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("sends dimensions first and orders reset before the fresh stream", () => {
    act(() => root.render(<TerminalView sessionId="one" supervisor="tmux" />));
    const socket = mocks.sockets[0];
    act(() => socket.open());

    expect(JSON.parse(socket.sent[0] as string)).toEqual({
      type: "hello",
      cols: 80,
      rows: 24,
    });

    act(() => {
      socket.message(JSON.stringify({ type: "ready" }));
      socket.message(new Uint8Array([102, 114, 101, 115, 104]).buffer);
    });
    expect(mocks.terminals[0].writes).toEqual([
      "\x1bc",
      new Uint8Array([102, 114, 101, 115, 104]),
    ]);
  });

  it("treats an older daemon's first binary frame as ready", () => {
    act(() => root.render(<TerminalView sessionId="one" supervisor="tmux" />));
    const socket = mocks.sockets[0];
    act(() => {
      socket.open();
      socket.message(new Uint8Array([111, 108, 100]).buffer);
    });
    expect(mocks.terminals[0].writes).toEqual([
      "\x1bc",
      new Uint8Array([111, 108, 100]),
    ]);
  });

  it("replaces stopped history before a resumed live stream", async () => {
    mocks.getLog.mockResolvedValue(new Uint8Array([108, 111, 103]));
    await act(async () => {
      root.render(
        <TerminalView sessionId="one" live={false} supervisor="tmux" />,
      );
    });
    expect(mocks.terminals[0].writes).toEqual([
      "\x1bc",
      new Uint8Array([108, 111, 103]),
    ]);

    act(() =>
      root.render(<TerminalView sessionId="one" live supervisor="tmux" />),
    );
    const socket = mocks.sockets[0];
    act(() => {
      socket.open();
      socket.message(JSON.stringify({ type: "ready" }));
      socket.message(new Uint8Array([108, 105, 118, 101]).buffer);
    });
    expect(mocks.terminals[0].writes).toEqual([
      "\x1bc",
      new Uint8Array([108, 111, 103]),
      "\x1bc",
      new Uint8Array([108, 105, 118, 101]),
    ]);
  });

  it("keeps a healthy visible socket and replaces an unresponsive one once", () => {
    vi.useFakeTimers();
    act(() => root.render(<TerminalView sessionId="one" supervisor="tmux" />));
    const first = mocks.sockets[0];
    act(() => first.open());

    visibility.mockReturnValue("visible");
    act(() => document.dispatchEvent(new Event("visibilitychange")));
    expect(JSON.parse(first.sent.at(-1) as string)).toEqual({ type: "ping" });
    act(() => first.message(JSON.stringify({ type: "pong" })));
    act(() => vi.advanceTimersByTime(3_000));
    expect(mocks.sockets).toHaveLength(1);

    act(() => document.dispatchEvent(new Event("visibilitychange")));
    act(() => vi.advanceTimersByTime(3_000));
    expect(mocks.sockets).toHaveLength(2);

    act(() => first.message(new Uint8Array([115, 116, 97, 108, 101]).buffer));
    expect(mocks.terminals[0].writes).toEqual([]);
  });

  it("backs off failed attaches until a ready stream stays healthy", () => {
    vi.useFakeTimers();
    act(() => root.render(<TerminalView sessionId="one" supervisor="tmux" />));

    const first = mocks.sockets[0];
    act(() => {
      first.open();
      vi.advanceTimersByTime(12_000);
      first.serverClose();
      vi.advanceTimersByTime(999);
    });
    expect(mocks.sockets).toHaveLength(1);
    act(() => vi.advanceTimersByTime(1));
    expect(mocks.sockets).toHaveLength(2);

    const second = mocks.sockets[1];
    act(() => {
      second.open();
      second.serverClose();
      vi.advanceTimersByTime(1_999);
    });
    expect(mocks.sockets).toHaveLength(2);
    act(() => vi.advanceTimersByTime(1));
    expect(mocks.sockets).toHaveLength(3);

    const ready = mocks.sockets[2];
    act(() => {
      ready.open();
      ready.message(JSON.stringify({ type: "ready" }));
      vi.advanceTimersByTime(5_000);
      ready.serverClose();
      vi.advanceTimersByTime(499);
    });
    expect(mocks.sockets).toHaveLength(3);
    act(() => vi.advanceTimersByTime(1));
    expect(mocks.sockets).toHaveLength(4);
  });
});
