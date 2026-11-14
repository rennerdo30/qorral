#!/usr/bin/env bash
# Render the real Qorral UI offscreen to artifacts/snapshot.png (default: --demo).
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p artifacts
args=("$@"); [ ${#args[@]} -eq 0 ] && args=(--demo)
cargo run --release --quiet -p qorral --bin qorral-gui -- --snapshot "$PWD/artifacts/snapshot.png" "${args[@]}"
echo "Saved $PWD/artifacts/snapshot.png"
