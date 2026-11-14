# Plan: Android APK client + Cloudflare tunnel for Harness (desktop Slint app)

Status: proposal. No code written. Spec to author after acceptance:
`docs/specs/harness/phase-12-remote-phone.md`.

Two features:
- **A. Android APK** that drives the *running desktop* Harness (never its own hub/agents).
- **B. Cloudflare tunnel** so the phone (or any cloud client) reaches that desktop without a
  port-forward, using the local `cftunnel` helper.

## 1. Current state

### 1.1 What remote surface exists today

| Surface | Transport | Auth | Where |
| --- | --- | --- | --- |
| Hub delegate RPC (agent → subagents) | raw TCP, one JSON line per connection, `127.0.0.1:0` | random 128-bit token per run, `HARNESS_DELEGATE_TOKEN` | `crates/harness-agent-hub/src/delegate_server.rs:27`, `:29`, `:34`; `docs/specs/harness/phase-9-parity.md:905-910` |
| Hub delegate stdio MCP | MCP over stdio (`harness-slint --delegate`) | token env | `apps/harness-slint/src/main.rs:279-290` |
| Host-node sidecar attach (old Tauri app only) | WS `ws://127.0.0.1:<port>/v1/attach` | single-use token, 30 s TTL | `crates/harness-host-protocol/src/lib.rs:39`, `:48`; `crates/harness-host-control/src/attach.rs:14`, `:51`; server `sidecars/host-node/src/wsServer.ts:167-199` |
| ZCode cloud relay (old Tauri app only) | vendored ZCode relay WS | the user's ZCode login | `docs/specs/harness/phase-4-web-remote-control.md:147-175`, `:76-78` |

**No HTTP API, no JSON-RPC endpoint for a UI-shaped client.** `apps/harness-slint` calls
`HarnessCore` in-process only (`crates/harness-core-api/src/lib.rs:86`); the trait was built for this
extension — "the same calls can later be carried over a remote transport" (`:4`). API `0.17.0`
(`:72`), 54 `EventKind`s (`crates/harness-core-api/src/events.rs:63`). Already promised, not built:
`phase-8-core.md:362-367` (crate `harness-remote`, authenticated WS, pairing token, snapshot + `seq`)
and `phase-9-parity.md:825-832` (HTTP/WS server exposing `HarnessCore`, pairing tokens, QR; phone
reuses the existing hub session). **This plan implements those sections; it invents nothing.**

### 1.2 What is missing

1. No listener serving `HarnessCore` + `EventStream` to an out-of-process client.
2. No pairing/token store, revoke, origin/host check, rate limit.
3. No tunnel supervision (spawn/stop `cftunnel`, discover the public URL).
4. No phone-facing UI, no static asset serving in the desktop app.
5. No Android target; `apps/harness-slint/Cargo.toml:53-61` pulls desktop-only deps (`xcap`,
   `enigo`, `rfd` with `gtk3`, `alacritty_terminal`, `clipboard-win`, `windows-sys`), and
   `src/workarea.rs` is Win32-only.
6. Subagent data exists (`SessionDetail.subagents`, `EventKind::SubagentUpdated`, `events.rs:212`)
   but has no remote consumer.

## 2. Architecture recommendation for the APK

| Option | Effort | Reuse | Blockers |
| --- | --- | --- | --- |
| (a) Slint Android build of `apps/harness-slint` as a remote client | 4-6 weeks | low: 195 KB `ui/app.slint` desktop layout, Win32 `workarea.rs`, `xcap`/`enigo`/`rfd(gtk3)`/`clipboard-win`, `live_update` shadow copy, tray, `rfd` picker | every desktop dep needs a cfg or removal; needs `backend-android-activity-05` + `android-activity` + NDK + `cargo-apk`; desktop layout unusable at 400 dp; APK rebuild per UI tweak |
| (b) **Thin Android shell loading a web remote UI served by Harness** | 1-2 weeks | new crate serves static assets + JSON-RPC/WS; `hyper`/`http-body-util` already in `Cargo.lock` (via reqwest); one `index.html` + small JS client | needs Android SDK for the wrapper APK; phone UI is new (small) |
| (c) PWA only (no APK) | 2 days after (b) | same web UI, "Add to home screen" | no store-style install, no reliable push |

