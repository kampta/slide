import { clusterLabel, repoLabel, type Session } from "../state/api";

export function sessionDisplayPath(session: Session): string {
  return `${clusterLabel(session)}:${[repoLabel(session), session.name]
    .filter(Boolean)
    .join("/")}`;
}

export function SessionPath({ session }: { session: Session }) {
  const cluster = clusterLabel(session);
  const repo = repoLabel(session);

  return (
    <span className="session-path" title={sessionDisplayPath(session)}>
      <span className="session-path-muted">{cluster}:</span>
      {repo && <span className="session-path-muted">{repo}/</span>}
      <span>{session.name}</span>
    </span>
  );
}
