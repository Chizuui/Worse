# Local memory (recall)

Status: implemented on `liquid-glass-ui`. Code: `crates/harness-agent-hub/src/recall/`, tools in
`crates/harness-agent-hub/src/mcp.rs`, session hooks in `crates/harness-agent-hub/src/session.rs`, the settings
page in `apps/harness-slint/src/settings/recall.rs`.

## 1. Goal

Context should survive a switch: a new session, another agent, a restart of the app. Before this, three
things carried context:

| What | Scope | Limits |
| --- | --- | --- |
| `MEMORY.md` (global, project) | hand-kept document, injected whole | grows without search; an agent has to rewrite the file to change it |
| session notes | one session | not visible to other sessions |
| seed / catch-up blocks | one session | earlier turns of the same session only |

Nothing told a new session what another session had done, and nothing could be searched. The local memory adds
that: many small entries, searched, with only the relevant ones put into a prompt.

## 2. What is stored

One file, `<home>/memory/recall.jsonl`, one JSON record per line.

- **Memories**: one fact each, at most 1,000 characters. Kinds: `fact`, `preference`, `decision`, `howto`.
  Scope: one project, or global. Written by an agent (`memory_add`) or by the user (Settings › Memory ›
  Recall). A memory can be **pinned**: it is then given to every new session of its scope. Preferences are
  pinned by default.
- **Episodes**: one digest per session, written by Harness when a turn completes and replaced when the session
  goes on. It is cut from the conversation, no model is involved: the first request, the last three later
  requests, the end of the last answer (the last 480 characters: how things stand), and the files that were
  changed. Subagent sessions leave none; their parent's digest covers the work. Episodes expire 180 days after
  the session's last turn.

Records are `put`, `forget` and `pin`. The live set is the fold of the file: a `put` retires the memory it
`replaces` and any earlier record with the same `key` (episodes: `session:<id>`); `forget` removes; `pin`
flips the flag; expired entries drop out. A line that does not parse is skipped, so a write cut off by a crash
loses that record only, and the next write starts on a new line. When at least 64 records are dead and they
outnumber the live ones, the file is rewritten with the live records (atomic rename). Writers hold the same
lock file scheme as `MEMORY.md`; readers do not lock.

Not stored: credentials. A text that carries a known token prefix (`sk-`, `ghp_`, `AKIA`, …), a
`password = …` style assignment with a credential-like value, a private key block or invisible characters is
refused by `memory_add` with the reason. Episodes are not refused but redacted, since nobody chose their words.
These are heuristics, not a guarantee.

## 3. Search

No embedding model, nothing to download, nothing sent anywhere.

- Words are lower-cased runs of letters and digits. `camelCase` identifiers also count by their parts
  (`TokenPairing` matches "pairing"). A trailing plural `s` is dropped. Common English and Indonesian filler
  words are ignored.
- BM25 (k1 1.2, b 0.75) over text and title ranks the matches.
- The lexical order and the newest-first order of the matches are fused by reciprocal rank (k 60), the lexical
  list counting double. Recency breaks ties; it does not outvote relevance.

Limits that follow: a memory is found by its words, not by its meaning ("expiry" does not find "expire");
languages written without spaces are matched as whole runs.

## 4. What a session is given

When an agent session starts fresh (a new Harness session, or an agent that has not served this session yet),
its first prompt gets, after `MEMORY.md`:

```
<harness-recall>
[Recalled from Harness's local memory: notes from earlier sessions, some written by other agents. …]
Always:
- [md0483ab1ca] preference: Reply in Indonesian, casual tone
Related to this request:
- [m6f03bff50b] decision · 3d ago · Claude Code: Pairing tokens expire after ten minutes because of replay attacks
Earlier sessions in this project:
- [m7433716e38] 2h ago · Codex · "Fix pairing expiry": Asked: … | Result: … | Files: src/auth.rs
</harness-recall>
```

followed by a short instruction on the tools.

- **Always**: pinned memories, newest first, up to 1,000 characters.
- **Related**: up to 6 search results for the user's request. A request of four or more search words needs
  two of them in a memory, so one shared word does not pull a memory in.
- **Earlier sessions**: the 3 latest episodes of the project from the last 60 days, other than this session's.
- The block is at most 3,200 characters, built once and not changed for that agent session.
- With nothing stored, only the instruction is sent.

## 5. Tools

On the `harness-memory` MCP server every agent already gets (`memory_read` / `memory_write` and the session
notes tools stay as they are):

| Tool | Does |
| --- | --- |
| `memory_search(query, kind?, limit?)` | search memories and episodes of this project and global |
| `memory_add(text, kind?, scope?, pinned?, replaces?, ttl_days?)` | save one memory; the same text is not stored twice; `replaces` retires an older one |
| `memory_forget(id)` | forget one memory |
| `memory_recent(limit?)` | latest sessions and memories, newest first |
| `memory_session(session)` | digest and working notes of an earlier session |

The server is started with `--agent <display name>`, so a memory says who wrote it.

## 6. Settings

Settings › Memory › **Recall**: two switches (recall in new sessions, remember each session), a field to add a
memory by hand, search, a filter (all / memories / sessions), pin and forget per entry. The switches are kept
in `<home>/memory/config.json` and read each time, so they apply to the next session without a restart.

## 7. Where the ideas come from

No code of these projects is used.

- **memvid** (`.mv2`): one portable file, append-only records with tombstones, a lexical index and a time
  index, usable without embeddings, reciprocal-rank fusion with k 60.
- **supermemory**: atomic memories instead of documents, a scope per project, an always-present profile next
  to what is recalled for a query, `updates` relations and `isLatest` (here `replaces`), `forgetAfter` (here
  `ttl_days` and the episode expiry).
- **Hermes Agent**: a bounded block fixed at session start, refusal of exact duplicates, a scan of writes for
  credentials and invisible characters, a fence saying recalled text is not user input, search of past
  sessions with no model in the loop.

Left out on purpose: embeddings and vector search, model-written extraction of facts from conversations,
decay scores, a memory graph.

## 8. Verification

- Unit tests (`recall::tests`, 17): add and reload, scopes, duplicates, replace, forget, pins, credentials
  refused and redacted, episode upsert, a torn last line, compaction, six concurrent writers, expiry, config,
  word splitting, ranking, the block's sections and budget, the episode text, project keys.
- `mcp::tests::recall_tools_carry_memories_across_sessions_and_agents`: one agent saves in one session; another
  agent in a later session finds it, reads the earlier session's notes, corrects and forgets; a session of
  another project sees only the global memory.
- In the app under Xvfb, with two scripted agents: agent A answers a request and saves a decision through the
  real `--memory-mcp` helper; the app is closed and started again with agent B; B's first prompt in a new
  session carries the decision and the digest of A's session. A third session carries the preference pinned in
  Settings. The Recall page: list, add, a refused password, search, forget.

Not verified: real agent CLIs deciding by themselves to call `memory_add` / `memory_search` (the scripted
agents were told to); Windows beyond a type-check of the target; stores larger than a few hundred entries
(every search reads the whole file, which is fine for thousands of entries and not measured beyond).