**Recommendation: (b) in two steps — (b1) web UI served by the desktop (usable today in any phone
browser / as a PWA), then (b2) a ~300-line Kotlin WebView APK whose only job is "open the paired
URL".** Reasons:
- The phone can never run the hub (agent CLIs, PTY, shadow copy are desktop). It is a *client* and
  should not carry the desktop dependency tree.
- UI updates ship from the desktop (`GET /`); the APK changes only when pairing/push plumbing does.
- Reuses the serializable in-repo contract (`lib.rs:4`) instead of a second Slint view model.
- `apps/harness-slint` is untouched except a new `remote` module: no risk to the desktop build.
- Escape hatch: (a) stays possible; a native client would talk to the same server layer.

Rejected: rewriting the Slint UI for phones on Android — highest cost, lowest reuse, and it needs the
same server + tunnel work anyway.

## 3. Tunnel design

Tool on disk: `cftunnel.exe` (66 MB single file, cloudflared embedded; `cftunnel` also
resolves via `PATHEXT`). It is **not** plain cloudflared — its own CLI:

```
Usage: cftunnel http <port> [--name <subdomain>]
```

`cftunnel --version` prints that usage and exits 1, so a supervisor must not rely on `--version`.

### 3.1 Lifecycle

- `apps/harness-slint/src/remote/tunnel.rs` owns one child process.
- `start(port)`: spawn `settings.remote.tunnel_bin` (append `.exe` on Windows when the configured
  path has no extension) with `["http", <port>]`, plus `["--name", <subdomain>]` in named mode;
  `stdout`/`stderr` piped, `CREATE_NO_WINDOW`.
- URL discovery: scan both pipes line by line for `https://[a-z0-9-]+\.trycloudflare\.com` (named:
  `https://<subdomain>.<zone>`), 30 s timeout → `Error` with the last 20 output lines.
- States `off | starting | up { url, since } | error { message }` drive the Settings page and QR.
- `stop()`: kill the child tree (`taskkill /T /F`, pgid elsewhere — the hub's own agent pattern,
  `phase-8-core.md:354-360`), join the readers. Child death flips state to `error`, never silent.
- Tunnel is **off by default**, started from Settings; auto-start is opt-in (open question 5).

### 3.2 Configuration

New `RemoteSettings` in `crates/harness-settings/src/model.rs` (next to `proxy` at `:193`, field on
`Settings` at `:13`; unknown fields already default):

`RemoteSettings { enabled: bool, port: u16 /*7878*/, tunnel_bin: String /*"cftunnel"*/,
tunnel_mode: Quick | Named { subdomain }, auto_start_tunnel: bool /*false*/,
allow_lan: bool /*false*/ }`.

Binary resolution: setting → `HARNESS_TUNNEL_BIN` → `cftunnel` (Windows) → `cloudflared` on
`PATH`. "Browse…" plus "Test binary" (spawn with no args, expect exit 1 + usage text) makes a wrong
path obvious without starting a tunnel. **Quick** (`cftunnel http <port>`): zero setup, random
hostname, new URL per start — the default. **Named** (`--name <subdomain>`): stable host, needs the
user's Cloudflare login/zone. Same supervisor; different flags and URL regex.

### 3.3 Pairing

- Pair URL `https://<host>/p#<token>` — token in the **fragment**, so it never reaches the tunnel
  edge log or a Referer. `GET /p` returns the shell page; JS reads `location.hash`, calls
  `POST /api/v1/pair {token}`, server sets `HttpOnly; Secure; SameSite=Strict` and the WS then
  authenticates by cookie (browsers cannot set headers on `new WebSocket`).
- Settings → Remote shows (i) the URL + Copy, (ii) a QR if `qrcode` can be added, (iii) an
  8-character short code as fallback (typed on the phone). **Short code keeps phase 1 dependency-free.**
