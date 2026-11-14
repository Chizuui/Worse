# Phase 4 (A): Remote workspaces (SSH / WSL / Docker)

Status: active implementation contract. Plan section: "Phase 4" in
`docs/plans/harness-rust-tauri-v2-plan.md`. Companion: Phase 4 (B) web remote control / relay /
mobile (owned separately; this spec only exposes the seam B needs, section 9).

## 1. Purpose

Remote workspaces must work in Harness exactly as in ZCode: the user picks SSH / WSL / Docker in
the shared UI, the window's Host deploys or reuses the remote runtime, and the remote workspace's
file / git / terminal / agent services are served by the remote source through a remote-scoped
attachment.

In ZCode the Electron **Main** process owns the desktop half of that flow
(`packages/desktop/src/main/desktopRemoteSessions.ts`, `desktopMainIpcRemote.ts`,
`desktopRuntimeEnv.ts`). The Host half (`windowRemoteConnectionRegistry`, deploy, handshake,
transports in `packages/server/src/remote`) already runs unchanged inside the Harness Host
sidecar. Before this phase Harness was missing only the Main half:

| Missing in Harness (before) | ZCode owner | Effect |
| --- | --- | --- |
| IPC `connectRemote`, `cancelPendingRemoteConnection`, `bindRemoteWorkspaceSessionContext`, `disposeRemoteSession`, discovery (`isDockerAvailable`, `listWSLDistros`, `listDockerContainers`, `listSSHConfigAliases`) | `desktopMainIpcRemote.ts` | `tauriPlatform.ts` returned failure / empty lists |
| Remote session manager (route table, pending connects, bind flights, renderer attachment ready barrier, reconnect, close, provider provisioning coordinator) | `desktopRemoteSessions.ts` + `providerProvisioningEnvironmentCoordinator.ts` | never instantiated |
| Remote-scoped `attach-service-port` + `ScopedServicePort` delivery to the renderer, `ScopedServicePortReady` confirmation | Main `MessageChannelMain` | no transport for a second, remote-scoped port |
| `remoteAssets` (CDN base URLs, cache dir, artifact policy) on `init-local` / `connect-remote-workspace` | `resolveRemoteAssetDirs` | Host had no deploy source |
| Host→Main answers: `remote-workspace-*`, `project-workspace-prepared`, `provider-provisioning-*` and `provider-provisioning-lane-request` | Main Host listener | Rust classified them `Unsupported` / answered lane `none` |
| Renderer: base services registration, remote port registration (`registerRemoteWorkspaceSession`), `allowRemoteWorkspace` | renderer `main.tsx` | remote UI disabled |

## 2. Product rules

- R1 Same behaviour as ZCode: the ZCode Main session manager runs **verbatim** (bundled from
  ZCode source, never edited), so routing, bind, reconnect, close, timeouts (renderer attachment
  ready 15 s, bind = `REMOTE_WORKSPACE_PREPARATION_TIMEOUT_MS` + 10 s) and error codes are
  identical.
- R2 One owner per window: the remote registry lives in the window's Local Host
  (`windowRemoteConnectionRegistry`); the Main-side route table lives in the Host **sidecar
  launcher** process of that window. There is no Desktop Remote Host and no second Agent.
- R3 Fail closed: every renderer request is answered (success or structured failure); a closed
  base attachment, a Host exit, or an unknown session rejects pending work instead of waiting.
  Discovery failures return `false` / `[]` like ZCode, and are logged.
- R4 Identity: `workspaceIdentity` is the isolation key and `workspacePath` the file-operation
  path; `remoteSessionId` and `workspaceIdentity` travel through every hop (section 5). No step
  matches on path alone.
- R5 Remote ports never touch Rust services: a remote-scoped attachment is delivered to the TS
  Host undecorated (`isLocalScopeHostMessage` is false for `scope.kind: "remote"`), so the Phase 3
  Rust fs/git/search/watch/pty routes never see remote traffic. The gate asserts it.
- R6 No ZCode source edits; no Rust protocol change. All new code lives in
  `Worse/sidecars/host-node/**` and `Worse/apps/harness-app/src/**`.
- R7 Credentials: the renderer→sidecar control channel carries the `RemoteTarget` exactly as the
  ZCode IPC does (it may contain an SSH password the user typed). It is never logged; the sidecar
  logs only `kind`, request ids and error codes.

## 3. State ownership

