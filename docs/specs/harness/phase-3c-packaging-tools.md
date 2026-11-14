# Phase 3c (item 9, item 4): packaged resources, native tool discovery, diff computation

Status: active implementation contract. Owner: Tauri shell packaging
(`Worse/apps/harness-app/src-tauri`, `Worse/scripts/stage-sidecar-resources.mjs`,
`Worse/scripts/build-host-sidecar.mjs`, `Worse/sidecars/rust-bridge` build). Builds on
[desktop-window-and-packaging.md](./desktop-window-and-packaging.md) section 2 (one layout, two
roots) and on [phase-3b-watch-pty.md](./phase-3b-watch-pty.md) (ConPTY sideload, terminal policy).

## 1. Scope

1. **Packaging.** A release Harness must run with no ZCode checkout present. Everything the Host,
   the rust-bridge and `harness-host` load at runtime is inside the bundled `host/` resource, and
   every path is resolved from the layout root chosen by `src-tauri/src/host/layout.rs`.
2. **Native tool discovery.** Inventory of how ZCode finds git, ripgrep, shells and Node, and a
   decision whether any of it moves to Rust (section 5).
3. **Diff computation.** Inventory of Host-served diff or preview-metadata work beyond
   `git.getDiff` and a decision whether any of it moves to Rust (section 6).

Out of scope: bundling Node.js (Phase 6), signing and installers beyond an unsigned NSIS/MSI
build (Phase 6), CUA helper tools (Phase 5), remote WSL/SSH tool deployment (Phase 4).

## 2. Gaps found in the Phase 3b layout

| Gap | Effect in a packaged build | Fix |
| --- | --- | --- |
| `sidecars/rust-bridge/dist-vendor/terminalPolicy.js` was not staged | the Rust PTY route cannot load ZCode's terminal policy, so every `terminal.create` falls back to TS | staged as a required source |
| `esbuild` (used by `rust-bridge/scripts/build-vendor.mjs`) was undeclared | the bundle built only because the root install hoists esbuild | declared as a `devDependency` of `@harness/rust-bridge` |
| `conpty.dll` + `OpenConsole.exe` were found only through `node-pty/package.json` | worked by accident (node-pty is staged for the Host); any change to node-pty staging silently drops the Rust PTY to the inbox ConPTY | staged next to `harness-host`, where portable-pty's `LoadLibrary("conpty.dll")` finds them first (the application directory) and the bridge skips the PATH sideload |
| `packages/desktop/bundled-tools/<platform>-<arch>/{ripgrep,ugrep,bfs}` were not staged | the Host's `findRuntimeToolBinary` (cwd-relative `packages/desktop/bundled-tools/<key>`) falls back to `PATH`; agent search tools break on machines without `rg` | staged at the same layout-relative path |
| `harness-host` path came from rust-bridge's own `../../target/release` guess | correct only because the staged tree mirrors the checkout | the shell passes `HARNESS_RUST_HOST` from `layout.rs` for packaged layouts |
| `tauri build` did not stage | a bundle built without running the staging script by hand shipped an empty `host/` | desktop `beforeBuildCommand` builds the sidecar, the release `harness-host`, and stages |
| a stale `Worse/target/release/harness-host` was staged as-is (found by the gate: the binary predated `pty.*`/`watch.*`) | the bridge sees `method not advertised` and every terminal silently goes to TS | staging runs an `initialize` handshake first and fails unless the binary advertises every method the bridge routes (route table + `pty.*`/`watch.*`) |
| release log: 40 KB, one file, Trace level (plugin defaults) | the startup lines (layout, resources, Rust services) rotate away within about a second of Host RPC logging | release log at Debug, 5 MiB per file, 3 rotations kept (`lib.rs`) |

## 3. Owners

| Artifact | Producer | Consumer at runtime |
| --- | --- | --- |
| `host/` tree | `stage-sidecar-resources.mjs` (only producer; copies, never builds) | the shell (`layout.rs`) |
| `dist-vendor/terminalPolicy.js` | `rust-bridge` `build` -> `scripts/build-vendor.mjs` (esbuild) | rust-bridge `sessions/terminalPolicy.ts` (`../../dist-vendor/` from `dist/`) |
| `conpty.dll`, `OpenConsole.exe` | node-pty `prebuilds/win32-<arch>/conpty`, copied by staging | `harness-host` (portable-pty) |
| bundled search tools | `pnpm --filter @zcode/desktop prepare:native-search` | TS Host `findRuntimeToolBinary` -> agent env `ZCODE_RG_BINARY` etc. |
| layout root, `HARNESS_RUST_HOST` | `layout.rs` (`HostLayout::resolve`, `rust_host()`) | host-node -> rust-bridge `readBridgeConfig` |

## 4. Packaged layout

On Windows `resource_dir()` is the executable directory; on Linux/macOS it is the bundle's
resource directory. The tree below is relative to it.

```text
harness-app(.exe)
host/                                         <- layout root (LayoutSource::Packaged)
  harness-host-manifest.json                  sources, packages, unresolved optionals, tools
  Worse/sidecars/host-node/dist/main.js       + package.json {"type":"module"}
  Worse/sidecars/rust-bridge/dist/*.js        + package.json {"type":"module"}
  Worse/sidecars/rust-bridge/dist-vendor/terminalPolicy.js
  Worse/target/release/harness-host(.exe)
  Worse/target/release/conpty.dll             (Windows)
  Worse/target/release/OpenConsole.exe        (Windows)
  packages/desktop/out/host/index.js          + chunks
  packages/desktop/bundled-tools/<os>-<arch>/ripgrep/rg(.exe)
  packages/desktop/bundled-tools/<os>-<arch>/ugrep/ugrep(.exe)   (when prepared)
  packages/desktop/bundled-tools/<os>-<arch>/bfs/bfs              (when prepared, Linux)
  apps/zcode-cli/packages/cli/dist/zcode.cjs
  config/provider/zcode-builtin{,.test}.json
  node_modules/                               runtime deps (node-pty, ws, ...), symlinks dereferenced
```

