#!/usr/bin/env bash
# Run slide in dev mode: Rust daemon + Vite dev server concurrently.
# Vite proxies /api and /ws to the daemon (see web/vite.config.ts).
#
# Usage: ./scripts/dev.sh
set -euo pipefail

for arg in "$@"; do
  case "$arg" in
    -h|--help)
      sed -n '2,5p' "$0"
      exit 0
      ;;
    *)
      echo "unknown arg: $arg" >&2
      echo "usage: $0" >&2
      exit 2
      ;;
  esac
done

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$here"

# --- Make sure cargo/node are on PATH even in non-login shells ----------------
for p in "$HOME/.cargo/bin" "/opt/homebrew/opt/rustup/bin" "/opt/homebrew/bin" "/usr/local/bin"; do
  [ -d "$p" ] && PATH="$p:$PATH"
done
export PATH

have() { command -v "$1" >/dev/null 2>&1; }

if ! have cargo; then
  echo "cargo not found on PATH. Run ./scripts/bootstrap.sh first." >&2
  exit 1
fi
if ! have npm; then
  echo "npm not found on PATH. Run ./scripts/bootstrap.sh first." >&2
  exit 1
fi

# --- Free the ports we're about to use ---------------------------------------
free_port() {
  local port="$1"
  local pids
  pids="$(lsof -ti tcp:"$port" 2>/dev/null || true)"
  if [ -n "$pids" ]; then
    echo "Killing leftover process on :$port (pids: $pids)"
    # shellcheck disable=SC2086
    kill $pids 2>/dev/null || true
    sleep 0.5
    pids="$(lsof -ti tcp:"$port" 2>/dev/null || true)"
    if [ -n "$pids" ]; then
      # shellcheck disable=SC2086
      kill -9 $pids 2>/dev/null || true
    fi
  fi
}
free_port 7777
free_port 5173

# --- Stop a stale slide daemon if its pidfile points somewhere alive ---------
lock="$HOME/Library/Application Support/slide/daemon.lock"
[ ! -f "$lock" ] && lock="$HOME/.local/share/slide/daemon.lock"
if [ -f "$lock" ]; then
  pid="$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["pid"])' "$lock" 2>/dev/null || true)"
  if [ -n "${pid:-}" ] && kill -0 "$pid" 2>/dev/null; then
    echo "Stopping previous slide daemon (pid $pid)"
    kill "$pid" 2>/dev/null || true
    sleep 0.5
  fi
fi

# --- Run both processes ------------------------------------------------------
pids=()
cleanup() {
  trap - EXIT INT TERM
  for pid in "${pids[@]}"; do
    kill "$pid" 2>/dev/null || true
  done
  wait 2>/dev/null || true
}
trap cleanup EXIT INT TERM

cargo run -p slide-cli -- serve --no-open --dev &
pids+=("$!")
(cd web && npm run dev) &
pids+=("$!")

# Portable "wait for any child" — poll both PIDs (macOS bash 3.2 lacks `wait -n`).
while :; do
  for pid in "${pids[@]}"; do
    if ! kill -0 "$pid" 2>/dev/null; then
      exit 0
    fi
  done
  sleep 1
done
