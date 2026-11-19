# Security panel — dependency vulnerabilities, file checks, sandbox

Status: current. Owner: plan item 6 (`plans_harness_worse.md`). Shared index for every security surface in
Harness; sibling specs own their own details:

- `docs/specs/harness/file-safety.md` — content-based file type checks (item 4, Magika).
- `docs/specs/harness/sandbox.md` — Linux command sandbox (item 15, nsjail).

This file is the source of truth for the **dependency audit** (osv-scanner) and for the shared rules every
security surface follows: network opt-in, secret hygiene, where results live and who may block an action.

## 1. Scope

| Surface | Trigger | Result surface | Can block |
| --- | --- | --- | --- |
| Dependency audit | lockfile change, user click, reviewer tool | Security settings page, diff badge, chat tool | commit (gate) |
| File content checks | file created / permission request | diff view chip, permission dialog | no |
| Command sandbox | agent shell command | permission dialog | command (policy) |

Rules that apply to all three:

1. Nothing leaves the machine unless the user enabled the feature that needs it. A feature that needs the
   network states it in its Settings row before the first request.
2. No secrets, no absolute home paths and no file contents in logs; only counts, ids and file names.
3. Security findings never mutate the project. Harness reports, it does not rewrite lockfiles.
4. One owner per state (see §2); the UI reads mirrored properties, it never recomputes a verdict.

## 2. Owners

| State | Owner | Mirror |
| --- | --- | --- |
| Audit settings (`security.*` keys) | `harness-settings` `SettingsStore` (single writer) | `SettingsUi` get/set by path |
| Scanner binary (`~/.harness/bin/tools/`) | `apps/harness-slint/src/security/binary.rs` | none (path only) |
| Audit report per workspace | `apps/harness-slint/src/security` worker (one thread) | `SecurityUi` in `ui/settings/globals.slint` |
| Commit gate verdict | `src/security/gate.rs` (pure function of report + settings) | `SecurityUi.commit-*` |
| File type verdict | `harness-fs` (item 4) | diff/permission rows |
| Sandbox policy | `harness-agent-hub` permission flow (item 15) | permission dialog |

The audit worker is modelled on the panels worker (`src/panels/mod.rs`): a named `std` thread started on the
first command, a `Cmd` channel in, `slint::invoke_from_event_loop` out, `teardown()` on shutdown. It owns the
child process, so no other module spawns `osv-scanner`.

## 3. Dependency audit

### 3.1 Binary acquisition (network opt-in)

- Pinned version `v2.7.1` of `google/osv-scanner` (release asset names and CLI flags below are that release).
- Asset name for the running platform:
  `osv-scanner_{windows|linux|darwin}_{amd64|arm64}` plus `.exe` on Windows.
- Stored as `<HARNESS_HOME>/bin/tools/osv-scanner-2.7.1[.exe]`; never committed, never added to `PATH`, and
  removed by the "Remove scanner" button on the Security page.
- Download happens only after an explicit user action (`Scan now`, `Download scanner`, or enabling the
  commit gate). Checking for the file locally is always allowed.
- Integrity: download `osv-scanner_SHA256SUMS` from the same release tag, find the line for the asset, then
  compare with the SHA-256 of the downloaded bytes. A mismatch deletes the temporary file and reports an
  error; nothing is installed. There is no "download anyway" path.
- The download is atomic: write `<name>.part`, verify, rename into place, then make it executable.
- Failure modes are user-visible and non-fatal: offline, HTTP error, checksum mismatch, unsupported platform.

### 3.2 Scan

- Command: `osv-scanner scan --format json --verbosity error <lockfile>` for each lockfile, one process per
  run with all lockfiles passed as arguments; lockfiles are passed absolute, never a shell string.
- Lockfiles considered (first existing per ecosystem): `Cargo.lock`, `package-lock.json`, `pnpm-lock.yaml`,
  `yarn.lock`, `go.mod`, `poetry.lock`, `Pipfile.lock`, `requirements.txt`, `Gemfile.lock`, `composer.lock`,
  `pubspec.lock`, `packages.lock.json`.
- Triggers: the user clicks `Scan now`; a lockfile of the workspace changes while `security.scanOnLockfileChange`
  is on (debounced ~3 s); the reviewer role calls the `dependency_audit` tool when that registry exists.
- Offline mode: `osv-scanner --offline-vulnerabilities` uses the locally cached OSV database and makes no
  request. Settings › Security › "Offline scanning only" selects it; the normal mode queries osv.dev.
- The run is cancelled by dropping the child process on `teardown`; the last report stays in memory only for
  the life of the app.

### 3.3 Report

