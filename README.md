# slide

A lightweight IDE for juggling coding-agent sessions (Claude Code, Codex, …) at once. The session list keeps live work first and stopped work collapsed, while hotkeys jump directly to **Waiting** or **Active** sessions. Each local session runs in an auto-created git worktree so concurrent agents don't stomp on each other.

Architecture: a small Rust daemon (`slide serve`) hosts an HTTP+WebSocket API and serves a React SPA. Open it in any browser.

## Quick start (any fresh macOS/Linux laptop)

```bash
git clone https://github.com/kampta/slide
cd slide
./scripts/bootstrap.sh   # installs rustup, node, and JS deps
./scripts/dev.sh         # runs daemon + Vite dev server
# → click the http://localhost:5173/?token=… URL the daemon prints
```

In dev mode the daemon prints the full URL with the bootstrap token so you can click it. The token is stripped from the URL and stored in `localStorage` so paired mobile browsers survive tab suspension and restarts.

Or, after `bootstrap.sh`, build a single binary:

```bash
(cd web && npm run build)
cargo build --release -p slide-cli
./target/release/slide serve  # auto-opens the browser; token never hits stdout
```

If you started the daemon with `--no-open`, run `slide open` from another terminal to launch the browser. `slide token` will print just the token if you need it for a manual paste.

## Hotkeys

| Key | Action |
| --- | --- |
| `Alt+N` | New session dialog |
| `Alt+J` / `Alt+K` | Next / previous session |
| `Alt+Shift+W` | Cycle to next **waiting** session |
| `Alt+Shift+A` | Cycle to next **active** session |
| `Alt+Shift+X` | Stop / resume focused session |
| `Esc` | Close modal |
| Drag | Select text in the terminal |
| `Cmd+C` / `Ctrl+Shift+C` | Copy selection to the system clipboard |
| `Cmd+V` / `Ctrl+Shift+V` | Paste from the system clipboard |
| Mousewheel | Scroll xterm scrollback (normal screen) or page through tmux pane history via copy-mode (alt-screen TUI — press `q` to exit) |

## New session

- **Name** (required) — shown in the left panel and used as the worktree folder / branch if auto-creating one.
- **Backend** — reported by the daemon (`claude` and `codex` today). Additions implement the `Backend` trait and metadata in `crates/slide-core/src/backend/`; the UI needs no new hardcoded option.
- **Base directory** (required) — a git repo. Remembered across dialogs.
- **Location** — Local or Remote (SSH host).

## State model

Every running session has its own classifier task that wakes on byte activity (via `tokio::sync::Notify`) or a per-session settle deadline. No global polling ticker.

- **Active** — bytes observed in the last ~1.5 s, or the backend's "working" regex matched the rendered pane.
- **Waiting** — bytes have settled AND the backend's prompt regex matched (or an explicit idle hint).
- **Stopped** — child process ended, or the user stopped the session. Resume spawns a fresh backend that, when supported, continues the previous conversation via `--resume`.

## Layout

```
slide/
├── crates/
│   ├── slide-core/   # Rust library: sessions, PTY, backends, git, async SQLite
│   └── slide-cli/    # Binary: `slide serve` / `slide open` / `slide token`, HTTP + WS + embedded SPA
└── web/              # React + xterm.js SPA (served by the daemon in release)
```

## Data

SQLite at `~/Library/Application Support/slide/slide.db` (macOS) / `$XDG_DATA_HOME/slide/slide.db` (Linux), accessed via `tokio-rusqlite` so daemon hot paths never block on a sync mutex. Per-session scrollback logs live at `…/slide/logs/<id>.log`. Set `SLIDE_DATA_DIR` to isolate development or test data.

## Security

- Daemon binds `127.0.0.1` only.
- Every session API and WebSocket request is authenticated (the health check is intentionally public). HTTP uses a constant-time-compared bearer token; WebSocket upgrades carry it in the `Sec-WebSocket-Protocol: slide.bearer.<token>` subprotocol because browsers can't set arbitrary handshake headers.
- The middleware also rejects requests whose `Host` (or `Origin`, when present) isn't a loopback name — DNS rebinding hardening.
- Token + port + pid live at `~/Library/Application Support/slide/daemon.lock` (macOS) / `$XDG_DATA_HOME/slide/daemon.lock` (Linux), mode 0600. In production, `slide serve` does **not** print the token-bearing URL to stdout — the auto-open path hands it straight to the browser, and `slide open` reads it from the lock file. Dev mode prints the URL because Vite's dev server (port 5173) needs to receive the token in the page load.
- `SIGINT` / `SIGTERM` trigger a graceful drain: axum finishes in-flight requests, then direct-supervised backends are killed so they don't outlive the daemon as orphans. Tmux-supervised sessions are left alive on purpose.

## License

MIT — see [LICENSE](LICENSE).
