#!/usr/bin/env bash
set -euo pipefail
SCRIPT_PATH="${BASH_SOURCE[0]}"
if [ "${SCRIPT_PATH#*/}" = "$SCRIPT_PATH" ]; then
    SCRIPT_PATH="./$SCRIPT_PATH"
fi
SCRIPT_DIR="${SCRIPT_PATH%/*}"
# shellcheck source=mobile-common.sh
source "$SCRIPT_DIR/mobile-common.sh"
usage() {
    cat <<'EOF'
Usage: mobile-init.sh <android|ios> [tauri android/ios init options]
Checks the platform toolchain, then initializes the Harness mobile project.
This script does not start a local Host, agent, or PTY on the device.
EOF
}
if (($# < 1)); then
    usage
    exit 2
fi
platform="$1"
shift
mobile_require_base
case "$platform" in
android)
    mobile_require_android
    cd "$HARNESS_APP"
    exec pnpm exec tauri android init "$@"
    ;;
ios)
    mobile_require_ios
    cd "$HARNESS_APP"
    exec pnpm exec tauri ios init "$@"
    ;;
*)
    usage
    mobile_fail "unknown mobile platform: $platform"
    ;;
esac
