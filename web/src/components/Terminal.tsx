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
  clipboardAction,
  filterTerminalResponse,
} from "./terminalInteractions";

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
  // Set once *either* the WS snapshot or the disk-log fetch has populated
  // the terminal for the current sessionId. Prevents re-writing the disk
  // log on Active↔Stopped toggles, which would duplicate scrollback.
  const backfilledRef = useRef(false);
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
    backfilledRef.current = false;

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
      scrollback: 5000,
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
      const filtered = filterTerminalResponse(data, supervisor);
      if (!filtered) return;
      const ws = wsRef.current;
      if (ws?.readyState === WebSocket.OPEN) {
        ws.send(new TextEncoder().encode(filtered));
      }
    });

    const detachMouse = attachMouseSelection(hostRef.current, term);
    const detachTouch = attachTouchNavigation(hostRef.current, term, () => wsRef.current);

    const observer = new ResizeObserver(() => {
      try {
        fit.fit();
        wsRef.current?.send(
          JSON.stringify({ type: "resize", cols: term.cols, rows: term.rows }),
        );
      } catch {}
    });
    observer.observe(hostRef.current);

    return () => {
      observer.disconnect();
      onData.dispose();
      detachMouse();
      detachTouch();
      wsRef.current?.close();
      wsRef.current = null;
      term.dispose();
      termRef.current = null;
    };
  }, [sessionId, isMobile]);

  // Manage the data source. `live=true` opens a WebSocket whose first frame
  // is a server-side snapshot atomic with the live subscription (see
  // SessionManager::subscribe_output_with_snapshot) followed by streamed
  // bytes. `live=false` pulls the on-disk log exactly once per sessionId.
  // Toggling `live` without remount never re-fetches the log.
  useEffect(() => {
    const term = termRef.current;
    if (!term) return;

    if (live) {
      let disposed = false;
      let retryTimer: number | null = null;
      let attempts = 0;
      let connectedOnce = false;

      const resetTerminal = () => {
        term.reset();
        term.clear();
      };

      const connect = () => {
        if (disposed) return;
        if (connectedOnce) resetTerminal();
        attempts += 1;
        const ws = openSessionSocket(sessionId);
        wsRef.current = ws;
        ws.onopen = () => {
          attempts = 0;
          connectedOnce = true;
          ws.send(
            JSON.stringify({ type: "resize", cols: term.cols, rows: term.rows }),
          );
        };
        ws.onmessage = (e) => {
          if (e.data instanceof ArrayBuffer) {
            term.write(new Uint8Array(e.data));
            backfilledRef.current = true;
            return;
          }
          if (typeof e.data !== "string") return;
          try {
            const message = JSON.parse(e.data);
            if (message.type === "terminal_reset") resetTerminal();
            if (message.type === "error" && typeof message.error === "string") {
              term.writeln(`\r\n\x1b[2m[slide] ${message.error}\x1b[0m`);
            }
          } catch {
            // PTY output is binary; unknown text frames are protocol noise.
          }
        };
        ws.onclose = (event) => {
          if (wsRef.current === ws) wsRef.current = null;
          if (disposed || event.code === WS_CLOSE_AUTH_FAILED) return;
          retryTimer = window.setTimeout(connect, Math.min(500 * 2 ** attempts, 5000));
        };
        ws.onerror = () => ws.close();
      };

      connect();
      return () => {
        disposed = true;
        if (retryTimer !== null) window.clearTimeout(retryTimer);
        const ws = wsRef.current;
        wsRef.current = null;
        ws?.close();
      };
    }

    if (backfilledRef.current) return;
    let cancelled = false;
    (async () => {
      try {
        const bytes = await api.getLog(sessionId);
        if (!cancelled && bytes.length > 0) {
          term.write(bytes);
          backfilledRef.current = true;
        }
      } catch {
        // brand-new session or transient failure; if live mode is enabled
        // later, the WS snapshot will fill the terminal.
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
