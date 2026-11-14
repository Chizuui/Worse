# Phase 7 (agent A): performance baseline and comparison

Status: active measurement contract. Owner: performance gate `Worse/scripts/e2e/phase7-perf.mjs`
and its helpers in `Worse/scripts/e2e/phase7-perf/`. Functional parity (Phase 7 agent B,
`phase7-parity*`) is out of scope.

Targets come from `docs/plans/harness-rust-tauri-v2-plan.md` section 4.1. This spec fixes how each
one is measured on this Windows machine, against a ZCode baseline measured by the same probe, with
the same fixtures and the same isolation, in the same gate run.

## 1. What is compared

| Side | Build | How it runs |
| --- | --- | --- |
| ZCode | `packages/desktop/out/{main,preload,host,scheduler,plugin-sandbox}` (current checkout) + the ZCode renderer built with `vite build` into `Worse/.work/cache/phase7/zcode-renderer` + Electron 44.7.0 (the pinned version) | "packaged-like" layout in `Worse/.work/cache/phase7/zcode-app/`: `ZCode.exe` is a hard link of `electron.exe` (so `app.isPackaged` is true and the repo `.env*` files are never loaded), `resources/app` carries the app, `resources/{glm,tools,config,icons}` mirror the `extraResources` of `electron-builder.config.js`. Nothing under `packages/**` is written. |
| Harness | `Worse/target/release/bundle/nsis/Harness_<v>_x64-setup.exe` (the release NSIS installer) | the installer payload (exactly the `$INSTDIR` files, minus `uninstall.exe`) is extracted with 7-Zip into `Worse/.work/cache/phase7/harness-install-<sha8>/`, keyed by the installer's SHA-256. The installer is **not executed**: Tauri's NSIS template (tauri-bundler 2.9.4, `CheckIfAppIsRunning`) force-kills every `harness-app.exe` of the user by name in silent mode, which would kill Harness instances started by other gates or the user. Extraction also writes no HKCU entries. Runs the extracted `harness-app.exe` with its bundled Node. |

There is no installed ZCode release on this machine and the ZCode NSIS installer cannot be built
offline (electron-builder downloads NSIS). ZCode installer and installed sizes are therefore
**estimates** (section 3.6), labelled as such in every output.

Known structural differences that are part of what is measured, not noise:

- ZCode Host runs in an Electron `utilityProcess` and the Agent under `ELECTRON_RUN_AS_NODE`;
  Harness runs the same Host code under the bundled Node 24 and routes `search,git,pty` to
  `harness-host.exe` (Rust). `fs` and `watch` stay on the TypeScript services by default.
- ZCode bundles the Agent as `resources/glm/zcode.cjs`; Harness ships
  `apps/zcode-cli/packages/cli/dist`. Both come from the same source tree.
- Harness has a single window (no multi-window API). The 3-window RAM target is not measurable
  for Harness and is reported as not applicable on this platform build.

## 2. Isolation and safety (hard requirements)

- The gate calls `installIsolatedHome("phase7-perf")` before anything else; every child inherits
  the isolated `HOME/USERPROFILE/APPDATA/LOCALAPPDATA`. The `HOME_LEAK` guard must stay green.
- Electron resolves `appData` through known folders, so ZCode gets `ZCODE_DESKTOP_USER_DATA_DIR`,
  `ZCODE_DESKTOP_SESSION_DATA_DIR`, `ZCODE_DESKTOP_HOME_DIR`, `ZCODE_DESKTOP_APPLICATION_NAME`
  (`ZCode Phase7 Perf`) and `--user-data-dir`, all inside the isolated home. WebView2 gets
  `WEBVIEW2_USER_DATA_FOLDER` inside the isolated home. (Tauri's own log/app-data dirs resolve to
  the real `%LOCALAPPDATA%\dev.harness.app`; accepted Phase 6 behaviour, reported as a note.)
