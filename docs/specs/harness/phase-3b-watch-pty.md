# Phase 3b: Rust file watcher and PTY/terminal behind the RPC port

Status: active implementation contract. Wire format: `Worse/.work/phase3/contract.md` r2, section 9
(normative for method names, params, results, notifications, error codes). Builds on
[phase-3a-rust-services.md](./phase-3a-rust-services.md) (seam, flags, fallback, supervisor).

## 1. Purpose

Move the two long-lived, event-streaming Host services to Rust without changing ZCode sources and
without changing what the renderer sees:

1. File watcher: RPC channel `file-watcher` (`watch`, `unwatch`, event `onDynamicChange(id)`).
2. Terminal: RPC channel `terminal` (`create`, `write`, `resize`, `dispose`, events
   `onDynamicData(id)`, `onDynamicExit(id)`).

The TS services (`fileWatcherService.ts`, `terminalService.ts` with node-pty) stay as the fallback and
the differential oracle.

## 2. Product rules

- W1 Drop-in events: a routed watcher emits the same `FileWatchEvent` sequence the TS watcher would
  emit for the same file-system activity: 150 ms trailing debounce, 1000 ms max-wait,
  `changedPath` only when the batch has exactly one known path, a final `{dirPath}` event and
  self-close when the watched path's identity changes or the backend errors. No ignore rules.
- W2 `watch` errors keep the TS text `无法监视目录 '<path>': <node message>`; `unwatch` of an unknown
  id is a no-op; subscribing to a closed or unknown watcher id never fails (TS returns
  `Event.None`).
- T1 Drop-in terminal: `create` returns the TS object shape `{id, shell, fontFamily, fontSize?,
  theme?, fontFamilySource, windowsPty?}`. Shell, cwd, env and the font/theme profile are computed
  by ZCode's own functions (`resolveTerminalShell`, `resolveTerminalCwd`, `resolveTerminalEnv`,
  `resolveTerminalFontProfile`, `resolveTerminalWindowsPtyInfo`) bundled from the ZCode source at
  bridge build time, so they cannot drift. Settings come from the TS Host's own `setting.get`.
- T2 Output is UTF-8 text decoded with a streaming decoder (no U+FFFD at chunk boundaries). Output
  produced before the renderer subscribes (and while no subscriber exists) is buffered up to
  1,048,576 UTF-16 units (oldest chunks dropped first) and replayed in order to the first new
  subscriber, exactly like `createBufferedTerminalOutputEmitter`.
- T3 `onDynamicExit` fires once with the exit code to current subscribers, after all output;
  afterwards the id is unknown (`write`/`resize` -> `Terminal not found: <id>`; a late subscription
  fails with the same message as an RPC `EventError`). `dispose` kills the process tree and emits no
  exit event.
- O1 No orphans: a Rust PTY session dies when (a) the renderer disposes it, (b) the `create` that
  spawned it was cancelled or its attachment closed before the result arrived, (c) the host-node
  process shuts down, or (d) harness-host dies (Job Object kill-on-close on Windows, hang-up on
  Unix). Desktop terminals are Host-global in the TS Host (the renderer re-subscribes to the same
  id over a new attachment after its RPC proxy is regenerated, see
  `sidePaneTerminalSessionRegistry.ts` `rebind`), so closing an attachment only drops that
  attachment's subscriptions; output then goes to the no-listener buffer. A Rust watcher is
  per attachment (the UI re-watches on a new service) and is closed in cases (a)-(d) and on
  attachment close (stricter than TS, which keeps it until host exit).
- O2 Never a silent hang: if harness-host crashes, every Rust terminal of every attachment receives
  `onDynamicExit(-1)` and every Rust watcher a final `{dirPath}` event; new `create`/`watch` calls go
  to TS (restart backoff or disabled), later calls for the dead ids go to TS (`Terminal not found`).
- F1 Flags `watch` and `pty` in `HARNESS_RUST_SERVICES` (also covered by `all`). Default decided by
  the gate (section 7): identical behaviour and not slower -> default on.
- S1 Scope: `watch` is routed on local-scope attachments; `pty` only on local-scope
  `desktop-continuous` attachments. Phone terminals keep the TS owner/lease scoping
  (`createWindowHostTerminalOwners`); remote scopes are never intercepted.
- S2 Single writer per id: an id is served by exactly one implementation for its whole life. Bridge
  ids (`rw-<n>`, `rpty-<n>`) never collide with TS numeric ids; frames naming an id the bridge does
  not own always go to the TS Host.

## 3. State ownership and event order

| State | Owner | Lifetime |
| --- | --- | --- |
| PTY process, ConPTY / pty master, Job Object | harness-host | spawn -> exit / kill / harness-host exit |
| Watch handle, debounce batch, identity | harness-host | subscribe -> unsubscribe / self-close / harness-host exit |
| Notification fan-out `pty:<sessionId>` / `watch:<watchId>` -> sink | rust-bridge session directory (process level, one listener per harness-host) | until the session ends or harness-host is lost |
| Terminals `rpty-n -> sessionId` and the no-listener output buffer | rust-bridge PTY session store (process level, Host-global like TS) | until exit, `dispose`, or harness-host loss |
| Watchers `rw-n -> watchId` | rust-bridge watch router, per attachment | until `unwatch`, `disposeAll`, self-close, attachment close, or harness-host loss |
| Event subscriptions (`EventListen` request ids) | rust-bridge, per attachment | until `EventDispose`, session end, or attachment close |
| Settings (`terminalFontFamily`, `terminalInheritSystemProfile`) | TS Host `setting` service | read per `create` through an injected Host RPC |

