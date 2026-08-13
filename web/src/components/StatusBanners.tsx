import { useSessions } from "../state/sessionStore";

export function StatusBanners({ floating = false }: { floating?: boolean }) {
  const connected = useSessions((state) => state.connected);
  const authError = useSessions((state) => state.authError);
  const error = useSessions((state) => state.error);
  const clearError = useSessions((state) => state.clearError);

  if (connected && !authError && !error) return null;

  return (
    <div className={`status-banners${floating ? " status-banners-floating" : ""}`}>
      {!connected && !authError && (
        <div className="disconnect-banner" role="status" aria-live="polite">
          Disconnected — retrying…
        </div>
      )}
      {(authError || error) && (
        <div className="error-banner" role="alert">
          <span>{authError || error}</span>
          {error && !authError && (
            <button type="button" onClick={clearError} aria-label="Dismiss error">
              ×
            </button>
          )}
        </div>
      )}
    </div>
  );
}
