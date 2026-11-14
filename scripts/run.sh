#!/usr/bin/env bash
# Build and start Qorral from this checkout (macOS, Linux).
#   scripts/run.sh [--release] [--pull] [--trace] [app options]
set -euo pipefail
cd "$(dirname "$0")/.."
pull=0 release=() args=()
for arg in "$@"; do
  case "$arg" in
    --pull) pull=1 ;;
    --release) release=(--release) ;;
    --trace) export QORRAL_TRACE=1 ;;
    *) args+=("$arg") ;;
  esac
done
if [ "$pull" = 1 ]; then git pull --ff-only origin main; fi
cpus=$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 2)
jobs=$(( cpus / 2 > 0 ? cpus / 2 : 1 )); [ "$jobs" -gt 8 ] && jobs=8
cargo build "${release[@]}" -j "$jobs" --quiet -p qorral
exec cargo run "${release[@]}" -j "$jobs" --quiet -p qorral --bin qorral-gui -- "${args[@]}"