`scan --format json` yields `results[].packages[].{package,vulnerabilities,groups}`. Harness keeps one row
per (package, advisory):

```
package, version, ecosystem, advisory id, severity, summary, fixed version, source lockfile
```

- `fixed version` = the first `affected[].ranges[].events[].fixed` of the matching affected entry, else empty.
- `severity` = `database_specific.severity` when present, else the CVSS score bucket, else `unknown`.
- Rows are sorted by severity, then package name; the page shows the total and the count per severity.
- An empty report is a success ("No known vulnerabilities"), not an error.

## 4. Commit gate

Settings › Security › "Commit gate": `off` (default), `warn`, `block`.

- Evaluated when a commit starts (`GitUi` commit path) and only when a lockfile is part of the changed set;
  the check may reuse the latest report, and it refreshes the report once when the lockfile changed since it.
- `off`: the commit proceeds unchanged.
- `warn`: the commit proceeds; the result row carries a warning with the number of advisories.
- `block`: the commit returns an `ApiError::Conflict` whose message names the advisory count and the first
  three packages; the working tree is untouched and the button returns to idle. The user can lower the gate to
  `warn` in Settings and commit again — Harness never silently overrides the gate.
- A failed scan (scanner missing, offline, timeout) does not block: it is reported as `skipped` with the reason.

## 5. Settings keys

| Key | Type | Default |
| --- | --- | --- |
| `security.commitGate` | `"off" \| "warn" \| "block"` | `"off"` |
| `security.scanOnLockfileChange` | bool | `false` |
| `security.offlineScan` | bool | `false` |
| `security.showDiffBadge` | bool | `true` |

## 6. UI

- Settings › Security page (`ui/settings/security.slint`, nav group APP, icon `shield-check`): scanner state
  (not downloaded / ready + version / download button), `Scan now`, gate selector, the three toggles, and the
  findings table (package, version, advisory id, severity, fixed version).
- Settings page strings are English.
- Diff view: files whose name is one of the §3.2 lockfiles get a vulnerability count badge in the file list
  when `security.showDiffBadge` is on and a report exists. A count of zero shows no badge.
- Chat: the `dependency_audit` tool answer lists the same rows (item 6.6 is deferred, see §8).

## 7. Acceptance scenarios

1. Fresh machine, gate `off`, no scanner: the Security page says the scanner is not installed and offers
   `Download scanner`; nothing is fetched until it is clicked.
2. Download succeeds: checksum verified, `Scan now` on a project with a known vulnerable dependency lists the
   advisory with its fixed version.
3. Checksum mismatch (simulated by editing the stored sums): the download is rejected, the previous binary (if
   any) stays, an English error is shown.
4. Lockfile change with `scanOnLockfileChange` on triggers a scan once per debounce window; with it off no
   process is spawned.
5. Gate `block` plus a lockfile that adds a vulnerable package: the commit is refused, the message names the
   count, `git log` is unchanged. Gate `warn`: the commit succeeds and the warning is shown.
6. Gate `block` but the scanner cannot run: the commit proceeds and the result says the check was skipped.
7. Offline mode with a cold cache reports the missing database as an error instead of scanning without data.
8. Removing the scanner deletes only `<HARNESS_HOME>/bin/tools/osv-scanner-*`.

### Verification (v2.7.1, 2026-11)

Cases 2, 3, 5, 6 and 8 are covered by unit tests in `apps/harness-slint/src/security/`
(`binary::tests::sha256_matches_known_vectors`, `checksum_file_lines_match_the_asset`,
`gate::tests::*`), plus one opt-in network test
(`binary::tests::download_and_verify_installs_the_pinned_release`, `cargo test -p harness-slint -- --ignored`)
which downloaded the real asset, matched its SHA-256 (`7121dcf4…c85800`) against the published
`osv-scanner_SHA256SUMS`, installed it and reused it on a second call.

Case 2 was also checked against the live service: `osv-scanner scan --format json --verbosity error -L Cargo.lock`
on this repository reported 11 advisories (3 `medium` with fixed versions, 8 `unknown` unmaintained), and multiple
`-L` lockfiles produced one merged report with exit code 1 while still emitting valid JSON — the error path accepts
that, because a non-zero exit with parsable JSON is not a failure.

Cases 4 and 7 need the GUI event loop and a running scanner; they were not executed in this environment.

## 8. Deferred

- `dependency_audit` for the reviewer role (plan item 6.6): the hub has no reviewer-role tool registry today
  (roles are UI-only, `apps/harness-slint/src/orchestra.rs`), so the tool is not exposed. Re-open this item when
  the role/tool registry lands; §3.3 already defines the row format it must return.
