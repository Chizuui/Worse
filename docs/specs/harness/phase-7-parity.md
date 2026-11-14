# Phase 7 (B): functional parity, rollback and kill switch

Status: active implementation contract. Gate: `Worse/scripts/e2e/phase7-parity.mjs` (modules in `Worse/scripts/e2e/phase7-parity/`). Report: `Worse/.work/phase7/parityB.md`.

Phase 7 of the [plan](../../../../docs/plans/harness-rust-tauri-v2-plan.md) needs "functional parity tests" and "a kill switch that routes a user back to ZCode without deleting Harness data". This spec covers both. Performance parity is Phase 7 (A) (`phase7-perf.mjs`) and is not covered here.

## 1. Product rules

1. Harness and ZCode share the same user data: `~/.zcode/v2/setting.json`, `provider_config.json`, `credentials.json`, `tasks-index.sqlite`, and `~/.zcode/cli/db/db.sqlite` (the agent session store). Harness runs ZCode's own Host and CLI, so it writes the same formats. Harness never migrates, rewrites or deletes these files beyond what the ZCode Host itself does.
2. Harness-only data stays in Harness locations: `%APPDATA%`/`%LOCALAPPDATA%` `dev.harness.app` (device id, updater state, WebView2 profile, logs) and `~/.zcode/harness/` (browser profile, kill-switch marker). ZCode ignores them.
3. Going back to ZCode never requires deleting Harness data, and coming back to Harness after using ZCode loses nothing (§6).
4. Running both apps at the same time on the same data is not supported (unchanged from `phase-6-packaging.md` §6.4). The rollback test runs them one after the other.

## 2. Test isolation (hard rules)

- The gate calls `installIsolatedHome("phase7-parity")` first. Every process it starts inherits the isolated `HOME`, `USERPROFILE`, `APPDATA`, `LOCALAPPDATA`, `XDG_*`, `ZCODE_DESKTOP_HOME_DIR` and `ZCODE_DATA_BASE_DIR`. The `HOME_LEAK` guard must stay green.
- No real provider, no real ZCode cloud: `ZCODE_ENDPOINT_ORIGIN` and `ZCODE_WEB_REMOTE_CONTROL_RELAY_WS_URL` point at a local server that returns 404 for everything ("dead cloud"). The model provider is the local mock in §3.
- Both apps keep their browser profile in the isolated home: Harness through `WEBVIEW2_USER_DATA_FOLDER=<home>/harness-webview2`, ZCode through Electron `userData` under the isolated `APPDATA`.
- The gate stops only the process trees it started (dev servers and apps) and fails if any of them is still alive at the end.
- Waits are event-driven: process output, mock-provider events, `fs.watch`, Playwright `waitFor*` on DOM conditions. No fixed sleeps decide a result.

### Getting past the sign-in wall without a login

`phase4-ui` stopped at a "session expired" alert because it seeded a fake JWT. The legitimate offline path is the one ZCode's own desktop e2e uses (`packages/desktop/test/e2e/helpers/session-pane-retention-fixture.ts`):

1. `setting.json`: `providerFamilyDomain: "zai"`, `providerFamilyDomainMigrated: true`, the fixture workspace in `recentProjects` and `lastWorkspaceSession`, `locale: "en-US"`, and `closeToTrayOnWindows: false` (§8).
2. `credentials.json` = `{}` (no account; nothing to expire).
3. `provider_config.json`: one Personal Provider (`anthropic-messages`, base URL = local mock, synthetic API key) and its model as the default model selection. It is written with ZCode's own `NodePersonalProviderConfigRepository` through `seedPersonalProviderConfig` / `seedPersonalDefaultModelSelection` from `packages/desktop/test/e2e/helpers/model-provider-restart-config.ts`, bundled at gate time with esbuild.
4. The occupation onboarding page (`data-testid="onboarding-page"`) still shows on first launch; it is dismissed with `Escape`, as ZCode's `dismissOccupationOnboarding` does.

