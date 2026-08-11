import { useEffect, useRef, useState } from "react";
import { api, type Artifact, type ArtifactList } from "../state/api";

type ArtifactKind = "image" | "video" | "audio" | "document";

export function artifactKind(contentType: string): ArtifactKind {
  if (contentType.startsWith("image/")) return "image";
  if (contentType.startsWith("video/")) return "video";
  if (contentType.startsWith("audio/")) return "audio";
  return "document";
}

function formatSize(bytes: number): string {
  if (bytes >= 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
  if (bytes >= 1024) return `${Math.round(bytes / 1024)} KiB`;
  return `${bytes} B`;
}

function ArtifactPreview({ sessionId, artifact }: { sessionId: string; artifact: Artifact }) {
  const previewRef = useRef<HTMLDivElement>(null);
  const [visible, setVisible] = useState(false);
  const [url, setUrl] = useState<string | null>(null);
  const [failed, setFailed] = useState(false);

  useEffect(() => {
    const preview = previewRef.current;
    if (!preview || typeof IntersectionObserver === "undefined") {
      setVisible(true);
      return;
    }
    const observer = new IntersectionObserver(
      ([entry]) => {
        if (entry.isIntersecting) {
          setVisible(true);
          observer.disconnect();
        }
      },
      { rootMargin: "200px" },
    );
    observer.observe(preview);
    return () => observer.disconnect();
  }, []);

  useEffect(() => {
    if (!visible) return;
    let cancelled = false;
    let objectUrl: string | null = null;
    setUrl(null);
    setFailed(false);
    api
      .getArtifactBlob(sessionId, artifact.id)
      .then((blob) => {
        if (cancelled) return;
        objectUrl = URL.createObjectURL(blob);
        setUrl(objectUrl);
      })
      .catch(() => {
        if (!cancelled) setFailed(true);
      });
    return () => {
      cancelled = true;
      if (objectUrl) URL.revokeObjectURL(objectUrl);
    };
  }, [artifact.id, artifact.size, sessionId, visible]);

  const kind = artifactKind(artifact.content_type);
  return (
    <article className="artifact-card">
      <div className="artifact-preview" ref={previewRef}>
        {!visible && <span>Preview loads when visible</span>}
        {visible && !url && !failed && <span>Loading preview…</span>}
        {failed && <span>Preview unavailable</span>}
        {url && kind === "image" && <img src={url} alt={artifact.title || artifact.filename} loading="lazy" />}
        {url && kind === "video" && <video src={url} controls preload="metadata" />}
        {url && kind === "audio" && <audio src={url} controls preload="metadata" />}
        {url && kind === "document" && <span className="artifact-document">PDF</span>}
      </div>
      <div className="artifact-meta">
        <strong>{artifact.title || artifact.filename}</strong>
        {artifact.text && <p>{artifact.text}</p>}
        <span>{artifact.filename} · {formatSize(artifact.size)}</span>
        {url && (
          <a href={url} target="_blank" rel="noreferrer" download={artifact.filename}>
            Open result
          </a>
        )}
      </div>
    </article>
  );
}

export function ArtifactDock({ sessionId }: { sessionId: string }) {
  const [snapshot, setSnapshot] = useState<ArtifactList | null>(null);
  const [failed, setFailed] = useState(false);
  const [expanded, setExpanded] = useState(false);
  const [refreshNonce, setRefreshNonce] = useState(0);

  useEffect(() => {
    if (!expanded) return;
    let cancelled = false;
    setFailed(false);
    void api.listArtifacts(sessionId).then(
      (next) => {
        if (!cancelled) setSnapshot(next);
      },
      () => {
        if (!cancelled) setFailed(true);
      },
    );
    return () => {
      cancelled = true;
    };
  }, [expanded, refreshNonce, sessionId]);

  const artifacts = snapshot?.artifacts ?? [];
  return (
    <section className="artifact-dock" aria-label="Published artifacts">
      <div className="artifact-dock-header">
        <button type="button" aria-expanded={expanded} onClick={() => setExpanded((value) => !value)}>
          <span aria-hidden="true">{expanded ? "▾" : "▸"}</span>
          <span>Artifacts</span>
          <span className="count">{artifacts.length}</span>
        </button>
        <button
          type="button"
          className="btn-icon artifact-refresh"
          onClick={() => setRefreshNonce((value) => value + 1)}
          title="Refresh artifacts"
          aria-label="Refresh artifacts"
        >
          ↻
        </button>
      </div>
      {expanded && (
        <div className="artifact-body">
          {failed ? (
            <p className="artifact-empty">Artifacts unavailable.</p>
          ) : artifacts.length > 0 ? (
            <div className="artifact-grid">
              {artifacts.map((artifact) => (
                <ArtifactPreview
                  key={`${artifact.id}:${artifact.filename}:${artifact.size}`}
                  sessionId={sessionId}
                  artifact={artifact}
                />
              ))}
            </div>
          ) : (
            <p className="artifact-empty">
              Publish plots, images, audio, video, or PDFs by writing a bounded manifest to <code>$SLIDE_ARTIFACT_MANIFEST</code>.
            </p>
          )}
          {snapshot && snapshot.unavailable > 0 && (
            <p className="artifact-unavailable">{snapshot.unavailable} manifest entr{snapshot.unavailable === 1 ? "y is" : "ies are"} unavailable or over the size limit.</p>
          )}
        </div>
      )}
    </section>
  );
}
