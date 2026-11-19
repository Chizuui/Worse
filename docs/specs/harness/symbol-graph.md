# Symbol graph (kythe/kythe)

Plan item 9 of `plans_harness_worse.md`. Source project: [kythe/kythe](https://github.com/kythe/kythe)
(Apache-2.0). Status: **spike done, indexing blocked on Windows** — see [Spike result](#spike-result).

## Goal

Definition, references and callers for a symbol selected in the code view, and a way for a subagent to
claim a symbol instead of a whole file.

## Product rules

- The symbol graph is an **index**, not a second source of truth: source files stay authoritative, the
  index is disposable and rebuilt on demand.
- Indexing is opt-in (`Settings › Code › Symbol index`, off by default). While it is off, Harness does
  no extraction, spawns no indexer and reads no index directory.
- Indexing never blocks a chat turn, a tool call or the UI thread. It runs on a blocking worker and at
  most one run per workspace is active.
- A failed or partial index degrades the panel to "no index" — it never hides code, blocks edits or
  fails a session.
- Third-party indexer binaries and serving tables are downloaded or built **on demand per platform**;
  nothing is committed to the repository.

## Languages covered

Kythe ships extractors and indexers for these languages only:

| Language | Kythe tools (v0.0.76 release) | Covered |
| --- | --- | --- |
| C++ | `extractors/cxx_extractor`, `indexers/cxx_indexer` | yes, once a Windows builder exists |
| Go | `extractors/go_extractor`, `indexers/go_indexer` | yes, once a Windows builder exists |
| Java | `extractors/javac_extractor.jar`, `indexers/java_indexer.jar` | yes (JVM jars are platform-neutral) |
| JVM bytecode | `indexers/jvm_indexer.jar` | yes |
| Protobuf / textproto | `extractors/proto_extractor`, `indexers/proto_indexer`, textproto pair | yes |
| Rust | `extractors/rustproject_extractor` (consumes `rust-project.json`) | partial, extractor only |

Everything else (TypeScript/JavaScript, Python, Rust without a project file, Kotlin, Swift, shell)
comes from the **language server bridge (plan item 22)**, which does not exist yet. Until then those
languages show no symbol panel and cannot be claimed by symbol.

## Index location and layout

Owned by the hub (`crates/harness-agent-hub`), one directory per workspace, keyed by a hash of the
workspace identity key (`workspaceIdentity?.trim() || workspacePath`, hashed — never the raw path, so a
moved or renamed workspace cannot collide with another one and no user path is embedded in lookups):

```
<HARNESS_HOME>/index/<hash16>/
  meta.json          schema version, indexer name+version, workspace key hash, last refresh, file digests
  entries/           interim Kythe entry streams (kzip / entrystream output)
  serving/           Kythe serving tables written by the `write_tables` tool
  log/last.log       output of the last indexer run, truncated every run
```

- Single writer: the hub worker. Readers (`harness-fs`-style read-only queries from the UI) open the
  serving tables read-only and never write.
- Removing the workspace, turning the setting off, or clearing "Code index" in Settings deletes
  `<HARNESS_HOME>/index/<hash16>/`.
- `meta.json` records the indexer name and version, so a Harness or Kythe upgrade forces a rebuild
  instead of serving tables in an older schema.

## Refresh rules

| Trigger | Action |
| --- | --- |
| Setting turned on, or workspace opened with it on | full run if `meta.json` is missing or its indexer version differs, else no-op |
| Watcher reports changed files (debounced 5 s of quiet) | incremental: re-extract only changed translation units |
| At most one run per 60 s per workspace | extra events are coalesced into the next run |
| Turn or panel request arrives while a run is active | panel serves the previous tables; nothing waits |
| Indexer version or `meta.json` schema changes | delete `serving/` and `entries/`, full run |
| Setting turned off / workspace removed | delete the workspace index directory |

A run is cancelled (child process killed, `entries/` cleaned) when a newer run starts for the same
workspace or the setting is turned off.

## Symbol ids

Harness uses one textual id in protocol, claims and UI:

```
<language>:<workspace-relative file>#<qualified name>
go:internal/server/server.go#Server.Start
cxx:src/core/hub.cc#hub::Hub::start
java:src/main/java/dev/harness/Store.java#dev.harness.Store#put
```

- `<language>` is one of `cxx`, `go`, `java` (aliases `cpp` and `cc` normalize to `cxx`).
- The file part goes through the same normalization as path claims (`delegate_coord::normalize`):
  forward slashes, no absolute paths, no `..`, lower-cased on Windows.
- The qualified name is trimmed and must not be empty.
- Index-backed locations (line/column, Kythe `VName`) are extra fields in the query result; they never
  appear in the id, so an id stays valid after the file shifts.

## Symbol claims → file claims

A subagent may claim a symbol (`claim_paths` with `symbols`) instead of listing files. The claim is
mapped to the workspace-relative files the existing path checks already enforce, so there is exactly
one enforcement path.

```
subagent ──claim symbols ["go:internal/server/server.go#Server.Start"]──▶ delegate_server
                                                                          │ parse + normalize
                                                                          ▼
                                                 symbol_claims::claim_paths → ["internal/server/server.go"]
                                                                          │
                                                                          ▼
                                     delegate_coord::Coordination::claim(paths)  (unchanged checks)
                                                                          │
                                                                          ▼
                                     claim/release/conflict JSON, same shape as path claims
```

Rules:

- A symbol claim covers its defining file plus any related files the caller lists explicitly
  (declaration/definition split, generated file). It never widens to a directory.
- Symbol ids are normalized to their file set **before** reaching `Coordination`, so a symbol claim and
  an overlapping path claim conflict exactly like two path claims do.
- A symbol that cannot be mapped to a file (unparseable id, empty file part) is reported as a conflict
  with the reason, never silently granted.
- Removing the index does not invalidate active claims: they hold mapped paths, not index entries.
- `release` accepts the same symbol list and releases the mapped paths.

Data model: `crates/harness-agent-hub/src/symbol_claims.rs` (`SymbolLanguage`, `SymbolId`,
`SymbolClaim`, `claim_paths`). `Coordination` keeps its single claim list; symbols add no state.

## Spike result

Verdict: **no-go for native Windows indexing**. No step that needs a running indexer is implemented.

Evidence gathered on this Windows host (metadata only, no archive downloaded or installed):

| Check | Result |
| --- | --- |
| Latest release | `v0.0.76`, published 2026-07-16 |
| Release assets | exactly two: `kythe-v0.0.76.tar.gz` (337.4 MB) and `kythe-v0.0.76.tar.gz.sha256` — no `.zip`, no `.exe`, no Windows asset |
| Earlier releases | `v0.0.71` … `v0.0.75` each ship the same single Linux `tar.gz` (263–337 MB) |
| Binary format | first tar member `kythe-v0.0.76/indexers/go_indexer` (15.3 MB) starts with `ELF` → Linux x86-64 executables, not PE |
| Release packaging | `kythe/release/BUILD`: `package_release.sh` copies Bazel + Docker build outputs (ELF indexers, `.jar` indexers, `javac-wrapper.sh`) |
| Upstream request | kythe/kythe#182 "Windows support" is still **open**, labelled P4 / wishlist |
| Upstream docs | `kythe.io/getting-started` documents Linux (Debian) and macOS builds only; requires Bazel, clang, docker |
| This host | WSL2 `Ubuntu` present but stopped; no Docker CLI → Kythe can only run inside WSL/Linux, not as a native helper |

Consequence: a Harness-owned background indexer on Windows would have to ship or build Kythe itself
(Bazel + LLVM + Docker), which the plan forbids ("third-party binaries are downloaded on demand per
platform, never committed") and which cannot be verified without installing a toolchain. The Java
indexers are platform-neutral jars but still need the native extractors and the Go/C++ tooling around
them.

Re-entry condition: upstream publishes per-platform assets (or WSL is accepted as the index host) and
the spike can index a small project and answer a reference query end to end.

## Step status

| Step | State |
| --- | --- |
| 9.1 Spec | done — this file |
| 9.2 Feasibility spike on Windows | done — no-go, evidence above |
| 9.3 Background indexing per workspace | blocked (Kythe ships no Windows binaries, ELF-only releases) |
| 9.4 Symbol panel in the code view | blocked (needs the LSP bridge from plan item 22, which does not exist) |
| 9.5 Symbol claims in `delegate_coord` | data model done (`symbol_claims.rs` + tests); wiring into `claim_paths` waits for an index that produces ids |
| 9.6 Release build | open — nothing user-visible shipped yet |

## Acceptance (when 9.3 and 9.4 can start)

1. With the setting off, no `index/` directory is created and no process is spawned.
2. Indexing a small Go project in a workspace produces `meta.json` plus `serving/` within the debounce
   window, and a symbol query returns definition + at least one reference.
3. Editing one file re-extracts that file only (log shows one translation unit).
4. A symbol claim and a path claim on the same file conflict, and both are reported with the holder.
5. Turning the setting off removes the workspace index directory; an active run is cancelled.
