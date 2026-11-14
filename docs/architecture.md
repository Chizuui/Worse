# Harness Architecture

Harness is a Tauri v2 desktop shell with a Rust service layer and the shared TypeScript React
UI in the webview. This document is the source of truth for layering, ownership, and extension
conventions inside `Worse/`. Background and phased migration context live in
[the migration plan](../../docs/plans/harness-rust-tauri-v2-plan.md).

## 1. Layering

- The Tauri app is a **composition root**, not a service container. It wires plugins, state, and
  commands and delegates behavior to crates.
- Crates expose a small public API from `lib.rs`; everything else is private.
- Dependencies flow one way only:

```text
harness-app (Tauri) --> harness-platform
                    --> harness-sidecar   (desktop only)
                    --> harness-protocol
harness-platform   --> harness-protocol
harness-protocol   --> harness-core
harness-core       --> (no internal dependencies)
```

- `harness-core` holds ids, surface, and workspace-key rules. It must stay dependency-free inside
  the workspace.
- `harness-protocol` owns every wire type, the command-name constants, and the generated
  TypeScript bindings. It is the only crate that generates TS.
- `harness-platform` owns native behavior behind traits. It must not depend on Tauri so that the
  same code can be reused by a future Host process and tested headlessly.
- `harness-sidecar` owns process supervision and is compiled only for desktop targets.
- `apps/harness-app/src-tauri` owns window lifecycle, plugin registration, command handlers, and
  the in-process adapters. It must not own task, session, queue, or projection state.

### State ownership

| State | Owner | Never duplicated in |
| --- | --- | --- |
| Drafts, focus, scroll, navigation | React UI | Rust shell |
| Windows, menus, dialogs, shortcuts | Tauri shell | Rust crates |
| Attachment routing, owner/lease metadata | Window Host (future) | UI |
| Conversation, queue, goals, interactions | Agent runtime | Rust caches |
| Native URL/file/path capability | `harness-platform` | Command handlers |

A convenience cache for file trees, task indexes, or search results is allowed. Conversation
rows, accepted input queues, goals, and interaction state are not: they stay derived from the
agent projection and are recovered by sequence and revision.

## 2. Crate responsibilities

| Crate | Responsibility | Must not do |
| --- | --- | --- |
| `harness-core` | `WorkspaceKey`, `WindowId`, `SidecarId`, `Surface`, `CoreError` | I/O, platform calls, wire formatting |
| `harness-protocol` | Wire structs, `PROTOCOL_VERSION`, `commands::*` constants, ts-rs export | Business logic, native calls |
| `harness-platform` | URL validation/opening, path reveal, capability matrix, `PlatformError` | Tauri imports, UI concerns |
| `harness-sidecar` | Child process spawn, NDJSON streams, graceful shutdown barrier | Protocol ownership, UI state |
| `harness-app` (Rust) | Tauri builder, plugins, `AppState`, command registration | Business state, shell-string execution |
| `@harness/app` (TS) | React entry, Tauri `IPlatformService` adapter, typed invoke wrappers | Direct `@tauri-apps/api` calls outside `src/platform/` |

### Mobile gating

Mobile builds (Android, iOS) are remote clients. Native desktop behavior is excluded at compile
time, not at runtime:

- `harness-sidecar` is guarded by
  `#![cfg(not(any(target_os = "android", target_os = "ios")))]` and declared as a target-specific
  dependency in `apps/harness-app/src-tauri/Cargo.toml`.
- OS adapter modules in `harness-platform` are selected with `cfg(target_os = ...)` and only
  compiled on desktop.
- `Surface::current()` reports the compile target; `capabilities_for(surface)` is the single place
  that decides which commands are supported.

Command handlers return `CommandError::Unsupported` instead of branching on `if mobile` inside
business logic.

## 3. Adding a crate

1. Create `crates/harness-<name>/` with a `package` name, `lib` name, and inherited metadata:

   ```toml
   [package]
   name = "harness-<name>"
   version = "0.1.0"
   edition.workspace = true
   rust-version.workspace = true
   license.workspace = true

   [dependencies]
   serde.workspace = true

   [lints]
   workspace = true
   ```

2. Add the crate to the dependency graph only in an allowed direction. Update the diagram in
   this file and in `README.md` in the same change.
3. Keep modules private and re-export the public API from `lib.rs`. Document every `pub` item.
4. Keep files below roughly 300 lines; split by responsibility instead of growing a `utils` file.
5. If the crate is desktop-only, add the `cfg` gate at the crate root and a target-specific
   dependency at the consumer.
6. Verify with `cargo fmt -p harness-<name> -- --check`, `cargo clippy -p harness-<name>
   --all-targets -- -D warnings`, and `cargo test -p harness-<name>`.

## 4. Adding a command

Commands are the only path from the UI into Rust. Add them in this order:

1. **Protocol types.** Add request/response structs in `harness-protocol` with
   `#[derive(Serialize, Deserialize, TS)]`, `#[serde(rename_all = "camelCase")]`, and
   `#[ts(export, export_to = "../../apps/harness-app/src/generated/")]`. Reuse `CommandError` for
   failures. Bump `PROTOCOL_VERSION` when the wire shape changes incompatibly.