No login request is made and no credential is stored.

## 3. Mock LLM provider

`phase7-parity/mockProvider.mjs` is a loopback HTTP server that speaks Anthropic Messages SSE (`message_start`, `content_block_*`, `message_delta`, `message_stop`), the same wire format as ZCode's `upstream-replay-server.ts`. It answers by a marker in the newest user text, so both apps get identical bytes:

| Marker | Response |
| --- | --- |
| `P7_HELLO_<id>` | text `PHASE7_REPLY_<id> streamed in five parts.` in 6 deltas |
| `P7_SLOW_<id>` | 400 deltas, one every 150 ms (stopped by the user) |
| `P7_BUSY_<id>` | 30 deltas, one every 100 ms, then `PHASE7_BUSY_DONE_<id>` |
| `P7_QUEUED_<id>` | text `PHASE7_QUEUED_DONE_<id>` |
| planned `P7_WRITE_<id>` | a `Write` tool_use; after the tool_result: `PHASE7_WRITE_DONE_<id> error=<is_error>` |
| title request | `{"title":"Phase7 <marker>"}` |

It records every request (marker, kind, completed / aborted, tool_result `is_error`) and emits `request`, `streaming`, `completed`, `aborted` events the gate waits on.

## 4. App drivers

Both apps run the same `packages/ui` renderer and are driven with `playwright-core` over CDP.

| | Harness | ZCode |
| --- | --- | --- |
| Binary | `Worse/target/debug/harness-app.exe` | Electron from `packages/desktop` (`electron .`, built `out/main`, `out/host`, `out/preload`) |
| Renderer | Vite dev server on `localhost:1430` (`apps/harness-app/scripts/run-vite.cjs`) | Vite dev server on `localhost:5174` (`ELECTRON_RENDERER_URL`) |
| CDP | `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=<free port>` | `--remote-debugging-port=<free port>`, `ZCODE_DISABLE_FIXED_REMOTE_DEBUGGING_PORT=1` |
| Graceful quit | `plugin:window\|close` → close barrier drains the Host | `window.close()` with `closeToTrayOnWindows: false` → `app.quit()` → `prepareAppQuit` |

The ZCode Vite server runs with `ZCODE_REACT_SCAN=0`: the dev-only React Scan overlay otherwise sits on top of the composer stop/send and permission buttons and intercepts clicks. Packaged ZCode has no such overlay.

If port 1430 or 5174 is already taken, the gate fails with `HARNESS_DEV_PORT` / `ZCODE_DEV_PORT` instead of attaching to someone else's server.

## 5. Scenarios and pass criteria

Each scenario runs on Harness and on ZCode, each app on its own fresh isolated data (same seed, same mock). A scenario passes for an app when every criterion holds. Parity holds when both apps pass, or when a difference is listed as a known gap in §8 with its root cause.

