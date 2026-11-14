# Harness Platform Guide
Harness is the Rust + Tauri v2 desktop shell for ZCode. Desktop builds run the
Harness shell and host local workspaces. Android and iOS builds are remote-control
clients only: they attach to a desktop Host and never start a local Host, agent,
PTY, or workspace process.
## Support Matrix
| Target | WebView engine | Local Host | Remote-control client |
| --- | --- | --- | --- |
| Windows 10/11 | WebView2 (Chromium) | Yes | Yes |
| macOS | WKWebView | Yes | Yes |
| Ubuntu 22.04/24.04 | WebKitGTK 4.1 | Yes | Yes |
| Android | Android WebView | No | Yes |
| iOS | WKWebView | No | Yes |
All desktop targets use the shared React UI from `@zcode/ui`. The Tauri shell is a
composition root for windows, native adapters, and process supervision; it does not
own task, session, queue, or projection state.
## Common Toolchain
- Node.js 24
- pnpm 10
- Rust 1.80 or newer; use the repository `mise.toml` when present
- Tauri v2 CLI from the `@tauri-apps/cli` dev dependency in `apps/harness-app`
- Platform WebView prerequisites listed below
Install frontend dependencies once from `Worse/`:
```sh
pnpm --dir apps/harness-app install
```
## Windows
Required:
- Windows 10 version 1809 or newer, or Windows 11
- Microsoft Edge WebView2 Evergreen Runtime
- Visual Studio Build Tools with **Desktop development with C++**
- Windows 10/11 SDK
- Rust MSVC toolchain (`stable-x86_64-pc-windows-msvc`)
Verify the native toolchain:
```powershell
rustup show
rustc --version
cargo --version
```
Run from `Worse/`:
```powershell
powershell -ExecutionPolicy Bypass -File scripts/check.ps1
powershell -ExecutionPolicy Bypass -File scripts/dev.ps1
powershell -ExecutionPolicy Bypass -File scripts/build-desktop.ps1
```
`check.ps1` runs formatting, Clippy with warnings denied, the Rust workspace tests,
and the frontend typecheck.
## macOS
Required:
- macOS 10.15 or newer
- Xcode Command Line Tools or full Xcode
- Rust toolchain for the host architecture
```sh
xcode-select --install
rustup target add aarch64-apple-darwin x86_64-apple-darwin
```
Run from `Worse/`:
```sh
bash scripts/check.sh
bash scripts/dev.sh
bash scripts/build-desktop.sh
```
WKWebView is provided by macOS; no separate runtime installation is required.
Release signing and notarization are separate release tasks, not development
prerequisites.
## Ubuntu 22.04 and 24.04
Install the Tauri v2 WebKitGTK and desktop dependencies:
```sh
sudo apt update
sudo apt install -y \
  libwebkit2gtk-4.1-dev \
  build-essential \
  curl \
  wget \
  file \
  libxdo-dev \
  libssl-dev \
  libayatana-appindicator3-dev \
  librsvg2-dev
```
Also install Rust, Node.js 24, and pnpm using the team's normal toolchain setup.
Harness requires WebKitGTK 4.1; do not substitute the older `libwebkit2gtk-4.0-dev`
package.
Run from `Worse/`:
```sh
bash scripts/check.sh
bash scripts/dev.sh
bash scripts/build-desktop.sh
```
Builds on minimal containers or headless CI need the same development packages and
a display server for interactive runs. Use a virtual display only for automated UI
tests, not as a substitute for the desktop acceptance matrix.
## Android Remote Client
Required:
- JDK 17
- Android Studio or the Android command-line SDK
- Android platform-tools
- Android SDK platform matching the generated Tauri project, normally API 34
- Android NDK, normally `26.1.10909125`
- `ANDROID_SDK_ROOT` or `ANDROID_HOME`
- `ANDROID_NDK_HOME` (or an NDK directory under `$ANDROID_SDK_ROOT/ndk`)
- Rust targets for every supported ABI
```sh
rustup target add \
  aarch64-linux-android \
  armv7-linux-androideabi \
  i686-linux-android \
  x86_64-linux-android
```
Initialize and build from `Worse/`:
```sh
bash scripts/mobile-init.sh android
bash scripts/mobile-build.sh android --apk
bash scripts/mobile-build.sh android --aab
```
The scripts check the SDK, NDK, Java, frontend dependencies, and Rust targets before
calling the Tauri CLI. They do not run automatically and they do not create a local
Host or agent.
## iOS Remote Client
Required:
- macOS with Xcode and command line tools
- Apple signing identity and provisioning profile for device builds
- Rust targets for device and simulator
```sh
rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios
```
Initialize and build from `Worse/`:
```sh
bash scripts/mobile-init.sh ios
bash scripts/mobile-build.sh ios --target aarch64-apple-ios
```
Open the generated Xcode project to set the development team and signing settings
when the Tauri template requires them. iOS builds cannot run on Windows or Linux.
## Runtime Semantics
- Desktop uses the `desktop-continuous` realtime profile.
- Mobile uses the `web-remote-replayable` profile with snapshot and gap recovery.
- Mobile attaches to an existing desktop Host and reuses its Agent sessions.
- Closing a phone attachment does not shut down the desktop Host or agent.
- Renderer reload reattaches to the surviving Host; it must not kill the Host.
- Only real window close, app quit, or update installation enters Host shutdown.
## WebView Engine Differences
Windows uses Chromium-based WebView2, macOS and iOS use WKWebView, and Linux and
Android use WebKit variants. Verify these areas on every target:
- **Layout**: `100vh`, safe-area insets, virtual keyboard resizing, and mobile
  orientation changes.
- **Scrolling**: overscroll behavior, scrollbar styling, sticky headers, and nested
  scroll containers.
- **Input**: IME composition, clipboard permissions, context menus, touch targets,
  and keyboard shortcuts.
- **Rendering**: `backdrop-filter`, `color-mix()`, container queries, subgrid, masks,
  and large diff or virtualized lists.
- **Workers**: the diff worker pool and workspace file-search worker must initialize
  under WebKitGTK and Android WebView.
- **Window chrome**: Windows and macOS drag regions and window controls differ from
  Electron. Keep interactive controls outside drag regions.
- **Native integration**: use `IPlatformService`; never call Electron or
  `window.zcode` from the webview.
- **Capabilities**: mobile exposes external links and notifications only. Path reveal,
  directory selection, file selection, and local Host access are disabled.
- **Notifications**: permission prompts and delivery behavior are owned by the OS and
  must be tested separately on each mobile platform.
- **External URLs**: only `http`, `https`, and `mailto` are accepted by the platform
  adapter.
## Verification
Before handing off a desktop change, run:
```sh
bash scripts/check.sh
```
On Windows:
```powershell
powershell -ExecutionPolicy Bypass -File scripts/check.ps1
```
For release builds, verify the installer or bundle from
`target/release/bundle/` on the target OS. Mobile builds must be tested against an
existing desktop Host; a mobile app that starts its own Host or Agent is invalid.