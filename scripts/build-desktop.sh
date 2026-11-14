#!/usr/bin/env bash
set -euo pipefail
SCRIPT_PATH="${BASH_SOURCE[0]}"
if [ "${SCRIPT_PATH#*/}" = "$SCRIPT_PATH" ]; then
    SCRIPT_PATH="./$SCRIPT_PATH"
fi
SCRIPT_DIR="${SCRIPT_PATH%/*}"
ROOT="$SCRIPT_DIR/.."
APP="$ROOT/apps/harness-app"
if ! command -v pnpm >/dev/null 2>&1; then
    printf 'error: required command pnpm was not found on PATH\n' >&2
    exit 1
fi
if [ ! -d "$APP/node_modules" ]; then
    printf 'error: frontend dependencies are missing; run: pnpm --dir apps/harness-app install\n' >&2
    exit 1
fi
cd "$APP"
exec pnpm exec tauri build "$@"