- No network beyond loopback: `ZCODE_ENDPOINT_ORIGIN`/`ZCODE_BASE_URL` point at a local 404 server,
  the LLM is a local mock (Personal Provider seeded with ZCode's own e2e seed helpers), the browser
  engines get `--proxy-server=127.0.0.1:9 --proxy-bypass-list=<-loopback>`, Node children get
  `HTTP(S)_PROXY=http://127.0.0.1:9`, `NO_PROXY=127.0.0.1,localhost`, `NODE_USE_ENV_PROXY=1`.
  The checked-out ZCode main bundle compiles RUM, telemetry endpoints and auto-update off
  (`ZCODE_ENV=test`, flavour `preview`).
- Only processes the gate spawned are measured or stopped (process trees rooted at our PIDs,
  identified by PID + creation time). Children are spawned with `windowsHide`; app windows are
  visible (they are the subject). Shutdown is graceful (`WM_CLOSE` via `CloseMainWindow`, then the
  tree is checked); `taskkill /T /F` is used only on our own tree after a timeout.
- Nothing is installed; the Harness payload is extracted (section 1), so no build lock or HKCU
  install entry is involved.
- The registry is not covered by the isolated home. ZCode main rewrites, on every start and
  unconditionally, `HKCU\Software\Classes\zcode` (`setAsDefaultProtocolClient`) and
  `HKCU\Software\Classes\{Directory,Drive}\shell\ZCode.OpenInZCode` (Explorer menu). Before the
  first ZCode launch the gate exports these keys (`Worse/.work/cache/phase7/reg-guard/`); after
  every ZCode run and at exit, a key that points into the gate's layout is deleted and the export
  re-imported. Keys another ZCode rewrote in the meantime are left alone. The gate fails with
  `REGISTRY` if a guarded key still points into the layout at the end. Harness writes no HKCU keys.

## 3. Metrics and method

All latencies: one warm-up, then N samples (N >= 5, default per metric below); report median, p50,
p95 (nearest-rank) and every raw sample. Clocks: epoch milliseconds from `performance.timeOrigin +
performance.now()` in pages and Node, one machine.

### 3.1 Startup (target: first window -30 %, first interactive workspace -25 %)

Per launch: spawn the app (`t0`), attach over CDP, read from the main page:

- **first window** = first paint of the main window (`first-paint`/`first-contentful-paint` entry,
  absolute) minus `t0`. A native "window shown" event is not observable the same way on both
  stacks; first paint is the first moment the user sees the window's content.
- **first interactive workspace** = the moment the composer input
  (`[data-testid="v4-composer-input"]`) of the restored workspace is attached and visible, stamped
  in-page by an init script `MutationObserver`, minus `t0`. If the page loaded before the init
  script could be installed, the sample is taken from an observer installed late and flagged
  `late` (upper bound); late samples are excluded from the median when at least 5 others exist.
- Also recorded: navigation start, `DOMContentLoaded`, and Host milestones parsed from each run's
  log (both apps run the same `packages/desktop/out/host`, same log lines): `initializing local
  services` (end of the shared database startup), `base-attachment-ready` (services exposed), and
  for ZCode the main-process `forked host process` line.

Both apps start from the same seeded isolated home: one workspace (small git repo) restored as the
last session, a Personal Provider pointing at the mock. Runs are interleaved (Z, H, Z, H, ...).
The very first launch of each app on a fresh profile is reported separately (`firstRun`); the
startup statistic uses the following N = 5 launches ("warm OS file cache, cold process" — clearing
the OS file cache needs admin rights and is not done).

### 3.2 Idle RAM, process count, idle CPU (targets: RAM -15..20 % on Windows, process count -25 %, idle CPU no regression and < 1 %)

Each app alone: launch, wait for the interactive workspace, detach CDP, stay idle for `IDLE_S`
seconds (default 300). Then sample the process tree rooted at the spawned PID:

- per process: name, PID, private working set (`WorkingSetPrivate` from
  `Win32_PerfFormattedData_PerfProc_Process`), working set, private bytes;
- totals: sum of private working set (headline number), sum of working set, process count;
- idle CPU: sum of `TotalProcessorTime` deltas of the tree over the last `CPU_WINDOW_S` seconds
  (default 60) of the idle period, as % of one logical CPU and % of the whole machine.

3 windows: measured for ZCode only when the app exposes a new-window path without UI changes;
Harness has no multi-window support, so the 3-window target is reported "not measurable".

### 3.3 Installer and installed size (target: NSIS -15..20 %)

- Harness: size of the setup `.exe`; installed size = sum of file sizes in the install dir.
- ZCode (estimate): installed size = staged tree an `electron-builder --dir` build would produce
  without `app.asar` `node_modules` (Electron runtime without `default_app.asar` + `out/**` without
  source maps + `package.json` + `resources/{glm,tools,config,icons}`), i.e. a **lower bound**.
  Installer size = that tree compressed with 7-Zip LZMA2 `-mx=9` (the NSIS payload format of
  electron-builder at its strongest setting), also a **lower bound**. A Harness win against a lower
  bound is a real win; a Harness loss is inconclusive and reported as such.

### 3.4 Command latency (targets: command-to-ACK p95 -20 %, command-to-first-delta p95 -15 %, delta-to-paint p95 -20 %)

In the restored workspace, through the real UI, against the local mock provider:

- type a prompt containing `P7PERFQ<id>` into the composer and press Enter. `t_send` = the
  capture-phase `keydown` (Enter) timestamp in the page;
- **ACK** = first entry pushed to `window.__zcodeV4CommandAcksE2E` after `t_send` (the v4 command
  ack the renderer records for every command, production builds included), stamped at push time;
- the mock streams `P7PERFA<id>` as the first text delta immediately (together with
  `message_start`), records the write time (`t_mock_first`), then deltas 30 ms apart and
  `P7PERFZ<id>` at the end (tokens without `_`, so Markdown cannot split them);
- **first delta in DOM** = the `MutationObserver` callback that sees `P7PERFA<id>` inside the
  page; **paint** = the frame after that callback (`requestAnimationFrame` + `MessageChannel`).
- command-to-ACK = ACK - `t_send`; command-to-first-delta = paint - `t_send`;
  delta-to-paint = paint - `t_mock_first`.

One warm-up prompt (creates the session), then `PROMPTS` (default 20, so nearest-rank p95 is the
19th sample rather than the maximum) prompts in the same session, each after the previous turn
finished.

### 3.5 Files, search, git, watcher (targets: 100 MB open -50 %, search p95 -40 %, git status -35 %, watcher p95 -30 %)

Service layer, through a real Host and the RPC client (the path the renderer uses), with the
shared Phase 3 harness (`phase3/hostSession.mjs`, Node sidecar launcher):

- ZCode side: `HARNESS_RUST_SERVICES=none` (ZCode's TypeScript services, unchanged);
- Harness side: the installed defaults (`search,git,pty` routed to the installed
  `harness-host.exe`); also reported as information: `fs,watch` opted in.

Both sides run the Host under the same Node, so the delta isolates the service implementation.
The ZCode Host normally runs inside Electron's utility process; that difference is stated in the
report, not hidden.

Fixtures (generated once, cached in `Worse/.work/cache/phase7/fixtures`, deterministic content):

- `big-100mb.txt`: 100 MiB text file. **file open** (headline, target checked here) = one
  `file.readTextFile { offset: 0, length: 256 KiB }`, exactly what `PreviewPane.tsx` does when a file
  is opened (`FILE_VIEWER_MAX_TEXT_BYTES`; a 100 MiB file comes back `truncated` and the viewer
  shows "file too large" — no UI path reads the whole file). Info rows: `stat` + `readTextFile`,
  and a **full read** = `file.readFileRange` over the whole file in 1 MiB windows (both services cap
  one window at 1 MiB, `MAX_BINARY_READ_BYTES`), advancing by bytes returned and asserting the total
  equals the file size. Single 64 KiB vs 1 MiB `readFileRange` timings are recorded to separate
  per-call overhead from per-byte transport cost. Measured for ZCode, Harness default, and Harness
  with `HARNESS_RUST_SERVICES` fs opt-in.
- `tree/`: generated source tree (`TREE_FILES`, default 60 000 files). **search** =
  `file.searchWorkspaceFiles` with `refresh:false` for 10 queries (after one indexing call),
  p50/p95 per query set.
- `repo/`: git repo (`REPO_FILES`, default 20 000 committed files, 500 modified, 200 untracked).
  **git status** = `git.refresh` + `git.getChanges { sourceId: "unstaged" }`, timed as the sum of
  both calls (what the changes panel does on open); `getChanges` alone is also reported. Raw
  `git` timings of the same repo are the floor: both stacks spawn `git status` and `git diff
  --numstat` the same way.
- **watcher** = `watch` on `tree/`, write a new file, time from write to the `onDynamicChange`
  event at the RPC client (event-to-Host-client latency); 10 samples.

Scale is chosen so the whole gate stays near 15 minutes; it is recorded in the output.

### 3.6 Not measured / caveats

- Absolute cold start after reboot (needs admin cache flush) - not measured.
- macOS/Linux rows of 4.1 - not applicable here.
- The machine runs the user's other apps; every result records the logical CPU count and the
  background process count, and medians are used.

## 4. Outputs

- `Worse/.work/phase7/baseline.json`: raw ZCode numbers of a complete run. Never overwritten; when
  it exists the run writes `baseline-<timestamp>.json`. Partial or debug runs (`SKIP`, `QUICK`,
  `STARTUP_N < 5`) always write `baseline-partial-<timestamp>.json`.
- `Worse/.work/phase7/perfA-analysis.md` (hand-written, optional): investigation notes; appended to
  `perfA.md` when present.
- `Worse/.work/phase7/harness-perf.json`: raw Harness numbers (overwritten each run).
- `Worse/.work/phase7/perfA.md`: table `metric | ZCode | Harness | delta % | target | pass/fail`
  plus the layer that explains each delta.

## 5. Gate contract

```text
node Worse/scripts/e2e/phase7-perf.mjs
  PHASE7_PERF_QUICK=1        short idle (60 s), 6 prompts (debug; result is a partial run)
  PHASE7_PERF_SKIP=<list>    skip parts: startup,idle,latency,service,size
  PHASE7_PERF_IDLE_S=<s>     idle period (default 300)
  PHASE7_PERF_STARTUP_N=<n>  startup launches per app (default 5; < 5 is a partial run)
  PHASE7_PERF_INSTALLER=<p>  Harness NSIS installer (default target/release/bundle/nsis/Harness_0.6.0_x64-setup.exe)
  PHASE7_PERF_7Z=<p>         7-Zip executable (default: the WindowsApps 7z.exe alias)
  PHASE7_PERF_STRICT=1       also fail when a 4.1 target is missed
```

Last stdout line: `PHASE7_PERF_OK {...}` when every requested measurement was collected, the
isolation guard is green, the registry guard is clean and no process the gate started is left;
otherwise
`PHASE7_PERF_FAIL <step> <reason>`. Target misses are reported in `perfA.md` and in the OK line;
they fail the gate only with `PHASE7_PERF_STRICT=1`.
