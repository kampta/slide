import type { Session } from "../state/api";
import { SessionPath } from "./SessionPath";

export function SessionItem({
  session,
  active,
  onClick,
}: {
  session: Session;
  active: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      className={`session-item state-${session.state}${active ? " active" : ""}`}
      onClick={onClick}
      aria-current={active ? "page" : undefined}
    >
      <span
        className={`dot dot-${session.state}`}
        title={session.state === "unknown" ? "State unknown — session is still running" : session.state}
        aria-label={`Session state: ${session.state}`}
      />
      <SessionPath session={session} />
      <span className="badge">{session.backend}</span>
    </button>
  );
}