- QR: `qrcode` crate → RGBA matrix → `slint::Image::from_rgba8` in `ui/settings/remote.slint`.
- Token: 32 random bytes hex in `<HARNESS_HOME>/remote.json` (`0600`, atomic write), one row per
  device (`{id, name, created_ms, last_seen_ms}`), max 8. Revoke deletes the row and closes that
  device's sockets.

### 3.4 Security

- **Never unauthenticated.** Everything except `GET /` (static shell, no data) and `GET /healthz`
  needs the device token; otherwise 401 with no body.
- Constant-time compare; 10 failed auths per IP per minute → 429 + 5-minute lockout.
- `Host` must be the current tunnel host (or `127.0.0.1:<port>`) → blocks DNS rebinding; `Origin`
  must match the tunnel origin on `POST`.
- Body cap 1 MB, one JSON object per request, `/rpc` method allowlist (§4.1); assets come from
  `include_str!`/`include_bytes!`, so no path traversal.
- TLS is Cloudflare's; the local hop is loopback HTTP. `0.0.0.0` only when `allow_lan`.
- Tokens, pair codes and URLs-with-fragments are never logged (same rule as
  `phase-4-web-remote-control.md:76-80`). The tunnel exposes only this server: the delegate TCP
  server (`delegate_server.rs:27`) and the host-node sidecar are not routable through it.

## 4. Subagents over the tunnel

### 4.1 Transport (phase 1, all under `/api/v1`)

| Route | Purpose |
| --- | --- |
| `POST /api/v1/pair` | `{code \| token}` → sets cookie |
| `GET /api/v1/status` | `{version, api_version, devices, tunnel, hub_ready}` |
| `POST /api/v1/rpc` | `{method, params}` → `HarnessCore` call; `{ok, value \| error}` |
| `GET /api/v1/events` (WS) | `HubEvent {seq, session_id, kind}` from `HarnessCore::subscribe` (`lib.rs:724`); first frame `{hello, seq}`; on `RecvError::Lagged` the client re-`open_session`s (`events.rs:244`) |
| `POST /api/v1/devices/revoke` | remove a device |

`/rpc` allowlist = what a phone needs: `list_agents`, `list_sessions`, `open_session`,
`send_message`, `stop`, `cancel_queued`, `respond_permission`, `set_active_agent`, `set_mode`,
`set_model`, `rename_session`, `set_pinned`, `find_in_session`, `list_projects`, `turn_diff`,
`memory_read`. **Not** exposed: `shutdown`, `import_*`, `archive_*`, file writes. The server is a
second *consumer*, not an attachment: it never touches host-node attachments, so
`desktop-continuous` / `web-remote-replayable` stay untouched (`phase-4-web-remote-control.md:60-71`).

### 4.2 Seeing and driving subagents

Already in the API — no hub change needed for MVP:
- List: `list_sessions` hides subagent rows (`crates/harness-agent-hub/src/store.rs:240`), so the
  phone lists parents, then `open_session(parent)` returns `SessionDetail.subagents` (newest first,
  `phase-9-parity.md:926`).
- Watch: `EventKind::SubagentUpdated { subagent }` arrives on the same `seq` stream with
  `session_id = parent` (`events.rs:212`), carrying `status`, `activity` (current tool title),
  `started_at`, `result_preview`. Live transcript of a running child = `open_session(child)` plus
  the child's own `MessageDelta`/`ToolCall` events, which the stream already broadcasts.
- Drive: `send_message(child)` (queued next turn; interrupt = `stop` then send), `stop(child)`,
  `respond_permission(child, …)`.

Optional additive API (0.18.0, phase 3): `list_subagents(ListSubagents { parent, status }) →
Vec<SubagentView>` so "All subagents" needs no N×`open_session` fan-out; and
`EventKind::SubagentFinished` with an explicit push payload. No transport change: `harness-delegate`
MCP stays agent-facing (`phase-9-parity.md:891-998`); the phone uses the same hub API as the desktop.

## 5. Phased implementation

### Phase 1 — remote server + auth + tunnel on the desktop (3-5 days)

