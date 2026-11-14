# Session import (Claude Code, Codex, ZCode)

Builds on phase-9-parity.md §9.2.1 ("Import, read-only", decision 14A). The hub API and the ZCode / Claude Code
readers already exist; this round adds Codex as a source and the UI that uses the API.

No code is taken from third-party exporters (for example `coreyx/claude-session-export`, which has no license); the
readers are Harness's own.

## 1. Product rules

1. **Read-only.** The importer never writes, renames or locks source files, and reads only the transcript subtrees:
   - ZCode: unchanged (§9.2.1).
   - Claude Code: `<claude>/projects/**/*.jsonl` (unchanged).
   - Codex: `<codex>/sessions/**/*.jsonl` and `<codex>/archived_sessions/*.jsonl`. Never `config.toml`, `auth.json`
     or any other file. `<codex>` = `CODEX_HOME` else `~/.codex`.
2. **Codex parsing** (one rollout file = one session):
   - `session_meta.payload`: `id` (else the file stem) is the external id, `cwd` the workspace path.
   - Sessions whose `originator` is `harness` are skipped (Harness already has them); so are sessions whose `source`
     is a subagent object.
   - Messages come from `event_msg` `item_completed` items when the file has any `UserMessage` / `AgentMessage`
     item: `UserMessage.content[].text` (joined by newlines) opens a turn, `AgentMessage.content[].text` is assistant
     text, `CommandExecution` (`command`), `McpToolCall` (`server`/`tool`) and `ImageView` become tool titles.
   - Otherwise (older files) from `response_item`: `message` with role `user` (`input_text`) / `assistant`
     (`output_text`), `function_call.name` as tool titles; role `developer` is skipped.
   - User text drops `<environment_context>`, `<user_instructions>`, `<turn_aborted>`, `<harness-memory …>` and
     `<system-reminder>` elements and messages that start with `# AGENTS.md instructions`. A user message left empty
     is not a turn.
   - Timestamps: the top-level `timestamp` (RFC 3339) of each record; title = first user text (80 chars).
3. **Agent of an imported session:** the agent of its source when it is configured (Claude Code → `claude-code`,
   Codex → `codex`; when that id is absent, the first agent of the fallback order whose kind is Claude Code /
   Codex), else the first agent of the fallback order (unchanged for ZCode).
4. **Continuing an imported session (native resume):** when the session got the agent of its source (rule 3), the
   import also stores a native handle `(agent, native_session_id = external_id, last_seq = imported turn count)`.
   The first prompt then goes through the existing setup path: `session/load` with that id and the original
   `cwd` when the adapter advertises `loadSession` (claude-agent-acp and codex-acp do), so the agent continues with
   its full original context (tool results included), and no seed or catch-up block is sent for the imported turns.
   If the load fails or is not supported, the hub falls back to `session/new` plus the seed of all earlier turns
   (spec §7), so the session can always be continued. ZCode imports and imports that fell back to another agent
   store no handle and always use the seed.
   - Import itself stays read-only. Continuing a resumed session writes to the original agent's own session store
     (for example a Claude Code transcript), exactly as that agent does when resumed from its own CLI.
5. **Sessions Harness itself ran are not candidates:** a Claude Code or Codex session whose id is the native session
   id of any Harness session (`agent_handles.native_session_id`) is left out of `import_scan`, unless it was
   imported (then it stays listed with the Imported badge). This covers Claude Code transcripts that
   claude-agent-acp writes for Harness sessions, which carry no Harness marker.
6. **Dedupe** stays `imported_from = <source>:<external_id>`; Codex uses the prefix `codex`.

## 2. Owner, API (hub API 0.17.0)

- `ImportSource::Codex` (`"codex"`); `ImportRoots.codex`. Scan, import and dedupe are unchanged otherwise.
- Owner of the page state: app module `import` (candidates, filter, query, selection, busy, result). The hub owns
  parsing and writing; the page holds no session data after an import (the sidebar refreshes on `SessionsChanged`).

## 3. UI: Settings › Import sessions (group "Data and statistics", after Archive management)

- The Settings nav scrolls when it is taller than the dialog, so the last entries stay reachable at a 720px window.

- Header: "Import sessions" + description (read-only copy; source files are never changed).
- Source filter (All / Claude Code / Codex / ZCode with counts), search over title and folder, **Refresh**.
- Rows: checkbox, title, `source · folder · N turns · updated`; already imported rows show an **Imported** badge and
  cannot be selected. Newest first.
- Footer: "N selected" + **Import selected**. The page sends one `import_sessions` per source in the selection,
  one after another, then shows "Imported N sessions" (+ skipped count and first reason) and rescans.
- Opening the page scans once; while scanning or importing the buttons are disabled and a "Scanning…" /
  "Importing…" line shows.

## 4. Verification

Quick build only. Fixture roots under the scratchpad (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `ZCODE_DATA_BASE_DIR`
pointing at fixtures; never the user's real folders): a Claude Code jsonl and two Codex rollouts (one new item format,
one old response format) plus one `originator: harness` rollout. The page lists 3 candidates (the Harness one is
absent); importing 2 adds them to the sidebar with their turns and tool titles; reopening shows them as Imported.
