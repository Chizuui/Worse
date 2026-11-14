#!/usr/bin/env bash
# Shared prerequisite checks for the Harness mobile scripts. Source only.
HARNESS_SCRIPT_PATH="${BASH_SOURCE[0]}"
if [ "${HARNESS_SCRIPT_PATH#*/}" = "$HARNESS_SCRIPT_PATH" ]; then
    HARNESS_SCRIPT_PATH="./$HARNESS_SCRIPT_PATH"
fi
HARNESS_SCRIPT_DIR="${HARNESS_SCRIPT_PATH%/*}"
HARNESS_ROOT="$HARNESS_SCRIPT_DIR/.."
HARNESS_APP="$HARNESS_ROOT/apps/harness-app"
mobile_fail() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}
mobile_require_command() {
    if ! command -v "$1" >/dev/null 2>&1; then
        mobile_fail "required command '$1' was not found on PATH"
    fi
}
mobile_require_base() {
    mobile_require_command pnpm
    mobile_require_command cargo
    mobile_require_command rustup
    if [ ! -d "$HARNESS_APP/node_modules" ]; then
        mobile_fail 'frontend dependencies are missing; run: pnpm --dir apps/harness-app install'
    fi
}
mobile_require_rust_targets() {
    local installed install_list missing=() target
    installed="$(rustup target list --installed)"
    for target in "$@"; do
        if ! printf '%s\n' "$installed" | grep -qxF "$target"; then
            missing+=("$target")
        fi
    done
    if [ ${#missing[@]} -gt 0 ]; then
        install_list="$(printf '%s ' "${missing[@]}")"
        mobile_fail "missing Rust targets: ${missing[*]}; install with: rustup target add ${install_list% }"
    fi
}
mobile_require_android() {
    local sdk ndk ndk_candidates
    sdk="${ANDROID_SDK_ROOT:-${ANDROID_HOME:-}}"
    [ -n "$sdk" ] || mobile_fail 'set ANDROID_SDK_ROOT or ANDROID_HOME to the Android SDK directory'
    [ -d "$sdk/platform-tools" ] || mobile_fail "Android platform-tools not found under $sdk"
    ndk="${ANDROID_NDK_HOME:-${NDK_HOME:-}}"
    if [ -z "$ndk" ]; then
        ndk_candidates=("$sdk"/ndk/*)
        if [ -d "${ndk_candidates[0]}" ]; then
            ndk="${ndk_candidates[0]}"
        fi
    fi
    [ -n "$ndk" ] && [ -d "$ndk" ] || mobile_fail 'Android NDK not found; set ANDROID_NDK_HOME or install an NDK under $ANDROID_SDK_ROOT/ndk'
    mobile_require_command java
    java -version >/dev/null 2>&1 || mobile_fail 'java is installed but not runnable'
    mobile_require_rust_targets \
        aarch64-linux-android \
        armv7-linux-androideabi \
        i686-linux-android \
        x86_64-linux-android
}
mobile_require_ios() {
    [ "$(uname -s)" = 'Darwin' ] || mobile_fail 'iOS builds require macOS with Xcode'
    mobile_require_command xcode-select
    mobile_require_command xcodebuild
    xcode-select -p >/dev/null 2>&1 || mobile_fail 'Xcode command line tools are not selected; run: sudo xcode-select --switch /Applications/Xcode.app/Contents/Developer'
    xcodebuild -version >/dev/null 2>&1 || mobile_fail 'xcodebuild is not usable; install or repair Xcode'
    mobile_require_rust_targets aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios
}
