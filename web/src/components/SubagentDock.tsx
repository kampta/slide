import { useEffect, useMemo, useState } from "react";
import { api, type Subagent, type SubagentState } from "../state/api";

const ACTIVE_STATES = new Set<SubagentState>(["starting", "running", "waiting"]);

const STATE_ORDER: Record<SubagentState, number> = {
  running: 0,
  starting: 1,
  waiting: 2,
  failed: 3,
  completed: 4,
};

export function subagentLabel(agent: Subagent): string {
  return agent.name?.trim() || agent.role?.trim() || `agent ${agent.id.slice(0, 8)}`;
}

export function sortSubagents(agents: Subagent[]): Subagent[] {
  return [...agents].sort(
    (a, b) => STATE_ORDER[a.state] - STATE_ORDER[b.state] || b.updated_at - a.updated_at,
  );
}

function formatDuration(seconds: number): string {
  const total = Math.max(0, Math.floor(seconds));
  if (total < 60) return `${total}s`;
  const minutes = Math.floor(total / 60);
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  return `${hours}h ${minutes % 60}m`;
}

function isoTimestamp(seconds: number): string | undefined {
  const date = new Date(seconds * 1000);
  return Number.isNaN(date.getTime()) ? undefined : date.toISOString();
}

function AgentRow({
  agent,
  rootThreadId,
}: {
  agent: Subagent;
  rootThreadId: string;
}) {
  const active = ACTIVE_STATES.has(agent.state);
  const end = active ? Date.now() / 1000 : agent.updated_at;
  const duration = formatDuration(end - agent.created_at);
  const nested = agent.parent_id !== rootThreadId;
  const role = agent.role?.trim();
  const label = subagentLabel(agent);

  return (
    <li className={`subagent-row${nested ? " subagent-nested" : ""}`}>
      <span
        className={`subagent-state subagent-state-${agent.state}`}
        aria-hidden="true"
      />
      <span className="subagent-copy">
        <span className="subagent-name" title={agent.id}>
          {label}
        </span>
        {role && role !== label && <span className="subagent-role">{role}</span>}
      </span>
      <span className="subagent-meta">
        <span>{agent.state}</span>
        <time dateTime={isoTimestamp(agent.updated_at)}>{duration}</time>
      </span>
    </li>
  );
}

export function SubagentDock({
  sessionId,
  rootThreadId,
  live,
}: {
  sessionId: string;
  rootThreadId: string;
  live: boolean;
}) {
  const [agents, setAgents] = useState<Subagent[]>([]);
  const [supported, setSupported] = useState(true);
  const [expanded, setExpanded] = useState(true);

  useEffect(() => {
    let cancelled = false;
    let timer: number | undefined;
    setAgents([]);
    setSupported(true);

    const poll = async () => {
      let keepPolling = live;
      let delay = 5_000;
      try {
        const snapshot = await api.getSubagents(sessionId);
        if (cancelled) return;
        setSupported(snapshot.supported);
        setAgents(snapshot.agents);
        keepPolling = live && snapshot.supported;
      } catch {
        // Keep the last good snapshot during a transient CLI/SSH failure.
        // An initial failure stays invisible instead of adding an error bar
        // for an optional capability.
        delay = 30_000;
      }
      if (!cancelled && keepPolling) {
        timer = window.setTimeout(poll, delay);
      }
    };

    void poll();
    return () => {
      cancelled = true;
      if (timer !== undefined) window.clearTimeout(timer);
    };
  }, [sessionId, rootThreadId, live]);

  const ordered = useMemo(() => sortSubagents(agents), [agents]);
  if (!supported || ordered.length === 0) return null;

  const activeCount = ordered.filter((agent) =>
    ACTIVE_STATES.has(agent.state),
  ).length;
  return (
    <aside className="subagent-dock" aria-label="Subagents">
      <button
        type="button"
        className="subagent-dock-header"
        onClick={() => setExpanded((value) => !value)}
        aria-expanded={expanded}
      >
        <span
          className={`chevron${expanded ? "" : " collapsed"}`}
          aria-hidden="true"
        >
          ›
        </span>
        <span>Subagents</span>
        <span className="count">{ordered.length}</span>
        {activeCount > 0 && (
          <span className="subagent-active-count">{activeCount} live</span>
        )}
      </button>
      {expanded && (
        <ul className="subagent-list">
          {ordered.map((agent) => (
            <AgentRow key={agent.id} agent={agent} rootThreadId={rootThreadId} />
          ))}
        </ul>
      )}
    </aside>
  );
}
