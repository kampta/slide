import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useModalDialog } from "./useModalDialog";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

function Dialog({ onClose }: { onClose: () => void }) {
  const ref = useModalDialog<HTMLDivElement>(true, onClose);
  return (
    <div ref={ref} role="dialog" tabIndex={-1}>
      <button>First</button>
      <button>Last</button>
    </div>
  );
}

describe("useModalDialog", () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
  });

  it("closes on Escape and keeps Tab focus inside", async () => {
    const onClose = vi.fn();
    act(() => root.render(<Dialog onClose={onClose} />));
    await act(() => Promise.resolve());
    const buttons = container.querySelectorAll<HTMLButtonElement>("button");
    expect(document.activeElement).toBe(buttons[0]);

    buttons[1].focus();
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Tab", bubbles: true }));
    expect(document.activeElement).toBe(buttons[0]);

    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(onClose).toHaveBeenCalledOnce();
  });
});
