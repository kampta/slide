import { FormEvent, useEffect, useRef, useState } from "react";
import { api, type HistorySearchResponse } from "../state/api";

export interface HighlightSegment {
  text: string;
  match: boolean;
}

export function highlightSegments(text: string, query: string): HighlightSegment[] {
  const needle = query.toLocaleLowerCase();
  if (!needle) return [{ text, match: false }];
  const folded = text.toLocaleLowerCase();
  const segments: HighlightSegment[] = [];
  let cursor = 0;
  while (cursor < text.length) {
    const index = folded.indexOf(needle, cursor);
    if (index < 0) {
      segments.push({ text: text.slice(cursor), match: false });
      break;
    }
    if (index > cursor) {
      segments.push({ text: text.slice(cursor, index), match: false });
    }
    segments.push({ text: text.slice(index, index + query.length), match: true });
    cursor = index + query.length;
  }
  return segments.length > 0 ? segments : [{ text, match: false }];
}

export function HistorySearchModal({
  open,
  onClose,
  onSelect,
}: {
  open: boolean;
  onClose: () => void;
  onSelect: (sessionId: string) => void;
}) {
  const [query, setQuery] = useState("");
  const [response, setResponse] = useState<HistorySearchResponse | null>(null);
  const [submittedQuery, setSubmittedQuery] = useState("");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const requestRef = useRef(0);

  useEffect(() => {
    if (!open) {
      requestRef.current += 1;
      setQuery("");
      setResponse(null);
      setSubmittedQuery("");
      setLoading(false);
      setError(null);
    }
  }, [open]);

  if (!open) return null;

  async function submit(event: FormEvent) {
    event.preventDefault();
    const normalized = query.trim();
    if (normalized.length < 2 || loading) return;
    const request = ++requestRef.current;
    setLoading(true);
    setError(null);
    setResponse(null);
    try {
      const next = await api.searchHistory(normalized);
      if (request !== requestRef.current) return;
      setSubmittedQuery(normalized);
      setResponse(next);
    } catch (reason) {
      if (request !== requestRef.current) return;
      setResponse(null);
      setError(reason instanceof Error ? reason.message : String(reason));
    } finally {
      if (request === requestRef.current) setLoading(false);
    }
  }

  return (
    <div className="modal-backdrop" onMouseDown={onClose}>
      <section
        className="modal history-search-modal"
        role="dialog"
        aria-modal="true"
        aria-labelledby="history-search-title"
        onMouseDown={(event) => event.stopPropagation()}
      >
        <div className="diagnostics-heading">
          <div>
            <h2 id="history-search-title">Search history</h2>
            <p>Find text in persisted output from every local and SSH session.</p>
          </div>
          <button type="button" className="btn-icon" onClick={onClose} aria-label="Close search">
            ×
          </button>
        </div>
        <form className="history-search-form" onSubmit={submit}>
          <input
            autoFocus
            type="search"
            minLength={2}
            maxLength={200}
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            placeholder="Error message, filename, command…"
            aria-label="Search session history"
          />
          <button type="submit" className="btn-primary" disabled={loading || query.trim().length < 2}>
            {loading ? "Searching…" : "Search"}
          </button>
        </form>
        {error && <p className="error history-search-error">{error}</p>}
        <div className="history-search-results" aria-live="polite">
          {response?.results.map((result, index) => (
            <button
              type="button"
              className="history-search-result"
              key={`${result.session_id}-${result.position}-${index}`}
              onClick={() => onSelect(result.session_id)}
            >
              <span className="history-search-result-title">
                <strong>{result.session_name}</strong>
                <span>{result.backend} · {result.location} · {result.state}</span>
              </span>
              <span className="history-search-snippet">
                {highlightSegments(result.snippet, submittedQuery).map((segment, segmentIndex) =>
                  segment.match ? (
                    <mark key={segmentIndex}>{segment.text}</mark>
                  ) : (
                    <span key={segmentIndex}>{segment.text}</span>
                  ),
                )}
              </span>
            </button>
          ))}
          {response && response.results.length === 0 && (
            <p className="history-search-empty">No matching session output.</p>
          )}
        </div>
        {response && (
          <p className="history-search-summary">
            {response.results.length} result{response.results.length === 1 ? "" : "s"} across {response.searched_sessions} session{response.searched_sessions === 1 ? "" : "s"}
            {response.truncated ? " · newest matches shown" : ""}
            {response.unavailable_sessions > 0 ? ` · ${response.unavailable_sessions} unavailable` : ""}
          </p>
        )}
      </section>
    </div>
  );
}