Files:
- `Worse/crates/harness-remote/` (new crate, auto-membered by `crates/*`, `Worse/Cargo.toml:3`):
  `src/lib.rs` (start/stop handle), `src/http.rs` (routes, static assets, body cap, host/origin
  check), `src/auth.rs` (tokens, constant-time compare, rate limit), `src/rpc.rs` (allowlist →
  `HarnessCore`), `src/ws.rs` (event fan-out + seq), `src/assets/index.html`. Deps: already-locked
  `hyper`, `http-body-util`, `tokio`, `serde_json`, `sha2`, `base64`; hand-roll the RFC 6455
  read-only text frames (~200 lines) rather than add `tokio-tungstenite`.
- `Worse/crates/harness-settings/src/model.rs`: `RemoteSettings`.
- `Worse/apps/harness-slint/src/remote/{mod,tunnel,qr}.rs`: Slint bridge + state, `cftunnel`
  supervisor, QR matrix → `slint::Image`.
- `Worse/apps/harness-slint/src/main.rs`: `mod remote;` + start the server with the hub handle.
- `Worse/apps/harness-slint/src/core/hub.rs`: expose `Arc<dyn HarnessCore>` + `EventStream`
  (`:442` `Hub::start`, `:485` `subscribe`) to `remote`.
- `Worse/apps/harness-slint/src/settings/mod.rs:478`: `"remote" => …` page open.
- `Worse/apps/harness-slint/ui/settings/remote.slint` (new) + nav entry in `shell.slint`.
- `Worse/scripts/e2e/phase-remote.mjs` gate, `Worse/docs/specs/harness/phase-12-remote-phone.md`.

Done when: `HARNESS_UI_CORE=fake harness-slint` serves `127.0.0.1:7878`; `GET /` loads; wrong token
401; right token runs `rpc list_sessions` and receives `EventKind` frames with increasing `seq`; the
Settings toggle flips `off → starting → up {url}` against a **stub** tunnel
(`scripts/e2e/fixtures/fake-cftunnel.cmd`) and never contacts the real `cftunnel`; revoke drops
the device socket within 1 s.

Tests: unit — token compare (wrong/empty), rate limit, host/origin check, allowlist rejection, 1 MB
cap, `Lagged` → snapshot reopen, tunnel URL regex on sample output, `.exe` suffix resolution. Gate —
`node Worse/scripts/e2e/phase-remote.mjs` prints `PHASE_REMOTE_OK` (fake core + stub tunnel, no
network).

### Phase 2 — phone UI + APK (4-6 days)

- Phone web client in `harness-remote/src/assets/`, no build step: session list, streaming chat,
  permission cards, queue, stop, model/mode pickers, subagent panel. Target < 100 KB, usable at
  360 dp, one stylesheet with light/dark.
- PWA manifest + service worker (cached shell, offline "reconnect" screen) → installable today.
- APK (b2) at `Worse/apps/harness-mobile/`: Kotlin, one `MainActivity` with `WebView`
  (`setJavaScriptEnabled`, `domStorageEnabled`, no file access), a `PairingActivity` (QR via ML Kit
  barcode, or the short code), URL in `EncryptedSharedPreferences`, `SwipeRefreshLayout` reload,
  back button → WebView history, `usesCleartextTraffic="false"`. Gradle project, not a workspace
  member. If Android SDK is unavailable, ship the PWA and skip the APK.

Done when: a phone on mobile data pairs by QR/short code, lists sessions, sends a turn, sees the
stream, answers a permission card, and the desktop UI shows the same state (one hub, one session, no
second agent). Tests: CDP run of the web client in mobile emulation against the fake core
(`phase-remote.mjs` step `web-client`); `adb install` + screenshot, else `PHASE_MOBILE_PARTIAL`.

### Phase 3 — subagents + notifications (2-3 days)

- "Subagents" screen: all parents → children, status/activity/elapsed, tap = child transcript,
  actions message / stop / approve.
- `list_subagents` (§4.2) if the `open_session` fan-out proves slow.
- Notifications, in preference order: (1) desktop webhook to an ntfy.sh topic consumed by the APK
  foreground service, (2) FCM if the user has a Firebase project, (3) in-app only. Fired on turn
  finished, subagent finished, permission requested.

