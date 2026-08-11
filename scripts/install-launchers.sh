#!/usr/bin/env bash
# Point user launchers at the Tauri release binary (run after deno task tauri:build).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release/brickadia-world-tools"
LEGACY="$ROOT/target/release/heightmap_gui"
if [[ ! -x "$BIN" ]]; then
  echo "missing $BIN — run: cd apps/desktop && deno task tauri:build" >&2
  exit 1
fi
ln -sfn "$BIN" "$HOME/.local/bin/brickadia-world-tools"
ln -sfn "$BIN" "$HOME/.local/bin/brickadia-world-tools-gui"
ln -sfn "$BIN" "$HOME/.local/bin/heightmap2brz-gui"
if [[ -x "$LEGACY" ]]; then
  ln -sfn "$LEGACY" "$HOME/.local/bin/brickadia-world-tools-legacy-egui"
fi
echo "Installed primary: $BIN"
ls -la "$HOME/.local/bin/brickadia-world-tools" "$HOME/.local/bin/brickadia-world-tools-gui"
