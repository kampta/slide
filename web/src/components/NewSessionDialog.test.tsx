import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { api, type BackendInfo, type Session } from "../state/api";
import { NewSessionDialog } from "./NewSessionDialog";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const backends: BackendInfo[] = [
  {
    id: "claude",
    label: "Claude",
    context_usage: true,
    fork: true,
    execution_policies: ["unrestricted"],
  },
  {
    id: "codex",
    label: "Codex",
    context_usage: false,
    fork: true,
    execution_policies: ["unrestricted", "sandboxed_auto"],
  },
];

describe("NewSessionDialog", () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    vi.spyOn(api, "listBackends").mockResolvedValue(backends);
    vi.spyOn(api, "listSshHosts").mockResolvedValue([]);
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
    vi.restoreAllMocks();
  });

  it("only offers sandboxed auto when the backend advertises support", async () => {
    await act(async () => {
      root.render(
        <NewSessionDialog open onClose={() => {}} onCreated={() => {}} />,
      );
    });
    const local = Array.from(container.querySelectorAll("button")).find(
      (button) => button.textContent === "Local",
    );
    const codex = Array.from(container.querySelectorAll("button")).find(
      (button) => button.textContent === "Codex",
    );
    expect(local?.getAttribute("aria-pressed")).toBe("true");
    expect(codex?.getAttribute("aria-pressed")).toBe("false");
    expect(container.textContent).not.toContain("Sandboxed auto");

    await act(async () => {
      codex?.click();
    });
    expect(codex?.getAttribute("aria-pressed")).toBe("true");
    expect(container.textContent).toContain("Sandboxed auto");

    await act(async () => {
      Array.from(container.querySelectorAll("button"))
        .find((button) => button.textContent === "Claude")
        ?.click();
    });
    expect(container.textContent).not.toContain("Sandboxed auto");
  });

  it("closes with Escape and restores the previous focus", async () => {
    const opener = document.createElement("button");
    document.body.appendChild(opener);
    opener.focus();
    const onClose = vi.fn();

    await act(async () => {
      root.render(
        <NewSessionDialog open onClose={onClose} onCreated={() => {}} />,
      );
    });
    await act(async () => {
      document.dispatchEvent(
        new KeyboardEvent("keydown", { key: "Escape", bubbles: true }),
      );
    });
    expect(onClose).toHaveBeenCalledOnce();

    await act(async () => {
      root.render(
        <NewSessionDialog open={false} onClose={onClose} onCreated={() => {}} />,
      );
    });
    expect(document.activeElement).toBe(opener);
    opener.remove();
  });

  it("submits the selected Codex execution policy", async () => {
    const created: Session = {
      id: "created",
      name: "sandboxed",
      backend: "codex",
      execution_policy: "sandboxed_auto",
      location: "local",
      base_dir: "/tmp/repo",
      project_path: "/tmp/repo/.slide-worktrees/sandboxed",
      worktree: true,
      state: "active",
      created_at: 1,
      last_activity: 1,
      supervisor: "tmux",
    };
    const create = vi.spyOn(api, "createSession").mockResolvedValue(created);

    await act(async () => {
      root.render(
        <NewSessionDialog open onClose={() => {}} onCreated={() => {}} />,
      );
    });
    await act(async () => {
      const buttons = Array.from(container.querySelectorAll("button"));
      buttons.find((button) => button.textContent === "Codex")?.click();
    });
    await act(async () => {
      const buttons = Array.from(container.querySelectorAll("button"));
      buttons.find((button) => button.textContent === "Sandboxed auto")?.click();
    });

    const [baseDir, name] = Array.from(container.querySelectorAll("input"));
    await act(async () => {
      const setValue = Object.getOwnPropertyDescriptor(
        HTMLInputElement.prototype,
        "value",
      )?.set;
      setValue?.call(baseDir, "/tmp/repo");
      baseDir.dispatchEvent(new Event("input", { bubbles: true }));
      setValue?.call(name, "sandboxed");
      name.dispatchEvent(new Event("input", { bubbles: true }));
    });
    await act(async () => {
      container.querySelector("form")?.dispatchEvent(
        new Event("submit", { bubbles: true, cancelable: true }),
      );
    });

    expect(create).toHaveBeenCalledWith({
      name: "sandboxed",
      backend: "codex",
      execution_policy: "sandboxed_auto",
      location: "local",
      ssh_host: undefined,
      base_dir: "/tmp/repo",
    });
  });
});