Done when: a subagent started on the desktop appears on the phone within 1 s, its activity line
updates, `stop` from the phone cancels it, and a finished subagent raises exactly one notification.

## 6. Risks and open questions

Risks:
1. **`cftunnel` output contract is unverified** (no tunnel started during planning): the URL regex
   and flag set come from usage text. Mitigation: a "Raw output" expander in Settings, "no URL in
   30 s" is a first-class error, and phase 1 step 1 is a manual `cftunnel http <port>` run by the
   user to capture real output.
2. **Quick-tunnel URL changes every start** → the phone's stored URL dies. Mitigation: store the
   device token and re-pair by QR, or named mode for a stable host.
3. **New crates** (`qrcode`, maybe `tokio-tungstenite`) need registry access; `index.crates.io`
   answered 200 today, but `STATUS.md` records an earlier TLS failure here. Mitigation: `qrcode` is
   optional (short code), WS frames hand-rolled.
4. **Android toolchain verified present** (§7.4): `adb` at `C:\platform-tools\adb.exe`,
   `ANDROID_HOME=%LOCALAPPDATA%\Android\Sdk`, NDK `27.1.12297006` and `30.0.16248370`,
   JDK 21 (`Android Studio\jbr` + Adoptium), rustup android targets installed, **no `gradle` on
   PATH** → the project must check in a Gradle wrapper.
5. Scope creep: terminal, files, git and diff review are reachable through the same API and would
   double phase 2. Excluded from MVP.
6. Security: a public hostname plus one bearer token equals full agent control (agents run shell
   commands). Mitigations in §3.4, 8-device cap, revoke, permanent "tunnel is up" badge.

Open questions for the user:
1. Quick tunnel (random URL, no account) or named tunnel (stable URL, needs Cloudflare login + a zone
   on your account)? Default here: quick.
2. Android toolchain is present (§7.4), so a real APK is the default: Kotlin WebView wrapper, or do
   you want the PWA only for v1?
3. v1 phone scope: sessions + chat + approvals + subagents only, or terminal / files / git too?
4. Push: ntfy.sh topic (~5 min setup), Firebase project (needs your Google account), or in-app only?
5. Start the tunnel at launch, or only from the Settings toggle?
6. Also reachable on the LAN (phone + desktop on one Wi-Fi)? Default here: loopback only.

## 7. Parallel work split (4 subagents, phase 1 first)
> **SUPERSEDED — do not follow this section.** The binding work split and API contract is `docs/plans/remote-contract.md`. Ignore §7.1/§7.2 names (e.g. `info`, `pair_url`, `ev` frames) where they differ.

Priority: **W1 + W2 are phase 1 and land first.** W3 and W4 start immediately against the frozen
contract below (W3 against a mock server it owns, W4 against the pair-URL format only) and merge
after W1/W2.

### 7.1 Frozen interface (agreed before any code; nobody changes it unilaterally)

Crate/module: `crates/harness-remote`, public surface (W1 owns, others consume):

```rust
pub struct Config { pub port: u16, pub bind_lan: bool, pub home: std::path::PathBuf }
pub struct DeviceInfo { pub id: String, pub name: String, pub created_ms: u64, pub last_seen_ms: u64 }
pub struct ServerInfo { pub port: u16, pub api_version: &'static str, pub devices: Vec<DeviceInfo> }
pub struct RemoteHandle;
impl RemoteHandle {
    /// Spawns the HTTP/WS server on its own task; returns once the socket is bound.
    pub async fn start(core: std::sync::Arc<dyn harness_core_api::HarnessCore>,
                       events: harness_core_api::EventStream, cfg: Config) -> std::io::Result<Self>;
    pub fn info(&self) -> ServerInfo;
    pub fn pair_url(&self, token: &str) -> String;   // https://<host>/p#<token> (host filled by UI)
    pub async fn revoke(&self, device_id: &str) -> harness_core_api::ApiResult<()>;
    pub fn stop(self);
}
/// W3 (web client) owns the body; W1 only calls it.
pub mod assets;                     // src/assets/mod.rs
pub fn get(path: &str) -> Option<(&'static str, &'static [u8])>;  // (content-type, body)
```