| # | Scenario | Driven by | Pass criteria |
| --- | --- | --- | --- |
| S1 | Send + streamed reply | type `P7_HELLO_a` in `v4-composer-input`, Enter | mock records one `hello` request, completed; `v4-timeline` shows `PHASE7_REPLY_a streamed in five parts.`; the session appears as a `task-item` |
| S2 | Stop mid-stream | send `P7_SLOW_b`; wait until the timeline shows `slow<tag>b-5 ` (rendered while streaming; the id-specific text avoids matching an earlier stream); click `v4-stop` | mock records the request `aborted` before its last event; `v4-stop` disappears; the composer accepts input again; the timeline stops growing |
| S3 | Queue while busy | send `P7_BUSY_c`; while it runs, send `P7_QUEUED_d` then `P7_QUEUED_e` | both appear as queue items while busy; mock main requests arrive in order `busy c` → `queued d` → `queued e`, each after the previous completed; the timeline shows `PHASE7_QUEUED_DONE_d` before `PHASE7_QUEUED_DONE_e` |
| S4 | Permission approve | plan a `Write` of `phase7-approved.txt`, send `P7_WRITE_f` | the permission listbox (`Permission required`) appears; click `allowOnce`; the tool_result has `is_error=false`; the file exists with the planned content; the timeline shows `PHASE7_WRITE_DONE_f error=false` |
| S5 | Permission deny | plan a `Write` of `phase7-denied.txt`, send `P7_WRITE_g` | click the reject option; the tool_result is an error; the file does not exist; the timeline shows `PHASE7_WRITE_DONE_g error=true` |
| S6 | File tree + open file | open the workspace file tree; expand `src`; open `hello.txt` | the tree lists `README.md` and `src`; the opened file view shows `PHASE7_HELLO_FILE_CONTENT` |
| S7 | Git status + commit | modify `README.md` and add `notes.txt` in the fixture repo; open the git pane; commit all with message `phase7 parity commit` from the commit dialog | the git pane lists both changes; after the commit `git log -1 --format=%s` is the message and `git status --porcelain` is empty |
| S8 | Terminal | open the terminal panel; wait for the shell prompt; run `mkdir phase7-term-<tag>`, then `echo <token>` | the directory appears in the workspace (watched); the xterm rows show the token as typed and as output |
| S9 | Multiple sessions | `task-new-button`, send `P7_HELLO_h` in the new session; click the first session's `task-item`, then the new one | each switch shows that session's own reply and not the other's; `data-session-id` of the session pane changes |
| S10 | Language + theme | Settings → General → language `zh-CN`, back to `en-US`; theme toggled dark ↔ light | visible labels follow the locale (Settings `Language`/`界面语言`, help menu `Help`/`帮助`; `<html lang>` is not set by the renderer and is not a signal); `setting.json` `locale`/`localePreference` change; the theme class on `<html>` changes and `localStorage["zcode-theme"]` holds it |
| S11 | Restart restores | quit gracefully, relaunch on the same data | exit code 0; after relaunch the sidebar lists the same sessions, the last session's timeline shows its reply, locale and theme are as left |

Persisted-data shape: after the scenarios the gate compares the two apps' data (top-level key sets of `setting.json` and `provider_config.json`, the table list and column sets of `tasks-index.sqlite` and `cli/db/db.sqlite`, and the row counts of sessions/messages produced by the same scenarios). Keys that hold timestamps or random ids are compared by presence only. A difference is a parity gap.

## 6. Rollback (Harness → ZCode → Harness)

One isolated home, used by both apps in turn:

```mermaid
sequenceDiagram
  participant G as Gate
  participant H as Harness
  participant Z as ZCode
  participant D as ~/.zcode (shared)
  G->>H: launch, create sessions R1 + R2, locale zh-CN
  H->>D: sessions, settings (ZCode formats)
  G->>H: graceful quit (Host drained)
  G->>D: snapshot A (settings, session rows, files)
  G->>Z: launch on the same home
  Z->>D: reads
  G->>Z: verify R1, R2 visible with replies, locale zh-CN; create R3, locale en-US
  G->>Z: graceful quit
  G->>D: snapshot B (must contain all of A + R3)
  G->>H: kill switch on: launch exits at once
  G->>D: snapshot equal to B (switch wrote nothing)
  G->>H: kill switch off: launch
  G->>H: verify R1, R2, R3 visible with replies, setting.json locale en-US; Harness data intact
  G->>H: graceful quit
```

Pass criteria:

