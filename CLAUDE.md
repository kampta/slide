# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

First-time setup: `./scripts/bootstrap.sh` (installs rustup toolchain pinned by `rust-toolchain.toml`, Node from `.nvmrc`, and `web/` deps).

Day-to-day dev: `./scripts/dev.sh` runs `cargo run -p slide-cli -- serve --no-open --dev` alongside the Vite dev server. Vite (`:5173`) proxies `/api` and `/ws` to the daemon (`:7777`) — see `web/vite.config.ts`. The script frees both ports and kills any stale daemon via its pidfile before launching. In dev mode the daemon intentionally prints the full `http://localhost:5173/?token=…` URL on stdout so a developer can click it; production mode prints only the bare URL and either auto-opens the browser (default) or expects `slide open` (manual).

Other CLI subcommands:

- `slide serve` — start the daemon (default).
- `slide open` — launch the browser at the running daemon, with the bootstrap token injected via `?token=…` from the lock file. Never prints the token-bearing URL.
- `slide token` — print just the bootstrap token to stdout. Use sparingly — it explicitly writes the token to the terminal.

Release build (single binary with embedded SPA via `rust-embed`):
```bash
(cd web && npm run build)          # must precede cargo build — embeds web/dist
cargo build --release -p slide-cli
```

Test / lint — mirror CI exactly (see `.github/workflows/ci.yml`):
```bash
cargo fmt --all -- --check
npm --prefix web run build              # tsc -b --noEmit + vite build
npm --prefix web run test               # vitest
cargo clippy --all-targets --all-features -- -D warnings
cargo test -p slide-core --lib          # smoke scope used in PR CI
cargo test -p slide-cli --bin slide server::tests
```

Single test: `cargo test -p slide-core --lib session::manager::tests::test_name` or `npm --prefix web run test -- Terminal.test.tsx`.

CI intentionally runs a **smoke subset** of Rust tests (`slide-core --lib` + `slide-cli server::tests`); broader `cargo test --workspace` is fine locally but not required for PRs.

## Architecture

**Two processes that ship as one binary.** `slide-cli` is a thin Tokio/Axum daemon that owns all state; `web/` is a React+xterm.js SPA. In dev they run separately with Vite proxying; in release, `crates/slide-cli/src/assets.rs` uses `rust-embed` to serve `web/dist` from the daemon itself — so `web/dist` **must exist before `cargo build --release`** or the binary ships without a UI.

**Auth model is load-bearing.** Daemon binds `127.0.0.1` only and writes a per-process random token plus port + pid to `~/Library/Application Support/slide/daemon.lock` (macOS) or `$XDG_DATA_HOME/slide/daemon.lock` (Linux), mode 0600. Auth is centrally enforced in `crates/slide-cli/src/server.rs::auth_layer` — a Tower middleware applied once to the protected router, so a new route can't accidentally skip the check. HTTP requests use `Authorization: Bearer <token>`; WebSocket upgrades ride the `Sec-WebSocket-Protocol: slide.bearer.<token>` subprotocol because browsers can't set arbitrary headers on the WS handshake. The `auth_layer` also rejects requests whose `Host` (or `Origin`, when present) isn't a loopback name — DNS rebinding hardening. The `slide-cli server::tests` CI job exists specifically to protect both checks.

**Token bootstrap.** The browser receives the token via `?token=…` once at first load and stashes it in `sessionStorage`; subsequent reloads reuse the stored value. The daemon never accepts `?token=` for actual auth (only as a one-shot SPA bootstrap mechanism); `server.rs::query_token_is_no_longer_accepted` is the regression test for this.

**Session lifecycle lives in `slide-core`.** `SessionManager` (`crates/slide-core/src/session/manager.rs`) owns the map of sessions; `session/pty.rs` wraps `portable-pty` for the child process. Each running session spawns its own classifier task that reacts to byte arrivals via a `tokio::sync::Notify` ping from the reader task — there is no global polling ticker. The classifier captures the rendered pane (tmux `capture-pane` for tmux-supervised sessions, ANSI-stripped ring tail for direct-PTY) and runs the pure `classifier::classify` function, which produces one of:

