#!/usr/bin/env bash
# build.sh — produce release magpie; install into faeOS engine paths
set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT"
# -j1: parallel release trips rustc/ring SIGABRT on this box; single job is green
cargo build --release -j1
BIN="$ROOT/target/release/magpie"
echo "built: $BIN"
ls -la "$BIN"

if [[ "${1:-}" == "install" ]]; then
  LIB="$HOME/.local/lib/faeos"
  WRAP_SRC="$ROOT/scripts/magpie"
  mkdir -p "$LIB" "$HOME/bin"
  cp -f "$BIN" "$LIB/magpie"
  chmod +x "$LIB/magpie"

  install_launcher() {
    local dest="$1"
    mkdir -p "$(dirname "$dest")"
    cp -f "$WRAP_SRC" "$dest"
    chmod +x "$dest"
  }

  install_launcher "$HOME/bin/magpie"
  for kit in "$HOME/faeOS/bin" "$HOME/faeos/bin"; do
    if [[ -d "$kit" ]]; then
      install_launcher "$kit/magpie"
    fi
  done

  echo "installed engine → $LIB/magpie"
  echo "launcher        → $HOME/bin/magpie"
fi