| State | Owner | Notes |
| --- | --- | --- |
| Remote connections, transports, deploy, logical sessions | Host `windowRemoteConnectionRegistry` | unchanged ZCode Host code |
| Route table, pending connects/binds, renderer attachment state, provisioning lanes | sidecar `remoteWorkspace` controller (vendored `createRemoteWorkspaceSessionManager`) | one per sidecar = one per window |
| Base (local) attachment identity for the control channel | sidecar `remoteWorkspace` controller | latest local `desktop-continuous` attachment wins |
| Scoped attachment grants (id + token) | sidecar attachment registry (same registry Rust grants use) | minted by the sidecar, single use, 30 s TTL |
| Remote session services in the UI | `@zcode/ui` `remoteWorkspaceSessionStore` | registered by the renderer bridge |
| Remote assets (CDN URLs, cache dir) | sidecar, computed per connect | section 7 |

Rust (`harness-host-control`) keeps owning process supervision, the local `init-local` grant and
renderer reload grants. It never sees remote messages (section 6).

## 4. Components

### 4.1 Sidecar: vendored Main manager (`sidecars/host-node/vendor/remoteWorkspace.entry.ts`)

`scripts/build-vendor.mjs` bundles with esbuild into `dist-vendor/remoteWorkspace.js`:
`createRemoteWorkspaceSessionManager`, `createCanonicalWslTargetResolver`, discovery
(`listWSLDistros`, `isDockerAvailable`, `listDockerContainers`,
`listSSHConfigAliasesFromLocalConfig`), `resolveRemoteCdnBaseUrls`, and the shared helpers the IPC
layer needs (`remoteTargetSchema`, `normalizeRemoteConnectionError`, `normalizeUnknownError`,
`isPublicZCodeErrorCode`, `hostRemoteWorkspaceConnectFailedResponseSchema`, `formatZodError`,
`ZCODE_VERSION`). `electron` is replaced by a stub exposing a one-window `BrowserWindow` registry;
`MessageChannelMain` is never used because the manager's `createMessageChannel` option is
injected. `__ZCODE_VERSION__` / `__ZCODE_ENV__` are defined from the same build metadata the Host
bundle uses (`packages/desktop/scripts/build-metadata.mjs`; env from `ZCODE_ENV`, default
`production`, matching the Harness launch default).

### 4.2 Sidecar: controller (`src/remoteWorkspace.ts` + helpers)

- **Window adapter**: one window (`id` 1, `webContents.id` 1). `webContents.send(channel, payload)`
  and `webContents.postMessage(channel, payload, [port])` become `rw.evt` frames on the current
  base attachment. The window never reports destroyed (Harness has one window per sidecar and
  the launcher's lifetime is the window's); without a base attachment events are dropped, as
  Electron drops `webContents.send` during a reload.
- **Host adapter** (`windowHostProcessMap.get(1)`): `postMessage(msg, ports)` →
  `shim.emitHostMessage(msg, ports)`; `on("message")` receives every Host→Main message;
  `once("exit")` fires on launcher shutdown; `pid` = launcher pid.
- **In-process channel**: `createMessageChannel` returns two linked MessagePortMain-like ports
  (`on/off("message", ({data}) => …)`, `postMessage`, `start`, `close`, `once("close")`). `port2`
  goes to the Host with `attach-service-port`; `port1` is either delivered to the renderer
  (4.3) or returned to a Phase 4B caller.