- **Active** — bytes observed in the last `signals.settle_ms` (~1.5 s) OR a backend "working" regex matched on the pane.
- **Waiting** — a `needs_input` approval/authentication pattern matched, or a settled prompt/`idle_hints` pattern matched.
- **Unknown** — the backend is running but the settled pane has no reliable signal. Unknown panes and capture failures are periodically rechecked with bounded backoff.
- **Stopped** — child process ended OR user explicitly stopped the session. (No separate "Exited" / "Archived" — both collapsed into Stopped.)

Prompt regexes are per-backend, defined in the `Backend` trait (`crates/slide-core/src/backend/mod.rs`) and implemented in one file per CLI under `backend/`. **Adding a new backend = new file under `backend/` implementing that trait**, then wiring it into the dispatch in `mod.rs`. The UI's backend dropdown is driven by what the daemon reports, not hardcoded in the SPA.

**Worktree auto-creation.** When a new session is created without a `project_path`, `slide-core::git` runs `git worktree add <base>/.slide-worktrees/<name> -b slide/<name>` and the backend is spawned there. This is why concurrent sessions on the same repo don't stomp each other — each gets its own branch + working directory. Cold-start reconciliation in `SessionManager::new` marks a session Stopped if its local `project_path` no longer exists. Note: this repo itself lives under `.slide-worktrees/<name>` during dogfooding.

**Persistence.** SQLite at `<data_dir>/slide/slide.db`, accessed asynchronously via `tokio_rusqlite::Connection` — the connection owns a single background thread and a command queue, so `Store` methods are `async fn` and never hold a sync lock across an await boundary. Per-session scrollback logs at `<data_dir>/slide/logs/<id>.log`. All schema/migrations go through `crates/slide-core/src/store.rs`.

**Lifecycle.** `slide serve` registers `SIGINT` and (on Unix) `SIGTERM` handlers. On signal: axum drains in-flight HTTP, then `SessionManager::shutdown` kills direct-supervised children so they don't outlive us as orphans. Tmux-supervised sessions are deliberately left alive and reattached on the next daemon start. Because reattachment does not relaunch the backend, changed launch flags take effect on an existing tmux session only after Stop + Resume.

**Frontend state.** Zustand store in `web/src/state/sessionStore.ts` is the single source of truth for the React tree; `state/api.ts` is the fetch/WS client and is the only module that should touch `/api` or `/ws` directly. `components/Terminal.tsx` wires xterm.js to a WS per session — data flows daemon → WS → terminal with no intermediate buffering in React state (too expensive). The xterm instance survives live↔Stopped transitions; only the live WebSocket comes and goes. State dots and live/stopped grouping are derived from the enum the daemon reports; the frontend does not compute state itself.

**Terminal keyboard handling** lives in `Terminal.tsx::attachCustomKeyEventHandler` because xterm captures keys before React. Keep this limited to standard terminal behavior such as platform clipboard shortcuts; the app currently has no global hotkey system.

**Terminal mouse handling** is split across two layers because xterm.js's selection layer goes dormant whenever the inner terminal is in mouse-tracking mode. The daemon side keeps tmux mouse mode **on** (so wheel-up enters tmux's `copy-mode -e` for pane-history scrollback inside alt-screen agent TUIs) and unbinds tmux's `MouseDrag1Pane`/`MouseDragEnd1Pane` so drag isn't consumed (`crates/slide-core/src/tmux.rs::setup_mouse_argv`). The frontend then runs its own DOM-level drag-to-select in `Terminal.tsx`, computing cell coordinates from `.xterm-screen` bounds and calling `term.select(col, row, length)` in parallel with the click escapes xterm forwards to tmux. Click events still reach tmux harmlessly; wheel still scrolls; native drag works without modifier keys.
