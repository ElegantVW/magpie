#!/usr/bin/env bash
# build.sh — produce release magpie; install into faeOS engine paths
set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT"
# the nose links system Xapian: fail here with a house-style error,
# not three screens of c++ template noise (or --no-default-features).
if ! pkg-config --exists xapian-core 2>/dev/null; then
  echo "note: no xapian-core via pkg-config — building pure-Rust (--no-default-features)" >&2
  export MAGPIE_NO_NOSE=1
  NOSE_FLAGS="--no-default-features"
else
  NOSE_FLAGS=""
fi
# -j1: parallel release trips rustc/ring SIGABRT on this box; single job is green
# shellcheck disable=SC2086
cargo build --release -j1 $NOSE_FLAGS
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