- **Interception**: the parentPort shim's `postMessage` is wrapped. Every Host message is offered
  to the Host adapter listeners; these types are consumed by the controller and **not** forwarded
  to Rust: `remote-workspace-connection-log`, `-connected`, `-connect-failed`, `-context-bound`,
  `-context-bind-failed`, `-reconnected`, `-closed`, `project-workspace-prepared`,
  `provider-provisioning-source-changed`, `provider-provisioning-execution-result`,
  `provider-provisioning-lane-request` (answered from the coordinator exactly like
  `desktopHostProcess.ts`, so Rust's lane-`none` fallback is no longer used). Everything else,
  including `remote-workspace-acquired` (Phase 4B), still goes to Rust.
- **init-local**: the sidecar adds `remoteAssets` to an `init-local` that lacks it (ZCode Main
  does the same at spawn) before the Host sees it.

### 4.3 Control channel (text frames on the base attachment)

Renderer → sidecar: `{"t":"rw.req","id":n,"method":M,"params":P}`.
Sidecar → renderer: `{"t":"rw.res","id":n,"ok":true,"result":R}` or
`{"t":"rw.res","id":n,"ok":false,"error":{"message","code?"}}`, and events
`{"t":"rw.evt","event":E,"payload":P}`.

| Method | ZCode IPC equivalent | Result |
| --- | --- | --- |
| `connectRemote` `{target, requestId?, workspacePath?, workspaceIdentity?, connectTrigger?, confirmSourceServerInstall?}` | `ConnectRemote` | `{success, sessionId?, reusedConnectionTarget?, error?, reasonCode?, retryable?, profileId?, resolvedWslTarget?}` |
| `cancelPendingRemoteConnection` `{requestId?}` | `CancelPendingRemoteConnection` | `null` |
| `bindRemoteWorkspaceSessionContext` `{remoteSessionId, workspacePath, workspaceIdentity?, workspacePurpose?}` | `BindRemoteWorkspaceSessionContext` | `BindRemoteWorkspaceSessionContextIpcResult` |
| `disposeRemoteSession` `{sessionId}` | `DisposeRemoteSession` (grace 150 ms) | `null` |
| `isDockerAvailable` / `listWSLDistros` / `listDockerContainers` / `listSSHConfigAliases` | same names | `boolean` / arrays (failure → `false` / `[]`) |
| `scopedPortReady` `{sessionId, attachmentId}` | `ScopedServicePortReady` | `null` |

Events: `remote-connection-log`, `remote-session-closed`, `remote-session-reconnected`
(payloads exactly as the ZCode `PlatformChannels` events), and `scoped-service-port`
`{payload: ScopedServicePortPayload, token}`.

Requests are accepted only from local `desktop-continuous` (base) attachments; a request on any
other attachment (e.g. a remote-scoped one) is answered `ok:false` and not executed. Unknown
methods answer `ok:false`; while the vendored manager failed to load every request answers
`ok:false` with `REMOTE_WORKSPACE_UNAVAILABLE`.

### 4.4 Scoped attachments

For `webContents.postMessage(ScopedServicePort, payload, [port1])` the controller registers a
pending grant `{attachmentId: payload.attachmentId, token: random 32 bytes hex, ttl 30 s}` in the
same attachment registry Rust grants use, and sends `scoped-service-port` with the token. When the
renderer's WebSocket hello claims it, the WS virtual port is piped to `port1` (binary frames and
`connection-flow-v1` control both ways). Closing either side closes the other. The attach broker
ignores attachments it does not own (no `attach.active` / `attach.closed` to Rust for them).

### 4.5 Renderer bridge (`apps/harness-app/src/host/remoteWorkspace*.ts`)

- `bindPort` hooks onto the base `HostMessagePort` (text frames `rw.res` / `rw.evt`) as soon as
  it is created, because scoped ports re-issued after a reload can precede the app root;
  `setBaseServices` (on entering the app root) registers the base services
  (`registerBaseWorkspaceServices`) and flushes the buffered scoped ports.
- On `scoped-service-port`: opens `createHostMessagePort({url, token, attachmentId})`, builds
  services with a `RemoteServiceAccess` over it, applies the ZCode override map
  (`buildRemoteWorkspaceSessionServicesForRegistration`, copied from the private ZCode renderer
  module), calls `registerRemoteWorkspaceSession`, then sends `scopedPortReady`.
- `tauriPlatform.ts` methods map 1:1 to the control methods; bind results go through
  `unwrapRemoteWorkspaceBindResult`. `allowRemoteWorkspace` is enabled on desktop.

## 5. Event order (connect + bind + attach)

```mermaid
sequenceDiagram
  participant UI as Renderer (shared UI)
  participant BR as Renderer bridge
  participant SC as Sidecar controller (vendored Main manager)
  participant H as Host (windowRemoteConnectionRegistry)
  participant R as Remote source (WSL/SSH/Docker)
  UI->>BR: connectRemote(target, requestId, {workspacePath, workspaceIdentity})
  BR->>SC: rw.req connectRemote (base WS)
  SC->>SC: resolveWslTarget (WSL only), pending[requestKey]
  SC->>H: connect-remote-workspace {requestId, target, remoteAssets, workspacePath, workspaceIdentity}
  H-->>SC: remote-workspace-connection-log* (requestId)
  SC-->>BR: rw.evt remote-connection-log
  H->>R: deploy / handshake
  H-->>SC: remote-workspace-connected {descriptor: remoteSessionId, workspacePath, workspaceIdentity, sourceServerId?}
  SC->>SC: route[remoteSessionId]; provisioning initialSync barrier
  SC->>H: attach-service-port {scope: remote, remoteSessionId, workspacePath, workspaceIdentity} + port2
  SC-->>BR: rw.evt scoped-service-port {payload(attachmentId, sessionId, …), token}
  BR->>SC: WS hello(attachmentId, token) → piped to port1
  BR->>UI: registerRemoteWorkspaceSession(services)
  BR->>SC: rw.req scopedPortReady {sessionId, attachmentId}
  SC-->>BR: rw.res connectRemote {success, sessionId}
  UI->>BR: bindRemoteWorkspaceSessionContext(canonical workspace)
  BR->>SC: rw.req bind → bind-remote-workspace-context → context-bound → new attachment generation (same steps) → rw.res
```

