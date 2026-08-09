#!/usr/bin/env bash
# Bootstrap slide on a fresh machine (macOS or Linux).
# Installs rustup (if missing), the toolchain pinned by rust-toolchain.toml,
# Node (via fnm if available, else via the system package manager), and JS deps.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$here"

log() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
have() { command -v "$1" >/dev/null 2>&1; }

# --- Rust -------------------------------------------------------------------
if ! have rustup; then
  log "Installing rustup"
  if have brew; then
    brew install rustup-init
    export PATH="$(brew --prefix rustup)/bin:$PATH"
    rustup-init -y --default-toolchain none
  else
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none
    export PATH="$HOME/.cargo/bin:$PATH"
  fi
fi

log "Syncing Rust toolchain from rust-toolchain.toml"
rustup show active-toolchain >/dev/null 2>&1 || rustup toolchain install stable

# Make sure cargo is reachable in this shell even if rustup is keg-only in brew.
for p in "$HOME/.cargo/bin" "/opt/homebrew/opt/rustup/bin"; do
  [ -d "$p" ] && export PATH="$p:$PATH"
done
cargo --version

# --- Node -------------------------------------------------------------------
node_version="$(cat .nvmrc)"
if ! have node; then
  log "Installing Node $node_version"
  if have fnm; then
    fnm install "$node_version"
  elif have nvm; then
    # shellcheck disable=SC1091
    . "$(brew --prefix nvm)/nvm.sh" 2>/dev/null || true
    nvm install "$node_version"
  elif have brew; then
    brew install "node@$node_version" || brew install node
  else
    echo "No node installer found. Install fnm, nvm, or brew first." >&2
    exit 1
  fi
fi
node --version

# --- JS deps ----------------------------------------------------------------
log "Installing web dependencies"
(cd web && npm install)

log "Bootstrap complete. Run: scripts/dev.sh"
