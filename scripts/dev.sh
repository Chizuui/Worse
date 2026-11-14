#!/usr/bin/env bash
set -euo pipefail
SCRIPT_PATH="${BASH_SOURCE[0]}"
if [ "${SCRIPT_PATH#*/}" = "$SCRIPT_PATH" ]; then
    SCRIPT_PATH="./$SCRIPT_PATH"
fi
SCRIPT_DIR="${SCRIPT_PATH%/*}"
ROOT="$SCRIPT_DIR/.."
BUILD_ONLY=0
TAURI_ARGS=()
for arg in "$@"; do
    case "$arg" in
        --build-only) BUILD_ONLY=1 ;;
        *) TAURI_ARGS+=("$arg") ;;
    esac
done
if ! command -v pnpm >/dev/null 2>&1; then
    printf 'error: required command pnpm was not found on PATH\n' >&2
    exit 1
fi
if ! command -v node >/dev/null 2>&1; then
    printf 'error: required command node was not found on PATH\n' >&2
    exit 1
fi
node "$SCRIPT_DIR/build-host-sidecar.mjs"
if [ "$BUILD_ONLY" = "1" ] || [ "${HARNESS_BUILD_ONLY:-0}" = "1" ]; then
    exit 0
fi
cd "$ROOT"
exec pnpm --filter @harness/app exec tauri dev ${TAURI_ARGS[@]+"${TAURI_ARGS[@]}"}