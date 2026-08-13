import { useEffect, useState } from "react";

interface KeyDef {
  label: string;
  bytes: Uint8Array;
  title?: string;
}

export function shouldSendClick(detail: number): boolean {
  return detail === 0;
}

const enc = (s: string) => new TextEncoder().encode(s);

// xterm key codes: arrows are ESC [ A/B/C/D, Esc is 0x1b, Tab 0x09,
// Ctrl+letter is letter ASCII − 0x40 (so Ctrl-C=0x03, Ctrl-D=0x04).
// "Esc Esc" is the Claude interrupt sequence (two consecutive escapes).
const KEYS: KeyDef[] = [
  { label: "Esc", bytes: new Uint8Array([0x1b]), title: "Escape" },
  { label: "Tab", bytes: new Uint8Array([0x09]) },
  { label: "↑", bytes: enc("\x1b[A"), title: "Up arrow" },
  { label: "↓", bytes: enc("\x1b[B"), title: "Down arrow" },
  { label: "←", bytes: enc("\x1b[D"), title: "Left arrow" },
  { label: "→", bytes: enc("\x1b[C"), title: "Right arrow" },
  { label: "^C", bytes: new Uint8Array([0x03]), title: "Ctrl+C (interrupt)" },
  { label: "^D", bytes: new Uint8Array([0x04]), title: "Ctrl+D (EOF)" },
  { label: "Esc Esc", bytes: new Uint8Array([0x1b, 0x1b]), title: "Claude interrupt" },
];

/// Sticky bar above the soft keyboard with terminal-only modifier keys.
/// Positioned via the visualViewport API so it floats above the keyboard
/// instead of being hidden by it. Falls back to bottom: 0 in browsers
/// without visualViewport (none of our targets, but harmless).
export function MobileKeyBar({
  onSend,
}: {
  onSend: (b: Uint8Array) => void;
}) {
  const [bottomOffset, setBottomOffset] = useState(0);

  useEffect(() => {
    const vv = window.visualViewport;
    if (!vv) return;
    const update = () => {
      // When the soft keyboard is open, vv.height is reduced and
      // vv.offsetTop covers the keyboard's height. The diff is what we
      // need to translate the bar upwards by.
      const diff = window.innerHeight - vv.height - vv.offsetTop;
      setBottomOffset(Math.max(0, diff));
    };
    update();
    vv.addEventListener("resize", update);
    vv.addEventListener("scroll", update);
    return () => {
      vv.removeEventListener("resize", update);
      vv.removeEventListener("scroll", update);
    };
  }, []);

  return (
    <div
      className="mobile-keybar"
      style={{ transform: `translateY(${-bottomOffset}px)` }}
      role="toolbar"
      aria-label="Terminal modifier keys"
    >
      {KEYS.map((k) => (
        <button
          key={k.label}
          type="button"
          className="mobile-key"
          // tabIndex=-1 so iOS Safari doesn't shift focus to the button
          // on tap — focus shift would close the soft keyboard and trigger
          // a viewport resize cycle.
          tabIndex={-1}
          title={k.title ?? k.label}
          // touchstart + preventDefault is the iOS-reliable path: it fires
          // before the synthetic mouse cascade and also blocks the focus
          // shift to the button. mousedown covers desktop test runs.
          // Both call the same dispatch path; preventDefault on each
          // stops the OS from re-firing through the other channel.
          onTouchStart={(e) => {
            e.preventDefault();
            onSend(k.bytes);
          }}
          onMouseDown={(e) => {
            e.preventDefault();
            onSend(k.bytes);
          }}
          // Keyboard activation produces a click with detail=0. Pointer clicks
          // were already handled above, so ignore their synthetic click.
          onClick={(e) => {
            if (shouldSendClick(e.detail)) onSend(k.bytes);
          }}
        >
          {k.label}
        </button>
      ))}
    </div>
  );
}
