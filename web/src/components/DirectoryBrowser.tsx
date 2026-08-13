import { useCallback, useEffect, useRef, useState } from "react";
import { api } from "../state/api";
import { parentPath } from "../state/path";

export function isLatestRequest(request: number, latest: number): boolean {
  return request === latest;
}

export function DirectoryBrowser({
  startPath,
  host,
  onSelect,
}: {
  startPath: string;
  host: string | null;
  onSelect: (path: string) => void;
}) {
  const [path, setPath] = useState(startPath);
  const [entries, setEntries] = useState<{ name: string; path: string }[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const requestId = useRef(0);

  const load = useCallback(
    async (target: string) => {
      const current = ++requestId.current;
      setLoading(true);
      setError(null);
      try {
        const result = await api.listDir({
          path: target || undefined,
          host: host ?? undefined,
        });
        if (!isLatestRequest(current, requestId.current)) return;
        setPath(result.path);
        setEntries(result.entries);
        onSelect(result.path);
      } catch (cause) {
        if (!isLatestRequest(current, requestId.current)) return;
        setError(cause instanceof Error ? cause.message : String(cause));
      } finally {
        if (isLatestRequest(current, requestId.current)) setLoading(false);
      }
    },
    [host, onSelect],
  );

  useEffect(() => {
    void load(startPath);
    // Switching host changes the filesystem root. Path changes initiated by
    // this component are already loaded by `load` and must not fetch twice.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [host]);

  const parent = parentPath(path);

  return (
    <div className="dir-browser">
      <div className="dir-browser-bar">
        <button
          type="button"
          className="dir-browser-up"
          onClick={() => parent && load(parent)}
          disabled={!parent || loading}
          title="Parent directory"
          aria-label="Parent directory"
        >
          ↑
        </button>
        <code className="dir-browser-path" title={path}>
          {path || "…"}
        </code>
      </div>
      {error ? (
        <div className="dir-browser-error" role="alert">
          {error}
        </div>
      ) : (
        <ul className="dir-browser-list">
          {entries.length === 0 && !loading && (
            <li className="dir-browser-empty">No subdirectories</li>
          )}
          {entries.map((entry) => (
            <li key={entry.path}>
              <button
                type="button"
                className="dir-browser-entry"
                onClick={() => load(entry.path)}
                disabled={loading}
              >
                {entry.name}
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