```mermaid
sequenceDiagram
  participant R as Renderer (RPC client)
  participant B as rust-bridge (attachment router)
  participant T as TS Host
  participant H as harness-host (Rust)
  R->>B: Promise terminal.create {cols,rows,cwd}
  B->>T: injected Promise setting.get (bridge id >= 2^30)
  T-->>B: PromiseSuccess settings (consumed, never reaches R)
  Note over B: bundled ZCode policy: shell, cwd, env, font profile, windowsPty
  B->>H: pty.spawn {shell,args,cwd,env,cols,rows,name}
  H-->>B: {sessionId,pid}
  H--)B: $/pty/output (buffered: no listener yet)
  B-->>R: PromiseSuccess {id:"rpty-1", shell, fontFamily, ...}
  R->>B: EventListen terminal.onDynamicData("rpty-1")
  B-->>R: EventFire (buffered output replayed in order)
  R->>B: Promise terminal.write {id,data}
  B->>H: pty.write (per-session FIFO)
  H--)B: $/pty/output
  B-->>R: EventFire data
  H--)B: $/pty/exit {exitCode}
  B-->>R: EventFire onDynamicExit(exitCode)
  Note over B: id removed; later frames for rpty-1 go to the TS Host
```

Teardown order (no orphans):

```text
renderer dispose(id)      -> bridge drops id + subscriptions -> pty.kill (tree)       -> no exit event
attachment close          -> bridge drops that attachment's subscriptions and in-flight creates
                             (late spawn results are killed), unsubscribes its watchers;
                             terminals stay alive for re-subscription (Host-global, as TS)
host-node shutdown        -> bridge dispose -> harness-host stdin EOF -> kill all, exit 0 (<= 2 s)
harness-host crash        -> Job Object / SIGHUP kills shells; bridge: onDynamicExit(-1),
                             final {dirPath} per watcher, ids dropped, supervisor backoff
```

## 4. Interfaces

- RPC (renderer-facing, unchanged): `ITerminalService`, `IFileWatcherService` in
  `packages/services/src/{terminal,fileWatcher}`.
- Wire (bridge <-> harness-host): contract r2 section 9.
- Bridge modules (`Worse/sidecars/rust-bridge/src`):
  - `rpc/frames.ts`: header readers for Promise/Cancel/EventListen/EventDispose and outbound
    responses; `EventFire`/`EventError` encoders.
  - `interceptingPort.ts`: inbound interception (3a) plus outbound interception and Host injection
    (for the bridge's own `setting.get` call).
  - `sessions/sessionDirectory.ts`: process-level map from Rust ids to owners; one notification
    listener per harness-host instance; host-loss fan-out.
  - `sessions/ptyRouter.ts`: process-level PTY session store + per-attachment `terminal` router;
    `sessions/watchRouter.ts`: per-attachment `file-watcher` router.
  - `sessions/outputBuffer.ts`: TS-identical no-listener buffer.
  - `sessions/hostCall.ts`: injected Host RPC with bridge-owned ids.
  - `sessions/terminalPolicy.ts` + `vendor/terminalPolicy.entry.ts`: bundled ZCode policy loader.

## 5. Acceptance scenarios (gate `Worse/scripts/e2e/phase3b-diff.mjs`)

Each scenario runs against the real ZCode Host behind host-node twice (TS only, Rust routed) and
compares normalised results; latency is reported per mode.

| id | scenario | expectation |
| --- | --- | --- |
| A1 | watch dir; create / modify / rename / delete a file | same event list (dirPath, changedPath) per step |
| A2 | burst of 20 writes in 100 ms | coalesced into one event (several paths -> no `changedPath`), same as TS |
| A3 | recursive watch; change in `node_modules/x` and `.git/y` | reported (no ignore rules), same as TS |
| A4 | unwatch, then change | no event in either mode |
| A5 | watch missing path | same error text |
| A6 | delete / rename the watched dir | one final `{dirPath}` event, then silence |
| B1 | create default shell, echo marker | marker echoed; create result shape equal after id normalisation |
| B2 | explicit `cmd.exe` (Windows) / `sh` (Unix) via `pty.spawn` | echo roundtrip; exit code of `exit 3` is 3 |
| B3 | unicode output (`中文 ✓ é`) | identical decoded text, no U+FFFD |
| B4 | resize then query size | new size visible to the shell |
| B5 | dispose | no exit event, `write` afterwards -> `Terminal not found: <id>` |
| B6 | shell `exit 7` | `onDynamicExit(7)` |
| B7 | two concurrent sessions | outputs do not cross |
| B8 | close the attachment, re-subscribe from a new one; then stop host-node | terminal survives the attachment (buffered output replayed); after stop no shell, conhost or harness-host process left |
| B9 | harness-host killed mid-session | `onDynamicExit(-1)`, new create served by TS |

Gates also re-run `phase3-diff.mjs` (PHASE3_DIFF_OK) and `phase2-smoke.mjs` (PHASE2_E2E_OK) with
the new flags on and off.

## 6. Exclusions

- Phone terminals (owner/lease scoping) and remote-scope terminals/watchers stay on TS.
- No ignore rules or event filtering beyond TS behaviour.
- harness-host keeps no on-disk state.

## 7. Default decision rule

Same as 3a: a service defaults on only when the differential gate shows identical behaviour and
the Rust path is not slower (watch event latency, PTY spawn time, echo roundtrip). The decision and
numbers are recorded in `Worse/.work/phase3b/bridge.md` and in `routing/flags.ts`.
