# Harness Specs

Specs for Harness, the Rust and Tauri v2 remake of ZCode. All code lives under `Worse/`.

| Spec | Phase | Status | Covers |
| --- | --- | --- | --- |
| [phase-1-shell.md](./phase-1-shell.md) | 1 | Active implementation contract | Product rules, state ownership, module map, Rust traits, Tauri commands, TypeScript adapter, platform matrix, acceptance scenarios, exclusions |
| [phase-2-host-sidecar.md](./phase-2-host-sidecar.md) | 2 | Active implementation contract | Node Host sidecar under Rust supervision, loopback attachment transport, Host<->Main message handling, renderer bootstrap, build and acceptance gates |
| [desktop-window-and-packaging.md](./desktop-window-and-packaging.md) | 2 fixes | Active implementation contract | Frameless window config and permissions, close barrier, packaged Host layout, staging script, Node discovery |
| [phase-3a-rust-services.md](./phase-3a-rust-services.md) | 3a | Active implementation contract | Rust file/search/git read services behind the RPC port seam, flags, fallback, differential gate |
| [phase-3b-watch-pty.md](./phase-3b-watch-pty.md) | 3b | Active implementation contract | Rust file watcher and PTY/terminal behind the RPC port (events), session ownership, no-orphan cleanup, differential gate |
| [phase-4-web-remote-control.md](./phase-4-web-remote-control.md) | 4 (B) | Active implementation contract | Web remote control: vendored manager in the Host sidecar, `harness-web-remote-*` channel, phone `web-remote-replayable` attachments, relay statelessness, fail-closed rules, mobile remote-client entry, gate |
| [phase-4-remote-workspaces.md](./phase-4-remote-workspaces.md) | 4 (A) | Active implementation contract | SSH/WSL/Docker remote workspaces: vendored Main session manager in the Host sidecar, control channel, scoped attachments, renderer registration, gate |
| [phase-5-plugins.md](./phase-5-plugins.md) | 5 (B) | Active implementation contract | Plugins, MCP App / Gen UI sandbox (ZCode registry + protocol bundled into host-node, `hsandbox` scheme, iframe relay shell), permission model, CUA decision, feature matrix, gate `phase5-plugins.mjs` |
| [phase-6-packaging.md](./phase-6-packaging.md) | 6 (A) | Active implementation contract | Bundled Node runtime (fetch, checksum, cache, `host/runtime/node`, resolver order), installer targets per OS, signing policy and hooks (fail closed), per-user NSIS install/uninstall rules, data-directory parity, gate `phase6-package.mjs` |
| [phase-6-updater.md](./phase-6-updater.md) | 6 (B) | Active implementation contract | Auto-updater: `tauri-plugin-updater`, separate `latest.json` feed, minisign signatures with signed version, full packages only, fail-closed rules, Host-draining install orchestration, rollback, key management, gate `phase6-update.mjs` |
| [phase-7-parity.md](./phase-7-parity.md) | 7 (B) | Active implementation contract | Functional parity Harness vs ZCode (scenarios, drivers, mock provider, offline sign-in path), Harness↔ZCode rollback on shared data, kill switch, gate `phase7-parity.mjs` |

## Reading order

1. [Harness Rust and Tauri v2 plan](../../../../docs/plans/harness-rust-tauri-v2-plan.md) for context and phase gates.
2. `phase-1-shell.md` before touching `Worse/crates/*` or `Worse/apps/harness-app/*`.
3. `phase-2-host-sidecar.md` before touching the Host sidecar, Host bridge, or renderer bootstrap.
4. `AGENTS.md` and `docs/agents/runtime-boundaries.md` for repository-wide rules.

## Authority rules

- The plan defines phases and exit gates. A spec refines one phase and must not contradict the plan.
- `Worse/Cargo.toml` and each package manifest are authoritative for real dependency versions.
- Section 4 of the Phase 1 spec is the shared contract for Rust traits, command names, and generated TypeScript. Changes require updating the spec first.
- Acceptance scenarios are the phase exit evidence. A manual scenario needs a recorded OS, engine version, steps, result, and screenshot.

## Planned specs

The following are planned and do not exist yet. Do not link to them until they are written:

- Phase 3b+: remaining Rust hot-path services (diff, SQLite).
- Phase 5: Browser, plugins, and CUA decisions.
- Release: update replacement (Phase 6 B). Packaging and signing are in `phase-6-packaging.md`.

## Conventions

- Normal documentation is English. Bug-fix comments in code are Chinese, per `AGENTS.md`.
- Specs describe current repository capability only. Removed modules are not kept as references.
- Do not add App business unit tests. Verification follows `docs/testing/unit-test-quality.md`.
- Use diagrams for state ownership, process boundaries, and event order when a prose list would hide ordering.
- Never include credentials, real user data, or internal service addresses in examples.