## 5. Resolution order

| Resource | Order |
| --- | --- |
| layout root | `HARNESS_REPO_ROOT` -> release: `resource_dir()/host` (verbatim prefix stripped) -> debug: checkout |
| launcher, Host entry, provider config | layout root + fixed relative path; `HARNESS_HOST_ENTRY` / `ZCODE_BUILTIN_PROVIDER_CONFIG_FILE` override single files |
| `harness-host` (spawned lazily by the bridge on the first routed call) | `HARNESS_RUST_HOST` (user) -> packaged: shell sets `HARNESS_RUST_HOST=<root>/Worse/target/release/harness-host(.exe)` -> bridge default `../../target/release` then `../../target/debug` from its package |
| ConPTY (Windows) | `HARNESS_CONPTY_DIR` (`off` = inbox) -> `conpty.dll` next to `harness-host` (no PATH change) -> node-pty `prebuilds/win32-<arch>/conpty` resolved from the bridge package -> inbox ConPTY |
| terminal policy | `dist-vendor/terminalPolicy.js` next to the bridge `dist/` (no fallback: missing bundle = TS terminal) |
| ripgrep/ugrep/bfs | unchanged ZCode order: `ZCODE_*_BINARY` -> `ZCODE_SERVER_RUNTIME_ROOT/tools` -> `cwd/bundled-tools/<key>` -> `cwd/packages/desktop/bundled-tools/<key>` (matches the layout) -> `PATH` |
| Node.js | unchanged (desktop-window-and-packaging.md 2.3) |

At startup the shell logs (info) each resolved path and logs a warning for every missing
optional resource (`harness-host`, terminal policy, ConPTY pair, ripgrep). Missing optional
resources never block the Host: the bridge falls back to TS as before.

## 6. Native tool discovery (decision: not ported)

| Tool | ZCode code | Exposed as RPC? | Path |
| --- | --- | --- | --- |
| git binary | `services/src/git/providers/gitEnvironmentProvider.ts` (`git --version` per candidate, cached per process) | no | internal to git service; Rust git has its own runner |
| ripgrep / ugrep / bfs | `services/src/runtime-tools/runtimeToolResolver.ts` | no | builds the agent env patch at agent spawn |
| login-shell env (macOS/Linux) | `runtime-tools/runtimeLoginShellEnvCapture.ts` | no | Host-internal, at startup |
| terminal default shell | `terminal/terminalService.ts` `resolveTerminalShell` | no (inside `terminal.create`) | already used verbatim by the Rust PTY route via the vendored policy |
| integrated terminal shells (cmd, Git Bash) | `system/integratedTerminalShells.ts` | `system.listIntegratedTerminalShells` | Settings page only (`SettingsPage.tsx`), once on open; a few `access()` calls |
| WSL detection | `server/src/remote/wsl-detect.ts` | remote connect flow | Phase 4 |

The only RPC is a Settings-page call that costs a handful of `access()` checks; it is not on a
hot or startup path, and a Rust copy would add a second implementation without a measurable
gain. No `tools` flag is introduced. The packaging work above is the part of tool discovery that
matters for Harness: shipping the bundled search tools and the ConPTY pair so ZCode's own
resolvers find them.

## 7. Diff computation (decision: not ported)

The Host has no diff engine besides git. Host-served diff work:

- `git.getDiff` — routed to Rust in Phase 3a.
- `git-checkpoint.diffCheckpoints` — git-based; registered by the Host (`services/src/node.ts`)
  but called by no renderer, web, client or CLI code in the checkout.
- `file.readMediaPreview` / `file.readBinaryPreview` — preview metadata, routed in Phase 3a.
- `media-preview.prepare` — authorizes a path and mints a playback URL through Main; not a
  computation.

Edit-tool diffs are rendered in the renderer from tool payloads. No `diff` flag is introduced.

## 8. Acceptance scenarios

| ID | Scenario | Evidence |
| --- | --- | --- |
| K1 | Staging into a directory outside the repo produces every file in section 4 | `node Worse/scripts/e2e/phase3c-tools-diff.mjs` (static checks) |
| K2 | Staged Host runs from the staged root with dev-tree overrides removed: ready, init-local attach, `file.*` and `git.*` served by Rust (bridge trace `rust`) | same gate |
| K3 | `terminal.create` from the staged tree returns an `rpty-` id (vendored policy loaded), echoes a marker, and the `OpenConsole.exe` it runs is the staged copy | same gate (process image paths) |
| K4 | Staged `rg --version` runs | same gate |
| K5 | After shutdown no process whose image lives in the staged root is left | same gate |
| K6 | `layout.rs` selects the packaged root and the packaged `harness-host` path | `cargo test -p harness-app` |
| K6b | The `host/` inside a built installer passes K1-K5 | `phase3c-tools-diff.mjs --layout <extracted installer>/host` |
| K7 | Release bundle builds (`pnpm tauri build`, NSIS), the exe launched from a directory outside the repo starts the Host with Rust services active, renders, and closes gracefully with no orphans | manual run recorded in `Worse/.work/phase3c/toolsB.md` with a screenshot |

Gate output: `PHASE3C_TOOLS_DIFF_OK`.
