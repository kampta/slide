import type { Terminal } from "@xterm/xterm";
import type { Supervisor } from "../state/api";

export type ClipboardAction = "copy" | "native-paste";

const TMUX_HANDLED_DEVICE_ATTRIBUTES = new Set([
  "\x1b[?1;2c",
  "\x1b[>0;276;0c",
]);

/**
 * xterm.js emits these replies when it parses a device-attributes query from
 * the backend. A tmux-backed session has already handled that negotiation in
 * the PTY's terminal emulator, so forwarding xterm.js's duplicate reply can
 * make the backend line editor echo the printable tail (for example,
 * `0;276;0c`) into the prompt.
 */
export function filterTerminalResponse(data: string, supervisor: Supervisor): string {
  if (supervisor === "tmux" && TMUX_HANDLED_DEVICE_ATTRIBUTES.has(data)) return "";
  return data;
}

/** Identify platform clipboard shortcuts without performing the paste twice. */
export function clipboardAction(
  event: Pick<KeyboardEvent, "type" | "key" | "metaKey" | "ctrlKey" | "shiftKey">,
  isMac: boolean,
  hasSelection: boolean,
): ClipboardAction | null {
  if (event.type !== "keydown") return null;
  const mod = isMac
    ? event.metaKey && !event.ctrlKey
    : event.ctrlKey && event.shiftKey;
  if (!mod) return null;
  const key = event.key.toLowerCase();
  if (key === "c" && hasSelection) return "copy";
  if (key === "v") return "native-paste";
  return null;
}

type Cell = { col: number; row: number };
type Drag = Cell & { x: number; y: number; moved: boolean };

function cellAt(host: HTMLElement, term: Terminal, x: number, y: number): Cell | null {
  const screen = host.querySelector<HTMLElement>(".xterm-screen");
  if (!screen || term.cols === 0 || term.rows === 0) return null;
  const rect = screen.getBoundingClientRect();
  const cellWidth = rect.width / term.cols;
  const cellHeight = rect.height / term.rows;
  if (cellWidth <= 0 || cellHeight <= 0) return null;
  const clamp = (value: number, max: number) => Math.max(0, Math.min(max, value));
  return {
    col: clamp(Math.floor((x - rect.left) / cellWidth), term.cols - 1),
    row:
      clamp(Math.floor((y - rect.top) / cellHeight), term.rows - 1) +
      term.buffer.active.viewportY,
  };
}

function selectRange(term: Terminal, start: Cell, end: Cell) {
  const startFirst =
    start.row < end.row || (start.row === end.row && start.col <= end.col);
  const first = startFirst ? start : end;
  const last = startFirst ? end : start;
  const length = (last.row - first.row) * term.cols + last.col - first.col + 1;
  term.select(first.col, first.row, length);
}

/** Restore browser-side selection while tmux mouse tracking is enabled. */
export function attachMouseSelection(host: HTMLElement, term: Terminal): () => void {
  let drag: Drag | null = null;
  let selectionFrame: number | null = null;

  const onMouseDown = (event: MouseEvent) => {
    if (event.button !== 0) return;
    const cell = cellAt(host, term, event.clientX, event.clientY);
    if (cell) drag = { ...cell, x: event.clientX, y: event.clientY, moved: false };
  };

  const onMouseMove = (event: MouseEvent) => {
    if (!drag) return;
    const dx = event.clientX - drag.x;
    const dy = event.clientY - drag.y;
    if (!drag.moved && dx * dx + dy * dy < 9) return;
    drag.moved = true;
    const end = cellAt(host, term, event.clientX, event.clientY);
    if (end) selectRange(term, drag, end);
  };

  const onMouseUp = (event: MouseEvent) => {
    if (!drag) return;
    const start = drag;
    drag = null;
    if (!start.moved) {
      term.clearSelection();
      return;
    }
    const end = cellAt(host, term, event.clientX, event.clientY);
    if (!end) return;
    // xterm clears selection after forwarding mouseup to tmux. Reapply it
    // immediately before the next paint.
    selectionFrame = requestAnimationFrame(() => selectRange(term, start, end));
  };

  host.addEventListener("mousedown", onMouseDown);
  document.addEventListener("mousemove", onMouseMove);
  document.addEventListener("mouseup", onMouseUp);
  return () => {
    if (selectionFrame !== null) cancelAnimationFrame(selectionFrame);
    host.removeEventListener("mousedown", onMouseDown);
    document.removeEventListener("mousemove", onMouseMove);
    document.removeEventListener("mouseup", onMouseUp);
  };
}

/** Tap focuses xterm; vertical alt-screen swipes navigate agent history. */
export function attachTouchNavigation(
  host: HTMLElement,
  term: Terminal,
  socket: () => WebSocket | null,
): () => void {
  const tapThreshold = 10;
  const pixelsPerLine = 24;
  let startY: number | null = null;
  let lastY: number | null = null;
  let moved = false;

  const onTouchStart = (event: TouchEvent) => {
    if (event.touches.length !== 1) {
      startY = null;
      lastY = null;
      moved = false;
      return;
    }
    startY = event.touches[0].clientY;
    lastY = startY;
    moved = false;
  };

  const onTouchMove = (event: TouchEvent) => {
    if (startY === null || lastY === null || event.touches.length !== 1) return;
    if (term.buffer.active.type !== "alternate") return;
    const ws = socket();
    if (ws?.readyState !== WebSocket.OPEN) return;

    const currentY = event.touches[0].clientY;
    if (Math.abs(currentY - startY) < tapThreshold) return;
    moved = true;
    const delta = currentY - lastY;
    const lines = Math.trunc(Math.abs(delta) / pixelsPerLine);
    if (lines === 0) return;

    event.preventDefault();
    ws.send(new TextEncoder().encode((delta > 0 ? "\x1b[A" : "\x1b[B").repeat(lines)));
    lastY += Math.sign(delta) * lines * pixelsPerLine;
  };

  const finishTouch = (focus: boolean) => {
    const wasTap = !moved;
    startY = null;
    lastY = null;
    moved = false;
    if (focus && wasTap) term.focus();
  };
  const onTouchEnd = () => finishTouch(true);
  const onTouchCancel = () => finishTouch(false);

  host.addEventListener("touchstart", onTouchStart);
  host.addEventListener("touchmove", onTouchMove, { passive: false });
  host.addEventListener("touchend", onTouchEnd);
  host.addEventListener("touchcancel", onTouchCancel);
  return () => {
    host.removeEventListener("touchstart", onTouchStart);
    host.removeEventListener("touchmove", onTouchMove);
    host.removeEventListener("touchend", onTouchEnd);
    host.removeEventListener("touchcancel", onTouchCancel);
  };
}
