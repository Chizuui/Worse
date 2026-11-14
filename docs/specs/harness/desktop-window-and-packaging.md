# Desktop window chrome and packaged Host layout

Status: active implementation contract (fixes after Phase 3a). Owner: Tauri shell
(`Worse/apps/harness-app/src-tauri`). This spec covers two shell responsibilities that Phase 2 left
open: the frameless window that matches ZCode's Electron window, and where a packaged build finds
the Node Host sidecar, the ZCode Host bundle, the agent CLI, and the Node.js runtime.

## 1. Frameless window

### 1.1 Product rules

- The window shows exactly one set of window controls. ZCode's renderer draws its own controls on
  Windows and Linux, so the native title bar is removed there. On macOS the native traffic lights
  stay and the title bar is hidden, matching `buildDesktopWindowVisualOptions()` in
  `packages/desktop/src/main/desktopWindowChrome.ts` (`titleBarStyle: "hidden"`,
  `trafficLightPosition {22, 23}`).
- The window stays resizable from every edge. Minimum size matches ZCode
  (`MIN_DESKTOP_WINDOW_WIDTH` 480, `MIN_DESKTOP_WINDOW_HEIGHT` 640).
- Closing the window, from the native close or from the renderer's close button, always runs the
  Host shutdown barrier before the process exits.

### 1.2 Configuration (source of truth: the config files)

| File | Applies to | Window settings |
| --- | --- | --- |
| `tauri.conf.json` | all targets (mobile ignores window chrome) | `decorations: false`, `shadow: true`, `resizable: true`, 1280x800, min 480x640 |
| `tauri.macos.conf.json` | macOS (JSON merge patch; replaces the `windows` array) | `decorations: true`, `titleBarStyle: "Overlay"`, `hiddenTitle: true`, `trafficLightPosition {x:22, y:23}`, same sizes |
| `tauri.windows.conf.json`, `tauri.linux.conf.json` | Windows, Linux | no window override; only `bundle.resources` (section 2) |

On Windows and Linux, `tauri-runtime-wry` attaches its undecorated-resize handler to a
`decorations: false` + `resizable: true` window, so edge resizing keeps working. `shadow: true`
gives the Windows 10/11 undecorated window its system shadow and 1px border.

### 1.3 Frontend contract

The frontend drives controls through `getCurrentWindow()` from `@tauri-apps/api/window` (inside
`src/platform/`, per the architecture rules). `capabilities/desktop.json` grants exactly:

| Permission | Used by |
| --- | --- |
| `core:default` | includes `core:window:allow-is-maximized`, `core:window:allow-internal-toggle-maximize` and `core:event:default` (`listen`/`unlisten`), so `isMaximized()`, `onResized()` and the maximize-state listener need nothing more |
| `core:window:allow-minimize` | `minimize()` |
| `core:window:allow-maximize` | `maximize()` |
| `core:window:allow-unmaximize` | `unmaximize()` |
| `core:window:allow-toggle-maximize` | `toggleMaximize()` and double-click on `data-tauri-drag-region` |
| `core:window:allow-close` | `close()` |
| `core:window:allow-start-dragging` | `startDragging()` and `data-tauri-drag-region` |
| `core:window:allow-set-focus` | `setFocus()` |

`capabilities/mobile.json` is unchanged: mobile has no window controls.

### 1.4 Close event order

`Window.close()` invokes `plugin:window|close`, which calls `tauri::Window::close()`. That emits
`WindowEvent::CloseRequested` exactly like the native close button (tauri 2.12
`window/mod.rs` and `tauri-runtime-wry` `on_close_requested`). `Window.destroy()` skips the event
and must not be used for the close button.

```mermaid
sequenceDiagram
  participant UI as Renderer close button
  participant W as Tauri window
  participant S as Shell (lib.rs on_window_event)
  participant H as HostRuntime
  participant N as Node sidecar
  UI->>W: getCurrentWindow().close()
  W->>S: WindowEvent::CloseRequested
  S->>S: api.prevent_close(); begin_exit() guards re-entry
  S->>H: shutdown() (async task)
  H->>N: {"type":"dispose"}, close stdin, grace, kill tree
  N-->>H: exited
  S->>W: app.exit(0)
```

The frontend must not register an `onCloseRequested` handler that calls `destroy()`: the shell
owns the close barrier.

## 2. Packaged Host layout

### 2.1 One layout, two roots

Every Host launch file sits at the same path relative to a *layout root*:

| Layout-relative path | Content |
| --- | --- |
| `Worse/sidecars/host-node/dist/main.js` | sidecar launcher (+ `package.json` with `"type": "module"`) |
| `Worse/sidecars/rust-bridge/dist/` | Rust service bridge |
| `Worse/target/release/harness-host(.exe)` | Rust service host (rust-bridge resolves `../../target/release` from its package) |
| `packages/desktop/out/host/index.js` | ZCode Host bundle and chunks |
| `apps/zcode-cli/packages/cli/dist/zcode.cjs` | agent CLI (self-contained bundle, as ZCode ships it) |
| `config/provider/zcode-builtin{,.test}.json` | built-in provider config |
| `node_modules/` | runtime npm dependencies of the staged ESM files |