- Every session created in one app is listed and opens with its reply in the other, both directions.
- Settings changed in one app are in effect in the other, as far as ZCode itself makes them shared: ZCode shows `zh-CN` that Harness set (no locale cache in its fresh profile). Back in Harness, `setting.json` still holds ZCode's `en-US` (Harness did not rewrite it); the UI language Harness shows follows its own `localStorage` locale cache (§8).
- Session rows in `cli/db/db.sqlite` and `tasks-index.sqlite` only grow; no row from an earlier snapshot is lost or changed in its identity fields.
- `setting.json` and `provider_config.json` stay valid JSON; the provider, model selection and recent projects survive both directions.
- Harness-only data (`dev.harness.app` device id, `harness-webview2` profile) is still present after ZCode ran.
- Each app quits with exit code 0 and leaves no child process.

## 7. Kill switch

Contract (implemented in `apps/harness-app/src-tauri/src/kill_switch.rs`, checked first in `run()`):

- Engaged when `HARNESS_KILL_SWITCH` is `1`, `true` or `on`, or when the marker file `~/.zcode/harness/kill-switch` exists (home resolved as for the Host: `ZCODE_DESKTOP_HOME_DIR`, `HOME`, `USERPROFILE`).
- When engaged, Harness exits before the Tauri builder runs: no window, no WebView2 profile, no Host, no log file, no write to `~/.zcode` or `dev.harness.app`. It launches ZCode if found and prints one `HARNESS_KILL_SWITCH engaged (...)` line on stderr with the outcome. Exit code is 0.
- ZCode location, in order: `HARNESS_ZCODE_EXE`; the first non-empty line of the marker file; the per-user default (`%LOCALAPPDATA%\Programs\ZCode\ZCode.exe` on Windows, `/Applications/ZCode.app` on macOS, none on Linux).
- Turning it off = deleting the marker (or unsetting the variable). Harness then starts with all its data as it was.
- The kill switch deletes nothing. Uninstalling Harness also leaves `~/.zcode` in place (`phase-6-packaging.md` §6.2).

How a user goes back to ZCode:

1. Quit Harness (window close; the close barrier drains the Host).
2. Optional, to stop Harness opening by habit: create `~/.zcode/harness/kill-switch`, optionally with the ZCode executable path as its first line.
3. Start ZCode. Sessions, settings, providers and projects are the same data.

Gate check: with the switch engaged (variable and marker, separately) the exe exits 0 within the gate timeout, prints the line, launches the configured stand-in executable, and the isolated home snapshot is byte-identical before and after.

## 8. Known non-shared items

- Theme (`zcode-theme`), pane layout and some UI preferences live in renderer `localStorage`, which is per browser profile (Electron `userData` vs WebView2 profile). They do not carry between the apps. This is how ZCode stores them, not data loss; the rollback criteria do not include them.
- UI language cache: the desktop renderer (`ZCodeIntlProvider` without `preferSettingServiceLocale`) prefers `localStorage["zcode-locale-preference"]` over `setting.json` `locale`. A profile that already chose a language keeps it when the other app changes `setting.json`. Harness mirrors ZCode desktop here (`apps/harness-app/src/host/bootstrap.tsx`), so a ZCode profile behaves the same way. Making `setting.json` win would change ZCode semantics; it is left as is pending a product decision.
- Close to tray: ZCode on Windows hides to the tray on window close by default (`closeToTrayOnWindows`); Harness has no tray and quits. The gate seeds `closeToTrayOnWindows: false` so both quit on close.

## 9. Gate output

- Last line `PHASE7_PARITY_OK` followed by a JSON summary, or `PHASE7_PARITY_FAIL <STEP>: <message>` and exit code 1.
- Evidence under `Worse/.work/phase7/parity/`: per-launch logs, screenshots on failure, `parity-results.json` (per scenario and app), data snapshots.
- Steps: `BUILD`, `ZCODE_BUILD`, `HARNESS_DEV_PORT`, `ZCODE_DEV_PORT`, `*_DEV_SERVER`, `*_CDP`, `*_PAGE`, `SCENARIO_<id>`, `DATA_SHAPE`, `ROLLBACK_*`, `KILL_SWITCH`, `CLEANUP`, `HOME_LEAK`.