Ordering guarantees: the `scoped-service-port` event is sent before the `connectRemote` result
(the manager awaits the ready barrier before resolving); a renderer reload (new base attachment)
triggers `reattachRemoteWorkspaceSessionsForWindow(win, "renderer-reload")` after the new base
attachment is active, and scoped ports that arrive before the base services exist are buffered by
the bridge (same as ZCode `pendingRemoteWorkspaceServicePorts`).

## 6. Rust boundary

- No protocol or classification change is required: the remote Host messages no longer reach
  Rust (4.2). The `Unsupported` rows stay as a defensive fallback.
- No `remote_workspace*` Tauri command is needed: every native concern of the flow (spawning
  `wsl.exe`, `docker`, `ssh2`, reading `~/.ssh/config`) already runs in Node inside the Host or
  the vendored discovery code, and the transport is the existing loopback WS. Adding Rust
  commands would create a second write path to the same route table.
- Rust bridge: remote-scoped attachments bypass `decoratePort` routing (R5). Verified by the gate
  with `HARNESS_RUST_SERVICES=all` and the bridge trace (no routed request for the remote
  attachment id).

### 6.1 Why not a Rust/russh port

Not worth it in Phase 4: the SSH/WSL/Docker transports, deploy, source-server handshake, reconnect
and provisioning are ~20k lines of TS shared with the ZCode Host and server, with their own
release cadence; a russh port would duplicate them (two implementations of one protocol, R1
violation risk), still need Node on the remote side, and gain nothing measurable because the
remote path is network-bound. Revisit only if the Host itself moves to Rust.

## 7. Remote assets

Computed by the sidecar per connect (and once for `init-local`), mirroring
`resolveRemoteAssetDirs`:

- `remoteCdnBaseUrls` from `resolveRemoteCdnBaseUrls({locale, env: ZCODE_ENV, overrideBaseUrl:
  ZCODE_REMOTE_ASSET_CDN_BASE_URL, version: ZCODE_VERSION})`; locale from `HARNESS_LOCALE`.
- `remoteCacheDir`: `ZCODE_REMOTE_ASSET_CACHE_DIR`, else `<ZCODE_DATA_BASE_DIR or
  ~/.harness>/remote-assets-cache`.
- `mockCdnDir` + `sourceServerArtifactPolicy: "development-content-addressed"` only when
  `packages/desktop/mock-cdn/releases/<ZCODE_VERSION>` exists (dev bootstrap); otherwise the
  release-versioned CDN path. Deviation from ZCode dev mode, which sets the development policy
  even without mock-cdn and then fails SSH with "run pnpm bootstrap:with-remote".

## 8. Fail-closed rules

| Condition | Behaviour |
| --- | --- |
| No base attachment when a request arrives | impossible by construction (requests come on it) |
| Base attachment closes (renderer reload) | as in ZCode's `renderer-reload` path, sessions and pending connects are **not** cancelled; events and scoped ports sent while no base attachment exists are dropped (the scoped `port1` is closed, so the manager's ready timeout detaches it). When the next base attachment is dispatched the controller calls `reattachRemoteWorkspaceSessionsForWindow(win, "harness:renderer-reload")`. The renderer bridge rejects its pending requests |
| Scoped grant not claimed in 30 s / ready not confirmed in 15 s | manager detaches the port (ZCode timeout path) and the connect/bind fails |
| Host exits | Host adapter `exit` listeners fire; manager rejects pending executions |
| Invalid payload | structured `{success:false,error}` (connect) / bind failure / thrown error → `ok:false` |
| Lane request | always answered (`none` on coordinator error) |

## 9. Seam for Phase 4B (phone on a remote workspace)

`installRemoteWorkspaces` (`src/remoteWorkspace.ts`) exposes:

- `manager()` (null until `ready`), including `attachRemoteWorkspaceSessionHost`,
  `prepareProjectWorkspace`, `cancelPendingRemoteWorkspaceSessionsForWindow`;