2. **Command name.** Add a constant to `harness_protocol::commands`. Command names are
   `snake_case` and identical in Rust, Tauri registration, and TypeScript.
3. **Behavior.** Implement the logic in `harness-platform` behind an existing trait, or in the
   owning service crate. Return a typed error; do not format strings in handlers.
4. **Handler.** Add a thin handler in `apps/harness-app/src-tauri/src/commands/{app,platform}.rs`
   returning `Result<T, CommandError>`, re-export it from `commands/mod.rs`, and register it in
   the `tauri::generate_handler!` list in `lib.rs`.
5. **Capability.** If the command needs a new plugin permission, add it to
   `capabilities/desktop.json` and `capabilities/mobile.json` deliberately. Never broaden a
   capability file "to make it work".
6. **Bindings.** Run `cargo test -p harness-protocol` to regenerate `src/generated/*.ts`. Add the
   typed wrapper to `apps/harness-app/src/platform/commands.ts`, then expose it through
   `tauriPlatform.ts` only if `IPlatformService` has a matching method.
7. **Tests.** Add pure Rust unit tests in the owning crate. Do not add TypeScript app unit tests.

### Adding a native adapter

Define a trait in `harness-platform` when behavior differs per OS
(`UrlOpener`, `PathRevealer`). Implement it per OS in `windows.rs`, `macos.rs`, or `linux.rs` and
expose a default constructor behind `cfg(not(mobile))`. Command handlers depend on the trait, so
tests can substitute a fake without touching Tauri.

## 5. Errors

Use `thiserror` enums with meaningful variants; never stringly-typed errors and never
`unwrap()`/`expect()` outside tests (Clippy denies both).

| Layer | Type | Notes |
| --- | --- | --- |
| `harness-core` | `CoreError::{EmptyWorkspaceKey, InvalidId}` | No I/O; pure validation |
| `harness-protocol` | `CommandError { code, message }` | The only error shape crossing the IPC boundary |
| `harness-platform` | `PlatformError::{Unsupported, InvalidInput, Io}` | Converted with `From` into `CommandError` |
| `harness-sidecar` | `SidecarError::{Spawn, AlreadyRunning, NotFound, Io}` | Supervision failures |

Rules:

- Convert at the boundary that knows how to classify the failure. Handlers should mostly be `?`.
- Message text must be safe for the UI: no absolute internal paths, credentials, or stack traces.
- `Unsupported` means the surface cannot do it (for example reveal on mobile); `InvalidInput`
  means validation failed (for example a `file:` URL); `Denied` means permission; `Internal`
  means an unexpected failure that was logged.
- Log the technical cause (`tracing::error!` in library/service crates, `log::error!` in the Tauri shell) and return a short `message`.

## 6. Logging

Library and service crates use `tracing`. The Tauri shell entry (`apps/harness-app/src-tauri`) uses the `log` facade because `tauri-plugin-log` consumes it; debug builds also mirror to stdout. TypeScript uses the shared logger. `println!`/`dbg!` are not allowed.

| Level | Use for |
| --- | --- |
| `debug` | Raw protocol frames, stream chunks, per-tool updates, high-frequency diagnostics |
| `info` | Process and session lifecycle, permission results, one-time initialization |
| `warn` | Recoverable failures (retry succeeded, adapter fallback) |
| `error` | Crashes, handshake failures, lost authentication, unrecoverable I/O |

- Prefer structured fields: `tracing::info!(surface = ?surface, "capabilities resolved")`, or `log::info!` with the same message in the shell.
- Never log tokens, API keys, credentials, user content, or internal service addresses.
- Frontend code uses `packages/ui/src/logger.ts`. Service code uses
  `createServiceLogger(scope)` from `packages/services/src/logger/serviceLogger.ts`.
- Serialization errors caused by untrusted input are `warn`; failures that break a session are
  `error`.

## 7. Conventions

- Rust 2021, stable toolchain, `rustfmt` and `clippy` pinned in the workspace; workspace lints
  enable `clippy::all` and `clippy::pedantic` and deny `unwrap_used`/`expect_used`.
- `unsafe_code` is forbidden workspace-wide.
- Prefer small modules with explicit `pub` APIs. Add doc comments to every public item.
- Bug-fix comments in Chinese (per repository policy); all other documentation in English.
- Never build shell command strings. Pass arguments as separate `Command` args.
- Do not block the Tauri main thread: use async I/O, `tauri::async_runtime::spawn_blocking` for
  filesystem work, and keep long-running supervision off the event loop.
- Protocol names are `snake_case` in Rust/TS commands and `camelCase` in JSON payloads.

## 8. Verification

Run the narrowest check first, then the workspace:

```powershell
cd Worse
cargo fmt -p harness-core -- --check
cargo clippy -p harness-core --all-targets -- -D warnings
cargo test -p harness-core
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Frontend: `pnpm typecheck` and `pnpm build` inside `apps/harness-app`.

`cargo test --workspace` regenerates the protocol bindings. Commit generated files together with
the protocol change so the frontend and Rust crates never disagree.