HTTP contract (exact, versioned by `api_version` = `harness_core_api::API_VERSION`):

| Route | Request | Response |
| --- | --- | --- |
| `POST /api/v1/pair` | `{token?: string, code?: string, name?: string}` | `200 {device_id}` + `Set-Cookie: harness_device=<id>.<secret>; HttpOnly; Secure; SameSite=Strict; Path=/`; else `401 {error:{code,message}}` |
| `GET /api/v1/status` | — | `{version, api_version, hub_ready: bool, devices: [DeviceInfo]}` |
| `POST /api/v1/rpc` | `{method: string, params: object}` | `{ok:true, value}` \| `{ok:false, error:{code, message}}` |
| `GET /api/v1/events` | WS upgrade (cookie auth) | `{"t":"hello","seq":n,"api_version":"0.17.0"}` then `{"t":"ev","seq":n,"session_id":s\|null,"kind":{…EventKind…}}`; client may send `{"t":"ping"}` |
| `POST /api/v1/devices/revoke` | `{device_id}` | `{ok:true}` |
| `GET /`, `/app.js`, `/app.css`, `/manifest.webmanifest`, `/sw.js` | — | static, **no auth** |
| `GET /p` | — | pairing shell page, **no auth** |
| `GET /healthz` | — | `{ok:true}` |

Other frozen names: settings JSON key `"remote"` with `enabled`, `port`, `tunnelBin`,
`tunnelMode` (`"quick"`/`"named"`), `subdomain`, `autoStartTunnel`, `allowLan`; env
`HARNESS_TUNNEL_BIN`, `HARNESS_HOME`; `/rpc` allowlist = §4.1; gate markers `PHASE_REMOTE_OK`,
`PHASE_REMOTE_TUNNEL_OK`, `PHASE_REMOTE_WEB_OK`, `PHASE_REMOTE_APK_OK` / `…_APK_PARTIAL`.

Dependency rule: **only W1 and W2 add crates** (they own `crates/harness-remote/Cargo.toml` and
`apps/harness-slint/Cargo.toml`); W3/W4 add none. `Cargo.lock` may be touched by W1/W2 — regenerate,
never hand-edit. `Worse/Cargo.toml` needs no edit (members is `crates/*`, line 3).

### 7.2 Streams and file ownership (disjoint — do not edit outside your list)

**W1 — remote server + auth + protocol gate (phase 1, critical path).**
Files: `crates/harness-remote/{Cargo.toml,src/lib.rs,src/http.rs,src/auth.rs,src/rpc.rs,src/ws.rs}`,
`scripts/e2e/phase-remote.mjs`, `docs/specs/harness/phase-12-remote-phone.md`,
`docs/specs/harness/README.md`.
Step 0 (first hour, unblocks everyone): commit `lib.rs` with the §7.1 signatures and unimplemented
bodies. `src/assets/mod.rs` is a one-time handoff: W1 creates it with the placeholder `get` (needed
for `lib.rs` to compile) and from then on **W3 owns that file**.
Done: `cargo check -p harness-remote`; `cargo test -p harness-remote`; `cargo clippy -p harness-remote --all-targets -- -D warnings`;
`node Worse/scripts/e2e/phase-remote.mjs` prints `PHASE_REMOTE_OK` (fake core, ephemeral port, no
network): pair → cookie → `rpc list_sessions` → WS `ev` frames with increasing `seq` → wrong token
401 → 11th bad auth 429 → revoke drops the socket < 1 s → `Lagged` forces snapshot reopen.

