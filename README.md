# slide

A lightweight IDE for juggling Claude Code, Codex, Grok, Google Antigravity, and OpenCode sessions at once. The session list keeps live work first and stopped work collapsed. Each local session runs in an auto-created git worktree so concurrent agents don't stomp on each other.

Architecture: a small Rust daemon (`slide serve`) hosts an HTTP+WebSocket API and serves a React SPA. Open it in any browser.

## Setup

On a fresh macOS or Linux machine:

```bash
git clone https://github.com/kampta/slide
cd slide
./scripts/bootstrap.sh
```

The bootstrap script installs the pinned Rust toolchain, Node.js, and the web dependencies. Run it once per machine.

Slide launches agent CLIs already installed on the daemon host. Install and authenticate whichever backends you plan to use: `claude`, `codex`, `grok`, Antigravity's `agy`, or `opencode`. For remote sessions, install the selected CLI on the remote host.

## Development mode

```bash
./scripts/dev.sh
```

This starts the Rust daemon on `127.0.0.1:7777` and the Vite development server on `127.0.0.1:5173`. Vite serves the UI with hot reload and proxies `/api` and `/ws` to the daemon. Open the token-bearing `http://localhost:5173/?token=…` URL printed in the terminal. Stop both processes with `Ctrl+C`.

To test from another device on a trusted local network:

```bash
./scripts/dev.sh --lan
```

This exposes both development servers on the network, including the token-bearing URL, so do not use it on an untrusted network.

## Production mode

Build the web app first, then the release binary:

```bash
npm --prefix web run build
cargo build --release -p slide-cli
```

The release binary embeds `web/dist`, so production runs as one process with no Node.js or Vite server:

```bash
./target/release/slide serve
```

It serves the embedded UI and API on `http://127.0.0.1:7777` and opens the authenticated page in your browser without printing the token-bearing URL. To start without opening a tab, add `--no-open`; while the daemon is running, `./target/release/slide open` opens it later.

For access from another device, run `./target/release/slide serve --lan`, then use `./target/release/slide pair` to print the pairing URLs and QR codes. Only expose Slide on a trusted network.

Re-run both build commands whenever the frontend changes. Rust-only changes require only the Cargo build.

Restarting the Slide daemon reattaches existing tmux-backed agents; it does not relaunch them. After changing backend launch options, use Stop and then Resume on an existing session to start that backend with the new options.

## Terminal interaction

The terminal uses standard terminal controls. Drag to select text, use the platform clipboard keys to copy or paste, and use the mouse wheel for scrollback. In an alternate-screen TUI, the mouse wheel enters tmux copy mode; press `q` to leave it.

## Subagent dock

When a backend exposes structured child-agent metadata, Slide shows a collapsible dock above the terminal with each descendant's name, role, state, hierarchy, and elapsed time. Codex sessions use the CLI's app-server metadata API; the snapshot is bounded and excludes prompts, tool arguments, command output, and transcript paths. Unsupported backends continue to render as ordinary terminal sessions with no empty dock.

## Changes by turn

Slide snapshots the complete Git worktree when an agent turn starts and records the incremental patch when the session returns to Waiting (or stops). The collapsible Changes dock shows per-turn file/addition/deletion counts and loads a patch only when selected. Snapshots use a private temporary Git index, so staged work is never modified; local and SSH sessions share the same behavior. History is bounded to 50 turns per session and each stored patch to 256 KiB.

## Runtime diagnostics

Open Runtime diagnostics from the diamond button beside **New** to check every backend CLI, authentication state, version, and tmux capability on the local machine or a configured SSH host. Probes mirror Slide's launch environment, are cached for 60 seconds, and never return command output or account identity. Session creation and resume reuse the same snapshot; creation fails before making a worktree when the selected runtime—or remote tmux—is unavailable.

## History search

Open Search beside **New** to find text across the persisted terminal output of every local and SSH session, including stopped sessions. Searches stream local logs with fixed memory and group remote logs into one bounded SSH command per host. Results retain only the newest distinct snippets, strip terminal control sequences, and open the matching session when selected.

## New session

- **Name** (required) — shown in the left panel and used as the worktree folder / branch if auto-creating one.
- **Backend** — reported by the daemon rather than hardcoded in the dialog. Current choices are Claude, Codex, Grok, Antigravity, and OpenCode.
- **Base directory** (required) — a git repo. Remembered across dialogs.
- **Location** — Local or Remote (SSH host).

## State model

Every running session has its own classifier task that wakes on byte activity (via `tokio::sync::Notify`) or a per-session deadline. Unknown states use a bounded retry timer; there is no global polling ticker.

- **Active** — bytes observed in the last ~1.5 s, or the backend's "working" regex matched the rendered pane.
- **Waiting** — an approval/authentication modal, explicit idle hint, or settled prompt matched.
- **Unknown** — the backend is running, but the settled pane has no reliable working or input signal. Slide periodically rechecks it.
- **Stopped** — child process ended, or the user stopped the session. Resume continues the prior backend conversation when Slide has discovered its native conversation ID; otherwise it starts fresh.

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
