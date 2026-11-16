# Phone client redesign (design frames 08, 09, 10, M1–M18)

The phone web client lives in `crates/harness-remote/src/assets/` and is embedded by `http.rs`. The Android app
(`apps/harness-mobile`) is a WebView on this client, so one implementation serves both.

## Design source

`C:\Users\simy\Downloads\dwd\mobile-spec\` holds, per frame, `<frame>.txt` (every layer on one line: type, name,
x/y relative to its parent, size, fill, stroke, radius, auto-layout, font and text) and `<frame>.png` (2x render).
Frames: `08-sessions`, `09-chat`, `10-pair`, `M01-settings` … `M18-import`. The frame is 390 px wide; the status bar
(`9:41`, signal, battery) and the home indicator are device chrome and are never built.

## Design tokens (already the look of the desktop app)

| Token | Value |
| --- | --- |
| background | `#0B0C0E` |
| surface (inputs, cards) | `#16181C` |
| raised (active row, chips) | `#1C1F24` |
| border | `#24272D`, hover `#2A2D33` |
| text | `#EDEEF0`, secondary `#C9CCD2`, muted `#8A909A`, subtle `#6B717C`, faint `#5D636D` |
| accent | `#7C8CFF`, gradient `#7C8CFF → #B07CFF` |
| success | `#3DD68C` (chip background at 12% alpha) |
| agent dots | Claude `#E88A5C`, DeepSeek `#4D6BFE`, others from the frame |
| font | Inter, falling back to the system UI font; sizes 10/11/12/13/15/22 |
| radius | 6 (chips), 10 (rows, inputs), 12 (cards), 999 (pills) |

Every colour goes through a CSS custom property declared once in `style.css` `:root` (owner: orchestrator). View
styles use `var(--…)` and never repeat a hex value that has a token.

## File ownership (one owner per file; nobody edits a file they do not own)

| Files | Owner |
| --- | --- |
| `app.js`, `style.css`, `sw.js`, `http.rs`, `manifest.webmanifest` | orchestrator |
| `sessions.js`, `sessions.css` | sessions agent |
| `index.html` (only `#view-pair` and `#view-session`), `pair.css`, `chat.css` | pair + chat agent |
| `settings.js`, `settings.css` | settings agent |
| `rpc.rs`, `lib.rs`, `tests.rs`, `apps/harness-slint/src/remote/` | rpc agent |

## Module contract

`app.js` calls, at boot:

```js
initSessions(document.querySelector('#view-list'), api);
initSettings(document.querySelector('#view-settings'), api);
```

and afterwards `renderSessions(model)` whenever the list changes, and `showSettings(page)` when the hash is
`#/settings` (`page === ''`) or `#/settings/<page>`.

`api`:

| Member | Meaning |
| --- | --- |
| `el(tag, className, text)` | creates an element (text is set with `textContent`) |
| `rpc(method, params)` | hub call, throws on failure |
| `rpcSoft(method, params, fallback)` | hub call, returns `fallback` on failure or when the method is not allowed |
| `getStatus()` | `GET /api/v1/status`, `null` when unreachable |
| `toast(text)` | short message |
| `agentName(id)`, `agents()` | display name; `Map` of agent id → agent |
| `fmtWhen(ms)`, `baseName(path)` | formatting helpers |
| `sessionHash(id)` | href that opens a session |
| `reload()` | reloads the session list |
| `createSession(params)` | starts a session; rejects when the hub does not allow it |
| `openSettings(page)`, `closeSettings()` | navigation |

`model` for `renderSessions`: `{ sessions, statusText, live, remote }` — `sessions` are `SessionSummary` objects
(`id`, `title`, `workspacePath`, `activeAgent`, `pinned`, `unread`, `updatedAt`, `turnCount`, and a running flag as
sent by the hub), `statusText` is a loading or error line (empty when fine), `live` is whether the websocket is
connected, `remote` is the last `/api/v1/status` body.

## Settings data contract (read-only)

Two hub methods, both read-only and both free of secrets (no API keys, tokens, pairing codes or file contents of
credential files):

- `settings_overview` → `{ "host": "simy-PC", "items": { "<page>": "<value shown at the right of the row>" } }`
- `settings_page` with `{ "page": "<page>" }` →
  `{ "title": "...", "subtitle": "...", "sections": [ { "title": "...", "note": "...", "rows": [ Row ] } ] }`

`Row`: `{ "title", "detail", "value", "tone", "badge", "kind", "on" }` — all optional except `title`.
`tone` is one of `default | accent | success | warning | danger | muted`; `kind` is one of
`row | toggle | code | stat`; `on` is the state of a toggle (shown, not editable).

Pages: `general`, `appearance`, `notifications`, `inbox`, `shortcuts`, `agents`, `providers`, `orchestra`, `mcp`,
`subagents`, `instructions`, `memory`, `skills`, `commands`, `hooks`, `remote`, `projects`, `archive`, `import`.

A page the hub cannot fill returns `sections: []`; the client then shows "Manage this on the desktop".

## Rules

- No frameworks, no build step, no new dependencies: plain ES modules and CSS.
- Text reaches the DOM through `textContent` only. Never `innerHTML` with data.
- Touch targets are at least 40 px high. Layout works from 320 px to 480 px wide and respects
  `env(safe-area-inset-*)`.
- `node --check <file>.js` passes for every JS file touched.
- `create_session` stays blocked in `rpc.rs` (the allowlist test names it as dangerous). The "+" button is shown
  only when `createSession` succeeds; it is hidden after the first rejection.
