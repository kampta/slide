import {
  forwardRef,
  useEffect,
  useImperativeHandle,
  useRef,
} from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebLinksAddon } from "@xterm/addon-web-links";
import {
  api,
  openSessionSocket,
  WS_CLOSE_AUTH_FAILED,
} from "../state/api";
import type { Supervisor } from "../state/api";
import { useIsMobile } from "../hooks/useMediaQuery";
import {
  attachMouseSelection,
  attachTouchNavigation,
  attachVisibleReconnect,
  clipboardAction,
  queueTerminalReset,
} from "./terminalInteractions";

const TERMINAL_HEALTH_TIMEOUT_MS = 3_000;
const TERMINAL_STABLE_CONNECTION_MS = 5_000;

/// Imperative handle exposed via forwardRef so the parent (and the
/// MobileKeyBar it renders) can write raw bytes into the active session
/// without piercing the xterm encapsulation. Returns silently when no
/// live WebSocket is open (e.g. session is Stopped).
export interface TerminalHandle {
  sendBytes: (bytes: Uint8Array) => void;
}

export const TerminalView = forwardRef<
  TerminalHandle,
  { sessionId: string; live?: boolean; supervisor: Supervisor }
>(function TerminalView({ sessionId, live = true, supervisor }, ref) {
  const hostRef = useRef<HTMLDivElement | null>(null);
  const termRef = useRef<Terminal | null>(null);
  const wsRef = useRef<WebSocket | null>(null);
  const scheduleFitRef = useRef<() => void>(() => {});
  const isMobile = useIsMobile();

  useImperativeHandle(
    ref,
    () => ({
      sendBytes: (bytes: Uint8Array) => {
        const ws = wsRef.current;
        if (ws?.readyState === WebSocket.OPEN) ws.send(bytes);
      },
    }),
    [],
  );

  // Mount xterm once per sessionId. The instance, scrollback, and any
  // bytes already shown survive Active↔Stopped transitions; only the live
  // WebSocket is torn up and down (handled in the second effect).
  useEffect(() => {
    if (!hostRef.current) return;
    const term = new Terminal({
      convertEol: false,
      cursorBlink: true,
      fontFamily:
        'ui-monospace, "SF Mono", Menlo, "JetBrains Mono", Consolas, monospace',
      // Smaller default on phones — 13px wraps too aggressively in 375px-wide
      // viewports. Desktop layout is tuned for 13px so keep that elsewhere.
      fontSize: isMobile ? 12 : 13,
      theme: {
        background: "#0b0c0f",
        foreground: "#d6d6d6",
        cursor: "#e6e6e6",
        selectionBackground: "#264f78",
      },
      scrollback: 20_000,
      allowProposedApi: true,
    });
    const fit = new FitAddon();
    const webLinks = new WebLinksAddon();
    term.loadAddon(fit);
    term.loadAddon(webLinks);
    term.open(hostRef.current);
    fit.fit();
    term.focus();
    termRef.current = term;

    // Clipboard. Without this, every keystroke including Cmd+C/Cmd+V
    // (or Ctrl+Shift+C/Ctrl+Shift+V on Linux/Windows) reaches `onData`
    // and gets shipped to the PTY — the browser never sees a chance
    // to copy or paste. Returning false from the handler skips xterm's
    // default processing for that one event.
    const isMac = navigator.platform.toUpperCase().includes("MAC");
    term.attachCustomKeyEventHandler((e) => {
      const action = clipboardAction(e, isMac, term.hasSelection());
      if (action === "copy") {
        navigator.clipboard.writeText(term.getSelection()).catch(() => {});
        return false;
      }
      if (action === "native-paste") {
        // The browser dispatches a paste event after this keydown and xterm
        // forwards that clipboard payload through onData. Manually calling
        // readText() + term.paste() here sends the same text a second time.
        return false;
      }
      return true;
    });

    // iOS predictive text otherwise inserts smart-quotes and capitalisation
    // into the helper textarea, corrupting keystrokes by the time xterm
    // sees them. xterm exposes the textarea as a DOM child; tweak it
    // directly because the constructor doesn't take these options.
    const helper = hostRef.current.querySelector(
      ".xterm-helper-textarea",
    ) as HTMLTextAreaElement | null;
    if (helper) {
      helper.setAttribute("autocorrect", "off");
      helper.setAttribute("autocapitalize", "off");
      helper.setAttribute("spellcheck", "false");
    }

    const onData = term.onData((data) => {
      const ws = wsRef.current;
      if (ws?.readyState === WebSocket.OPEN) {
        ws.send(new TextEncoder().encode(data));
      }
    });

    const detachMouse = attachMouseSelection(hostRef.current, term);
    const detachTouch = attachTouchNavigation(
      hostRef.current,
      term,
      () => wsRef.current,
      supervisor,
    );

    let resizeFrame: number | null = null;
    const fitAndResize = () => {
      resizeFrame = null;
      try {
        fit.fit();
        const ws = wsRef.current;
        if (ws?.readyState === WebSocket.OPEN) {
          ws.send(
            JSON.stringify({ type: "resize", cols: term.cols, rows: term.rows }),
          );
        }
      } catch {}
    };
    const scheduleFit = () => {
      if (resizeFrame === null) resizeFrame = requestAnimationFrame(fitAndResize);
    };
    scheduleFitRef.current = scheduleFit;
    const observer = new ResizeObserver(scheduleFit);
    observer.observe(hostRef.current);

    return () => {
      observer.disconnect();
      if (resizeFrame !== null) cancelAnimationFrame(resizeFrame);
      scheduleFitRef.current = () => {};
      onData.dispose();
      detachMouse();
      detachTouch();
      wsRef.current?.close();
      wsRef.current = null;
      term.dispose();
      termRef.current = null;
    };
  }, [sessionId, supervisor]);

  useEffect(() => {
    const term = termRef.current;
    if (!term) return;
    term.options.fontSize = isMobile ? 12 : 13;
    scheduleFitRef.current();
  }, [isMobile]);

  // Live tmux sessions get a fresh, correctly sized tmux client stream;
  // direct sessions receive an atomic ring snapshot followed by live bytes.
  // Stopped sessions pull their bounded on-disk history once.
  useEffect(() => {
    const term = termRef.current;
    if (!term) return;

    if (live) {
      let disposed = false;
      let retryTimer: number | null = null;
      let healthTimer: number | null = null;
      let stableTimer: number | null = null;
      let attempts = 0;
      let authFailed = false;

      const clearHealthTimer = () => {
        if (healthTimer === null) return;
        window.clearTimeout(healthTimer);
        healthTimer = null;
      };

      const clearStableTimer = () => {
        if (stableTimer === null) return;
        window.clearTimeout(stableTimer);
        stableTimer = null;
      };

      const connect = () => {
        if (disposed) return;
        if (retryTimer !== null) {
          window.clearTimeout(retryTimer);
          retryTimer = null;
        }
        clearHealthTimer();
        clearStableTimer();
        attempts += 1;
        const ws = openSessionSocket(sessionId);
        let needsReset = true;
        const markReady = () => {
          if (!needsReset) return;
          queueTerminalReset(term);
          needsReset = false;
          clearStableTimer();
          stableTimer = window.setTimeout(() => {
            if (wsRef.current === ws && ws.readyState === WebSocket.OPEN) {
              attempts = 0;
            }
          }, TERMINAL_STABLE_CONNECTION_MS);
        };
        wsRef.current = ws;
        ws.onopen = () => {
          if (disposed || wsRef.current !== ws) return;
          ws.send(
            JSON.stringify({ type: "hello", cols: term.cols, rows: term.rows }),
          );
        };
        ws.onmessage = (e) => {
          if (disposed || wsRef.current !== ws) return;
          if (e.data instanceof ArrayBuffer) {
            // Older daemons have no `ready` control frame. Treat their first
            // binary frame as readiness so an already-open tab can cross a
            // daemon upgrade or rollback without appending two streams.
            markReady();
            term.write(new Uint8Array(e.data));
            return;
          }
          if (typeof e.data !== "string") return;
          try {
            const message = JSON.parse(e.data);
            if (message.type === "ready") markReady();
            if (message.type === "terminal_reset") queueTerminalReset(term);
            if (message.type === "pong") clearHealthTimer();
            if (message.type === "error" && typeof message.error === "string") {
              term.writeln(`\r\n\x1b[2m[slide] ${message.error}\x1b[0m`);
            }
          } catch {
            // PTY output is binary; unknown text frames are protocol noise.
          }
        };
        ws.onclose = (event) => {
          // A visibility-driven reconnect replaces the socket immediately.
          // Ignore the late close from that superseded connection so it cannot
          // schedule a second reconnect over the new one.
          if (wsRef.current !== ws) return;
          wsRef.current = null;
          clearHealthTimer();
          clearStableTimer();
          if (event.code === WS_CLOSE_AUTH_FAILED) authFailed = true;
          if (disposed || authFailed) return;
          retryTimer = window.setTimeout(connect, Math.min(500 * 2 ** attempts, 5000));
        };
        ws.onerror = () => ws.close();
      };

      connect();
      const detachVisibility = attachVisibleReconnect(() => {
        if (disposed || authFailed) return;
        const current = wsRef.current;
        if (current?.readyState === WebSocket.OPEN) {
          clearHealthTimer();
          current.send(JSON.stringify({ type: "ping" }));
          healthTimer = window.setTimeout(() => {
            if (wsRef.current !== current) return;
            wsRef.current = null;
            current.close();
            connect();
          }, TERMINAL_HEALTH_TIMEOUT_MS);
          return;
        }
        if (retryTimer !== null) {
          window.clearTimeout(retryTimer);
          retryTimer = null;
        }
        const stale = wsRef.current;
        wsRef.current = null;
        stale?.close();
        connect();
      });
      return () => {
        disposed = true;
        detachVisibility();
        clearHealthTimer();
        clearStableTimer();
        if (retryTimer !== null) window.clearTimeout(retryTimer);
        const ws = wsRef.current;
        wsRef.current = null;
        ws?.close();
      };
    }

    let cancelled = false;
    (async () => {
      try {
        const bytes = await api.getLog(sessionId);
        if (!cancelled) {
          // The persisted stopped log is the canonical final snapshot. Reset
          // before writing so the last PTY frames cannot be lost or duplicated
          // when lifecycle and output sockets close in different orders.
          queueTerminalReset(term);
          term.write(bytes);
        }
      } catch {
        if (!cancelled) {
          term.writeln("\r\n\x1b[2m[slide] terminal history unavailable; reopen to retry\x1b[0m");
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [sessionId, live]);

  // Tap-to-focus and swipe-to-scroll are both handled by the native
  // touch listeners inside the mount effect — see the touch-handling
  // block. JSX onTouchEnd would race with those.
  return <div className="term-host" ref={hostRef} tabIndex={0} />;
});
