# Skill marketplace

Browses the public skill catalog of `google/skills` inside Settings › Skills and installs a skill as a
normal `SKILL.md` directory for the selected agent CLIs. Modelled on the catalog published by
[google/skills](https://github.com/google/skills) (Apache-2.0); the repository stays the owner of the
skill content, Harness only reads and copies it.

Related: [skill-health.md](./skill-health.md) evaluates the *discovery* phase of the same catalog;
this spec owns the *install* phase. Both use the same catalog source and never write to it.

## 1. Product rules

1. **One catalog, many agents.** The marketplace lists skills from the remote catalog; installing one
   writes it to every agent that supports Skills today: Claude Code (`<base>/skills`), Codex
   (`<base>/skills`) and the Harness overlay (`<harness_dir>/skills`). Agy does not support Skills and
   is never offered.
2. **User scope only.** A marketplace install always targets user scope. Project scope stays in the
   existing Skills editor (`ConfigPage`, `+ New`), which is unchanged by this spec. Rationale: the
   catalog is personal tooling, a one-click install must not depend on the open workspace, and the
   existing scope switch keeps one meaning per page.
3. **The catalog is read-only and fetched by the app.** No Harness process modifies the upstream
   repository, and no Harness-owned skill index is published.
4. **Nothing is written without a diff.** An install, update or removal produces a normal
   `WritePlan` that the user confirms in the existing diff dialog. There is exactly one write path for
   agent configuration (`harness-agent-config`) and one owner of the pending plan (the settings
   worker).
5. **Installed = present in a target skill directory.** The marketplace does not keep a private
   "installed" registry that can drift from disk. It compares the catalog entry against the `SKILL.md`
   that is actually on disk for each supported agent.
6. **Everything works offline.** With no network the page still lists the installed skills (from the
   same listing the Skills editor uses), shows the cached catalog when one exists, and reports that
   the catalog is unavailable instead of failing the page.
7. **No credentials, no telemetry.** Requests carry a plain `harness/<version>` user agent and no
   token. Nothing about the user is sent upstream.

### 1.1 Catalog source

| Purpose | Request |
| --- | --- |
| Catalog index | `GET https://raw.githubusercontent.com/google/skills/main/index.json` |
| Skill file tree (extra files, revisions) | `GET https://api.github.com/repos/google/skills/git/trees/main?recursive=1` |
| `SKILL.md` body | the `entrypoint` URL given by the index (a `raw.githubusercontent.com` URL under `skills/<group>/<name>/SKILL.md`) |

`index.json` is `{ "generator": string, "skills": [ { "name", "description", "entrypoint" } ] }`.
The tree response is `{ "tree": [ { "path", "type": "blob"|"tree", "sha" } ], "truncated": bool }`;
blobs under `skills/<group>/<name>/` other than `SKILL.md` are the skill's extra files.

### 1.2 Cache

The app keeps a cache under `<data-dir>/skills-catalog/`:

| File | Content |
| --- | --- |
| `index.json` | last accepted catalog index body |
| `index.etag` | `ETag` of that body, sent back as `If-None-Match` |
| `tree.json` | last accepted tree body |
| `installed.json` | per skill name: `group`, `sha` set of the installed files, `installed_at` |

Rules:

- Revalidating with `If-None-Match` and getting `304 Not Modified` keeps the cached body; any other
  success replaces body and etag.
- The index is refreshed at most once per 24 hours on page open. A manual **Refresh** always
  revalidates.
- A cache miss with no network is not an error state: the page shows installed skills plus a status
  line.

### 1.3 Install, update, remove

- **Install** downloads `SKILL.md` plus the skill's extra blobs, converts them into a
  `Skill` body, and plans one `WritePlan` per agent. Each change is a normal text write
  with a truthful before/after diff.
- **Update** is offered when the recorded per-file sha set differs from the current tree. It reuses the
  install path, so it is still a diff the user confirms.
- **Remove** uses the existing skill delete plan (whole skill directory).
- Path safety is enforced before anything is planned: a catalog name must pass the same segment check
  as a manually created skill, an entrypoint must be `http(s)` and end in `.md`, and an extra file
  path must be relative, must not escape the skill directory and must not contain `..`, backslashes,
  a leading `/` or empty segments. A rejected entry is skipped (catalog) or refuses the install
  (extra file) with an error message — never silently written outside the skill directory.

### 1.4 Recorded metadata

`installed.json` records `group` and the sha set so an update can be detected without downloading the
bodies. It never records a project path, a token or any file content. Removing a skill deletes its
entry; a skill deleted outside Harness loses its entry on the next compare (the marketplace trusts
disk, not the file).

## 2. State ownership

```text
        catalog + tree + skill bodies            pending WritePlan               disk
  ┌──────────────────────────────┐      ┌──────────────────────────┐   ┌─────────────────────┐
  │ apps/harness-slint/src/skills │      │ settings worker          │   │ agent CLIs          │
  │ fetch, cache, parse, compare  │─────▶│ Req::Plan / Req::Confirm │──▶│ <base>/skills/<name>│
  └──────────────────────────────┘      └──────────────────────────┘   └─────────────────────┘
        │  catalog rows (view model)             ▲ diff dialog (UI)
        ▼                                        │
  SkillsUi (Slint global) ───────────────────────┘
```

- `crates/harness-agent-config/src/skill_catalog.rs` owns parsing and planning. It is network-free:
  it turns bytes into a catalog/tree, and a catalog entry plus its downloaded files into `WritePlan`s
  through the existing `md_store` planners. Unit-testable with `tempfile`.
- `apps/harness-slint/src/skills/` owns network access, the cache, and the update comparison. It is
  the only place that talks HTTP for skills.
- `apps/harness-slint/src/settings/worker.rs` stays the only owner of a pending plan and the only
  caller of `AgentConfigService::apply`.
- `SkillsUi` (Slint global) is a read-only projection; clicks are callbacks into the app module.

Event order for an install:

```text
UI install(name) → skills module: tree + SKILL.md + extra blobs (network)
                 → Req::Plan{ title: "install skill", op: InstallCatalog{..} }
                 → worker: per agent plan_upsert → with_extra_files → merged WritePlan
                 → DiffUi.open → user confirms → Req::Confirm
                 → worker: AgentConfigService::apply → applied(result)
                 → UI reloads the Skills listing → row shows "installed"
```

A failure before `Req::Plan` leaves no plan and no file change; a failure inside the worker is shown
as the existing plan error and no file is touched. Cancelling the diff discards the plan.

## 3. Interfaces

### 3.1 `harness-agent-config::skill_catalog`

| Item | Contract |
| --- | --- |
| `CatalogSkill { name, description, group, entrypoint }` | One catalog row; `group` derived from the entrypoint path. |
| `Catalog { skills: Vec<CatalogSkill> }` | Parsed index. |
| `parse_catalog(&[u8]) -> Result<Catalog>` | Tolerant: skips entries with an unusable name, a non-http(s) entrypoint or no `.md` suffix. |
| `TreeEntry { path, sha, is_blob }` | One tree row. |
| `parse_tree(&[u8]) -> Result<Vec<TreeEntry>>` | Parsed tree; an empty tree is valid. |
| `skill_files(&[TreeEntry], group, name) -> Vec<(String, String)>` | Relative path and sha of the extra files of one skill. |
| `parse_skill_md(name, &str) -> Skill` | Front-matter aware, including block scalars (`\|`, `>`) for `description`; extra keys kept, body kept. |
| `with_extra_files(WritePlan, &[(String, String)]) -> Result<WritePlan>` | Appends one text change per extra file under the plan's skill directory. |

### 3.2 App module `skills`

- `install(&AppWindow, req, data_dir)` registers the callbacks and remembers the request sender.
- Callbacks: `search(query)`, `refresh()`, `install(name)`, `update(name)`, `remove(name)`.
- Results are pushed into `SkillsUi` from the UI thread (`slint::invoke_from_event_loop`).
- Network work runs on a worker thread with a current-thread tokio runtime, like
  `plan_usage.rs`.

### 3.3 `SkillsUi` (Slint global, `ui/settings/globals.slint`)

`installed` / `catalog` rows (`name`, `description`, `group`, `installed`, `update`, `busy`),
`loading`, `catalog-available`, `query`, `status`, `status-error`, and the callbacks above. The page
component lives in `ui/settings/skills.slint` and is rendered by `ConfigPage` when
`ConfigUi.concept == "skill"`.

### 3.4 Remote and phone

`harness_remote::SettingsView::page("skill")` returns the installed skills (name, agent, scope) as
read-only rows with no actions. Browsing or installing from a phone is out of scope: remote pages are
read-only by contract, and a marketplace write needs the local diff dialog.

## 4. Acceptance scenarios

| # | Scenario | Expected |
| --- | --- | --- |
| A1 | Open Settings › Skills with network | Installed skills listed from disk; catalog rows appear with name, group and description; installed ones carry a badge. |
| A2 | Search the catalog | Rows filter by name, description and group, case-insensitively; an empty result shows an empty state, not an error. |
| A3 | Install a skill | Diff dialog lists one `SKILL.md` per supported agent plus its extra files; confirming writes only those paths; the row shows installed; the Skills editor lists it after refresh. |
| A4 | Cancel the diff | No file changes; the row returns to "not installed". |
| A5 | Install twice | Second plan is a no-op for that agent and is reported as such; no duplicate directory. |
| A6 | Update | When the tree sha differs, the row offers Update; the diff shows the changed body; confirming records the new sha set. |
| A7 | Remove | The confirm dialog shows the whole skill directory as removed; the directory is gone and the row returns to "not installed". |
| A8 | Offline open | Page renders with installed skills and a status line that the catalog is unavailable; no dialog, no error dialog. |
| A9 | Cached offline open | The cached catalog is shown, marked stale in the status line. |
| A10 | Malicious catalog entry | An entry with a name like `../evil`, an `http://`-only or non-`.md` entrypoint, or an extra file path with `..` is skipped or refused; nothing is written outside the skill directory. |
| A11 | Unsupported agent | Agy never appears in the target list; selecting it is impossible from the marketplace. |
| A12 | Catalog size 0 / malformed JSON | Page shows an error status and the installed list; the cached body stays untouched. |
| A13 | Auth CLI revalidation returns 304 | Cached body reused; no re-download of the index. |

Manual release check (A1–A7, OS recorded) is the Phase 9 exit evidence for this item.

## 5. Exclusions

- No publishing, editing or forking of upstream skills from Harness.
- No automatic background updates; update detection is on page open or manual refresh.
- No third-party catalog registry, no private catalog URL setting, no authentication.
- No project-scope marketplace install, and no `SKILL.md` execution or sandboxing (that is the agent
  CLI's job).