**W2 — desktop integration: settings, Slint Remote page, `cftunnel` supervisor (phase 1).**
Files: `crates/harness-settings/src/model.rs` (+ its `store`/defaults if needed),
`apps/harness-slint/Cargo.toml`, `apps/harness-slint/src/remote/{mod.rs,tunnel.rs,qr.rs}`,
`apps/harness-slint/src/main.rs`, `apps/harness-slint/src/core/hub.rs`,
`apps/harness-slint/src/settings/mod.rs`, `apps/harness-slint/ui/settings/{remote.slint,shell.slint,globals.slint}`,
`scripts/e2e/fixtures/fake-cftunnel.cmd`, `scripts/e2e/phase-remote-tunnel.mjs`.
Done: `cargo test -p harness-settings`; `cargo build -p harness-slint --profile quick`;
`cargo clippy -p harness-slint --all-targets -- -D warnings`;
`node Worse/scripts/e2e/phase-remote-tunnel.mjs` prints `PHASE_REMOTE_TUNNEL_OK` (stub tunnel only,
real `cftunnel` never spawned); Settings toggle walks `off → starting → up {url}`, wrong binary
path shows a readable error, and the QR/copy/short-code trio renders in the page.

**W3 — phone web client served by the desktop (phase 2 UI, starts on mock).**
Files: `crates/harness-remote/src/assets/*` (`index.html`, `app.js`, `app.css`,
`manifest.webmanifest`, `sw.js`, `mod.rs` body — the placeholder from W1 becomes real),
`scripts/e2e/fixtures/mock-remote-api.mjs` (its own contract server),
`scripts/e2e/phase-remote-web.mjs`.
Done: `node Worse/scripts/e2e/phase-remote-web.mjs` prints `PHASE_REMOTE_WEB_OK` against the mock
(no W1 needed) and then against the real server: session list → send a turn → streaming reply →
permission card answered → queue/stop → subagent panel rows; total assets < 100 KB; usable at 360 dp;
no build step, no new dependency.

**W4 — Android APK (phase 2 APK, fully parallel).**
Files: `apps/harness-mobile/**` (Gradle wrapper, `settings.gradle.kts`, `app/build.gradle.kts`,
`MainActivity.kt`, `PairingActivity.kt`, manifest, resources), `scripts/e2e/phase-remote-apk.mjs`.
Inputs it needs: only the pair-URL format `https://<host>/p#<token>` and the routes in §7.1.
Done (toolchain verified on this machine, §7.4): `apps/harness-mobile/gradlew.bat :app:assembleDebug`
produces `app/build/outputs/apk/debug/app-debug.apk`; `adb install -r` succeeds when a device is
attached; the app opens the stored URL, survives rotation, blocks cleartext, and stores the URL in
`EncryptedSharedPreferences`. Without a device: `node Worse/scripts/e2e/phase-remote-apk.mjs` prints
`PHASE_REMOTE_APK_PARTIAL <reason>` after a green Gradle build; a buildable skeleton with the
pairing screen is acceptable delivery, a red Gradle build is not.

### 7.3 Coordination rules

1. Claim paths before editing (`claim_paths`); a denied path is a message, not a workaround.
2. Cross-stream needs go through the §7.1 contract or `notify_parent` — never a shared file.
3. If the contract must change: W1 edits §7.1 in this plan first, then notifies the others; code
   follows the plan, never the reverse.
4. Integration order: W1 skeleton → W2 + W3 (mock) + W4 in parallel → W1 real server → W3 switches
   from mock to real → all four gates green → `cargo clippy --workspace --all-targets -- -D warnings`.

### 7.4 Toolchain facts checked for this machine

`where adb` → `C:\platform-tools\adb.exe`; `ANDROID_HOME=%LOCALAPPDATA%\Android\Sdk`;
NDK `27.1.12297006`, `30.0.16248370`; `JAVA_HOME=C:\Program Files\Android\Android Studio\jbr` plus
Adoptium JDK 21; `rustup target list --installed` → `aarch64-linux-android`,
`armv7-linux-androideabi`, `i686-linux-android`, `x86_64-linux-android`, `x86_64-pc-windows-msvc`;
**no `gradle` on PATH** → W4 checks in the wrapper (`gradle-wrapper.jar` + `gradlew.bat`). The APK is
therefore not blocked; the Slint-Android path stays rejected for the reasons in §2.