- `prepareProjectWorkspace(project, requestId)` / `cancelProjectPreparation(requestId, reason)`
  bound to window 1 (Main's `BrowserWindow.fromId(windowId)` lookups);
- `webRemoteControlManagerRef`, which the web remote adapter fills with its manager as soon as it
  is created (ZCode `index.ts` does `webRemoteControlManagerRef.current = webRemoteControlManager`);
- `stats()`: gate-only counters (`connectRequests` = `connect-remote-workspace` messages posted to
  the Host, `connectedEvents`, `remoteScopedAttachRequests` by `clientMode`, and the manager's
  `getRemoteConnectionStats()`). Counts only; no targets or credentials.

`src/remoteWorkspaceWebRemote.ts` is the Harness counterpart of ZCode's
`webRemoteControlSharedHostAttachments.ts` for remote targets: it validates the bridge context
like ZCode, calls `attachRemoteWorkspaceSessionHost`, and keeps the phone attachments per
`remoteSessionId` so `disposeRemoteSession(remoteSessionId)` / `releaseAttachment` /
`disposeWindow` can close them. `attachRemoteWorkspaceSessionHost` returns `{process, port,
remoteKind}` where `port` is the in-process `port1`; the web remote manager wraps it as it wraps an
Electron `MessagePortMain`. The phone attachment is one more remote-scoped attachment on the
existing logical session: no `connect-remote-workspace`, no new route.

`src/remoteTargets.ts` answers the phone's discovery `platform-request`s with the same vendored
`isDockerAvailable` / `listWSLDistros` / `listDockerContainers` /
`listSSHConfigAliasesFromLocalConfig` this spec's control channel uses (section 4.3).

Fail closed (Harness addition): when the renderer disposes a session (`disposeRemoteSession`),
the controller calls `webRemoteControlManagerRef.current?.failRemoteSession(sessionId,
"harness:desktop-disposed", {reason: "workspace-closed"})` after the session manager's dispose.
The phone gets `workspace-bridge-error`, its attachments are released, and a later bridge open for
the same workspace fails with `REMOTE_SESSION_MISSING` because the route is gone. Auto-reconnect
replacement is untouched (it still goes through ZCode's `replaceRemoteSession`).

## 10. Acceptance scenarios

| ID | Scenario | Evidence |
| --- | --- | --- |
| A1 | Discovery through the control channel: `listWSLDistros` lists installed distros; `isDockerAvailable` false without Docker; `listSSHConfigAliases` returns an array | gate |
| A2 | WSL connect through the real Host sidecar: `connectRemote` succeeds, a `scoped-service-port` is claimed, `scopedPortReady` confirmed | gate |
| A3 | Bind to a temp directory inside the distro; on the new remote attachment `file` read-dir lists the seeded files, `git` status reports the seeded repo, `terminal` create + write + output round-trips | gate |
| A4 | The remote attachment produced no Rust bridge routed request (`HARNESS_RUST_SERVICES=all`) | gate |
| A5 | `disposeRemoteSession` closes the session and its scoped attachment sockets close (renderer-initiated dispose emits no `remote-session-closed`, as in ZCode); a scoped attachment cannot send `rw.req`; temp directory removed | gate |
| A6 | Docker: same as A2-A5 with a gate-created container, else SKIP with reason | gate |
| A7 | SSH: localhost sshd only, else SKIP with reason (never remote hosts or user keys) | gate |
| A8 | Phase 2/3 gates unchanged (`PHASE2_E2E_OK`, `PHASE3_DIFF_OK`, `PHASE3B_DIFF_OK`) | gates |
| A9 | Phone on the desktop's WSL workspace, phone discovery listings, and fail closed after the desktop disposes the session | `phase4-web-remote.mjs` W9-W11 (phase-4-web-remote-control.md) |

Gate: `node Worse/scripts/e2e/phase4-remote.mjs` prints `PHASE4_REMOTE_OK` plus a JSON summary,
or `PHASE4_REMOTE_FAIL <step>`. The gate isolates `HOME` (and `ZCODE_DATA_BASE_DIR`) for the
sidecar, because ZCode's `setting.json` is resolved from `HOME`/`USERPROFILE` and a connect
registers a Source Server and connection profile there. The WSL remote runtime the Host deploys
lives in the distro user's `~/.zcode` and is left in place (set `PHASE4_REMOTE_CLEAN_RUNTIME=1`
to remove home entries the run created).