The root is chosen once at shell start by `src-tauri/src/host/layout.rs`:

1. `HARNESS_REPO_ROOT` if set (a checkout or any staged directory).
2. Release build (`!debug_assertions`): `resource_dir()/host`, with the Windows `\\?\` prefix
   removed so Node and its children get a normal cwd.
3. Debug build: the checkout the binary was compiled from.

`HARNESS_HOST_ENTRY` and `ZCODE_BUILTIN_PROVIDER_CONFIG_FILE` still override single files. The Host
runs with cwd = layout root, so its own `findUpward("apps/zcode-cli/packages/cli/dist/zcode.cjs")`
finds the CLI in both layouts and `process.versions.electron` stays unset.

A missing launcher or Host entry is `hostSpawnFailed`. In a dev layout the message names the build
command. In a packaged layout it says the bundle is incomplete and names the staging command.

### 2.2 Staging

`node Worse/scripts/stage-sidecar-resources.mjs [--out <dir>]` copies build outputs (it never
builds) into `src-tauri/resources/host/`, which is gitignored. `tauri.{windows,macos,linux}.conf.json`
bundle it as the `host/` resource (`"resources/host/": "host/"`). Android and iOS do not merge these
files, so mobile packages contain no Host. `build.rs` creates the directory empty when it is
missing, so a plain `cargo build` does not depend on staging.

The script:

- fails, naming the build command, when an input is missing (sidecar, rust-bridge, Host bundle,
  CLI, provider config, release `harness-host`);
- skips `*.map`, `*.d.ts`, `*.tsbuildinfo` and `*.pdb`;
- scans the staged ESM files for bare imports. Static imports are required. Dynamic `import()`
  and `require()` are staged when they resolve and reported otherwise. The script copies each
  package and its `dependencies` and `optionalDependencies`, dereferencing pnpm symlinks, into
  the root `node_modules/`. A second version of the same package goes under its requester;
- excludes the same build-only packages as `packages/desktop/electron-builder.config.js`
  (`nan`, `node-addon-api`, `node-gyp`, `cpu-features`, `buildcheck`, `@types/*`) and keeps only
  the build host's `prebuilds/<platform>-<arch>` (node-pty);
- writes `harness-host-manifest.json` (sources, packages with versions, unresolved optionals).

Staging is per target platform: run it on the OS and architecture being packaged.

Phase 3c extended the inputs (terminal-policy bundle `dist-vendor/`, `bundled-tools/<os>-<arch>`,
node-pty's `conpty.dll` + `OpenConsole.exe` beside `harness-host`, and the stale-binary method
check) and added `beforeBuildCommand` to the desktop Tauri configs. The current input list and
resolution order are in [phase-3c-packaging-tools.md](phase-3c-packaging-tools.md), which wins where
the two differ.

### 2.3 Node.js runtime

`src-tauri/src/host/node.rs` resolves `HARNESS_NODE_BIN`, else `node.exe` / `node` on `PATH`. It
then runs `--version` (no console window) and requires major >= 24, the root `engines.node`
floor; the Host uses `node:sqlite`. Failures are typed (`NotFound`, `MissingExplicit`,
`Unusable`, `TooOld`). Each one maps to `hostSpawnFailed` with an actionable message. The lookup
runs on every Host spawn, so installing Node does not require a shell restart.

On Windows the sidecar, `taskkill`, and the version probe start with `CREATE_NO_WINDOW`, so the
GUI-subsystem release shell does not open console windows.

**Phase 6 (implemented):** the official Node runtime pinned to the `mise.toml` version ships inside
`host/runtime/node/` (not as a Tauri `externalBin`), staged by the same staging script. The resolver
order is `HARNESS_NODE_BIN` -> bundled runtime -> `PATH`. Fetch, verification, signing and
installers are specified in [phase-6-packaging.md](phase-6-packaging.md), which wins where the two
differ.

## 3. Acceptance

| ID | Check | Evidence |
| --- | --- | --- |
| W1 | Window config parses and builds | `cargo build -p harness-app`, `cargo build --release -p harness-app` |
| W2 | Single title bar, drag, resize, min/max/close | manual on each desktop OS (frontend wiring owned by `src/platform/`) |
| W3 | Renderer close runs the barrier | `close()` -> `CloseRequested` path (section 1.4). The release exe exits on `WM_CLOSE` |
| P1 | Layout selection | `cargo test -p harness-app` (`host::layout` tests) |
| P2 | Staged layout is self-contained | stage with `--out` outside the repository, then run the phase-2 S1/S2/S4 flow with cwd = staged root: ready, init-local and attach-service-port `setting.get`, exit 0, rust-bridge uses the staged `harness-host` |
| P3 | Mobile unaffected | `tauri android build --debug --apk --target aarch64` succeeds and the APK contains no `host/` |
