import { act, createElement } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import type { Session } from "../state/api";
import { useSessions } from "../state/sessionStore";
import { matchesSession, SessionList } from "./SessionList";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const session: Session = {
  id: "session-1",
  name: "auth-refactor",
  backend: "codex",
  execution_policy: "unrestricted",
  location: "remote",
  ssh_host: "buildbox",
  base_dir: "/code/slide",
  project_path: "/code/slide/.slide-worktrees/auth-refactor",
  worktree: true,
  state: "waiting",
  created_at: 1,
  last_activity: 2,
  supervisor: "tmux",
};

describe("session filtering", () => {
  it("matches identity, host, backend, state, and path without case sensitivity", () => {
    for (const query of ["AUTH", "buildbox", "CODEX", "waiting", "slide-worktrees"]) {
      expect(matchesSession(session, query)).toBe(true);
    }
    expect(matchesSession(session, "unrelated")).toBe(false);
    expect(matchesSession(session, "  ")).toBe(true);
  });
});

describe("session grouping", () => {
  let container: HTMLDivElement;
  let root: ReturnType<typeof createRoot>;

  beforeEach(() => {
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
  });

  it("renders stopped sessions in a bottom-anchored group", () => {
    const stopped = {
      ...session,
      id: "stopped",
      name: "stopped",
      state: "stopped" as const,
    };
    const unknown = {
      ...session,
      id: "unknown",
      name: "unknown",
      state: "unknown" as const,
    };
    useSessions.setState({
      sessions: { stopped, unknown },
      // Keep this deliberately stale: the view must enforce the visual groups.
      order: ["stopped", "unknown"],
      activeId: null,
      connected: true,
      authError: null,
      error: null,
    });

    act(() =>
      root.render(
        createElement(SessionList, {
          onNew: () => {},
          onDiagnostics: () => {},
        }),
      ),
    );

    expect(container.querySelector(".session-group-live")?.textContent).toContain(
      "unknown",
    );
    expect(container.querySelector(".session-group-stopped")?.textContent).toContain(
      "stopped",
    );
    expect(
      container.querySelector(".session-group-live + .session-group-stopped"),
    ).not.toBeNull();
  });
});
