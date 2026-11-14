# Phase 10: Midnight Glass UI

Status: active implementation contract for `apps/harness-slint` (Slint 1.18.1).

This phase replaces the ZCode "Zai" look (neutral greys, black and white brand, Lucide icons). Harness gets its own visual world called Midnight Glass. Product behavior, data flow, state owners and protocols do not change. Only the Slint UI layer, the icon assets, their generator and the renderer choice change.

History: the first round of this phase, "Aurora", used colored radial lights, a teal-to-violet gradient on buttons, glows and icon tiles. The user rejected it as generic AI design. It also matched the slop catalogs in `no-ai-design-slop` (ARTICLE §5–6) and impeccable `craft-floor`. §3.5 lists the bans that came out of that review.

## 1. Goals

1. **Own identity.** Harness no longer copies the Claude Code or ZCode chrome.
   - A deep midnight canvas lit faintly from above.
   - Dark glass panels sit on it: translucent fill, hairline border, 1px top highlight and a real drop shadow.
   - Buttons are made of the same glass. The one primary action per surface is solid ice white.
2. **Smooth.** Every state change that the user causes with a pointer is animated, and the grammar is short.
   - Hover, press, select, open, close, expand and collapse all animate.
   - Nothing moves on its own when the app is idle.
3. **Simpler to use.**
   - Fewer visible controls at rest and larger hit targets.
   - One primary action per surface.
   - The composer keeps its tools in one row inside the card.
4. **Better symbols.**
   - Icon fonts and system glyphs are replaced by drawn icon sets with fixed roles.
   - No Unicode glyph or emoji stands in for an icon.

Mode: Operate (impeccable). Readability and scan speed outrank decoration inside the reading column.

## 2. Non-goals

- No real backdrop blur. Slint has none, and the user chose fake glass.
- No new `unsafe` code.
- No change to Rust state, core protocol, settings schema or keyboard shortcuts. The only Rust change is the renderer choice (§2.1).
- No continuous ambient animation, such as moving gradients or looping shimmer on idle surfaces.

### 2.1 Renderer

Slint 1.18.1's software renderer cannot draw this design:

- Drop shadows are not drawn (`draw_box_shadow` is a TODO).
- Clips ignore border radius.
- `transform-scale` and rotation are not supported.
- Linear gradients ignore border radius.

`select_renderer()` in `src/main.rs` therefore works as follows:

| Condition | Renderer used |
|---|---|
| `SLINT_BACKEND` is set | Slint picks the renderer itself. |
| `HARNESS_RENDERER=software` | Software renderer first. |
| `HARNESS_RENDERER=femtovg` | `winit` + `femtovg` (OpenGL) first. |
| Otherwise | `winit` + `skia`. |
| The chosen renderer fails | Falls back to `femtovg`, then `software`. |

Skia is the default since the 2026-11-04 text-quality fix: femtovg draws text with grayscale antialiasing and no hinting, so at 100% scaling (for example 2560×1440) every glyph stem is smeared over 2–3 pixels and text reads as blurry. Skia hints glyphs and draws crisp stems. Trade-off, measured on the quick build with the Skills page open: working set 218 MB (Skia) vs 153 MB (femtovg), executable +11 MB, and the build downloads Skia's prebuilt binaries. femtovg measured about 173 MB private memory at idle with the fake core; the software renderer used about 7 MB in the phase-8 measurement.

## 3. Visual world

### 3.1 Canvas

The window background is drawn once by `MidnightBackdrop` (`ui/theme/backdrop.slint`). It has two layers:

- **Base:** a vertical gradient. Dark goes from `#0A0E18` at the top to `#05070C` at the bottom. Light goes from `#F4F6FA` to `#E9EDF3`.
- **Top light:** one wide, low-saturation radial field centered above the window (x 0.5, y −0.25). It is the only light source.
  - Dark: `#7C8DB5` at 14%.
  - Light: `#FFFFFF` at 80%.

The canvas has no colored blobs, no second light and no noise.

### 3.2 Glass recipe

Layer order, from back to front:

1. canvas
2. panels: sidebar, conversation and side pane
3. raised: composer, cards
4. overlays: menus, popovers, dialogs

| Layer | Dark | Light |
| --- | --- | --- |
| Panel fill (`glass`, sidebar) | `#0C1019D1` | `#F7F8FBD9` |
| Reading fill (`glass-deep`, conversation and side pane) | `#090C14EB` | `#FBFCFDF0` |
| Raised and overlay fill (`glass-strong`) | `#121722F5` | `#FFFFFFF7` |
| Border (`glass-border`) | `#FFFFFF12` | `#0B10201A` |
| Top highlight (`glass-highlight`) | 1px, `#FFFFFF17`, faded at both ends | 1px, `#FFFFFF` |
| Shadow (`glass-shadow`) | blur 24, y 10, `#00000099` | blur 24, y 10, `#0B102014` |

Nested radius rule: inner radius = outer radius − padding, with a minimum of `radius-sm`.

### 3.3 Buttons

| Variant | Fill | Border | Extras |
| --- | --- | --- | --- |
| primary (one per surface: New session, Send, dialog confirm) | solid `primary`: ice `#E8ECF5` in dark, ink `#0B1020` in light | none | Text `primary-foreground`. Shadow blur 3, y 1, `primary-shadow`. Hover goes to `primary-hover`. |
| secondary and outline (glass) | `surface` | `glass-border` | 1px top highlight. Hover raises the fill to `surface-hover` and the border to `border-hover`. |
| ghost | transparent | none | Hover fill `hover`. |
| destructive and warning | semantic fill | none | Used only for the action that causes the harm. |

Rules for every variant:

- Press scales to 0.97.
- Disabled is opacity 0.45.
- Round buttons (send, stop, attach) follow the same table.

### 3.4 Color tokens

Token names stay as they were, so every call site keeps compiling. Only the values change.

| Role | Dark | Light |
| --- | --- | --- |
| foreground | `#E6E9F0` | `#0B1020` |
| foreground-subtle | `#E6E9F0B8` | `#0B1020B3` |
| foreground-subtlest (placeholders) | `#E6E9F08A` | `#0B102085` |
| accent (`accent-fg`) | `#8CB4FF` | `#2F5BD3` |
| primary | `#E8ECF5` | `#0B1020` |
| primary-foreground | `#0A0D15` | `#FFFFFF` |
| selected | `#FFFFFF14` | `#0B102012` |
| hover | `#FFFFFF0D` | `#0B10200A` |
| success | `#4ADE80` | `#16A34A` |
| warning | `#FBBF24` | `#D97706` |
| destructive | `#F87171` | `#DC2626` |

The accent has exactly these roles:

- focus border
- selected state
- link
- unread marker
- the running spinner

Status uses the semantic colors. Nothing else is colored. `violet`, `ember` and `sky` remain only as aliases:

- `sky` = accent.
- `violet` = a desaturated slate.
- `ember` = a desaturated sand.

They must not be used to make decoration colorful.

Semantic surfaces use the semantic color at 16% alpha in dark and 12% in light, with a foreground lifted for contrast. Body and placeholder text contrast on `glass-deep` is at least 4.5:1.

### 3.5 Bans

These are taken from the slop review. They are checked in A8.

| Banned | Replaced by |
| --- | --- |
| Gradient fills on buttons, avatars, marks or bubbles | Solid tokens. A gradient is allowed only for the glass top highlight and the canvas top light. |
| Glows: zero-offset colored shadows. `Theme.glow` is transparent. | Shadows with an offset and a dark color. |
| Colored radial lights without a source | — |
| Icon tiles: a colored rounded square behind an icon | Line icons in the text color, for tool rows, panel headers, the empty state and settings rows. |
| A grid of same-size icon + heading + text cards | — |
| The sparkle glyph as a stand-in for "AI" | — |
| Several accent hues on one surface | The single accent. |

## 4. Icons

Generated by `Worse/scripts/ui/gen-slint-icons.mjs <node_modules with @iconify-json/{hugeicons,ph,solar}>`. All output lives in `ui/icons/`. `SOURCES.txt` maps every file to its upstream name, and `LICENSES.md` carries the attributions.

| Global | Set | License | Role |
| --- | --- | --- | --- |
| `Icons` (`line/`) | Hugeicons stroke-rounded, 1.5 stroke | MIT | Every inline UI icon: navigation, toolbar, menus, buttons, tool rows. |
| `IconsFill` (`fill/`) | Phosphor fill and bold | MIT | Solid glyphs for state only: done, failed, warning, stop, send, pin. |
| `Tiles` (`tile/`) | Solar Bold Duotone | CC BY 4.0 (480 Design) | Generated and kept. Not used on screen in this round (§3.5). |
| `FileIcons` | Material Icon Theme | MIT | File tree. Index order is a contract with `src/files.rs`. |

Rules:

- Every icon is black in the SVG and is tinted with `colorize`.
- The fill set is never paired with the line set on the same control as an active/inactive swap.
- No Unicode glyphs as icons. `•`, `●`, `→`, `↑`, `↓`, `−` and `×` used as icons are replaced by drawn shapes or `Icons.*`.
- Typographic `·` and `…` stay. Keyboard glyphs in shortcut labels stay text. The `−` in diff counts (`−12`) is a minus sign in a number, not an icon, and stays.
- One icon never stands for every row of a list. Command center action rows get a line icon chosen by action id (new task `square-pen`, find `search`, attach `paperclip`, whiteboard `brush`, rename `pencil`, fork `git-fork`, pin `pin`, scheduled `calendar-clock`, export `download`/`file-code`, copy id `copy`, archive `archive`, side pane `panel-right`, theme `sun`/`moon`, previous/next `arrow-up`/`arrow-down`, back/forward `arrow-left`/`arrow-right`, zoom `plus`/`minus`/`scan`, settings `settings`, reveal `folder-open`, compact `layers`); unknown ids fall back to `command`. `wand-sparkles` marks skills only.

## 5. Motion grammar

Tokens live in `Theme`. Slint `animate` blocks reference them.

| Token | Value | Use |
| --- | --- | --- |
| `dur-press` | 120ms | Press scale (0.97) on buttons and rows |
| `dur-hover` | 150ms | Hover fills, borders, icon tints |
| `dur-menu` | 180ms | Popover and menu entrance (opacity 0→1, scale 0.96→1) |
| `dur-move` | 240ms | Sliding selection highlight, segmented thumb, chevron rotation |
| `dur-dialog` | 240ms | Dialog entrance (opacity, scale 0.96→1, y +8→0) |
| `dur-enter` | 220ms | Expanded detail and empty state entrance (opacity) |
| `dur-panel` | 280ms | Side pane entrance |

Easing:

- `ease-out-quint` for entrances and movement.
- Exits take 70% of the matching entrance duration.

Rules:

- Never start from scale 0. The smallest start is 0.95 with opacity 0.
- Keyboard-driven navigation, such as arrow keys in lists and palette selection, moves without animation.
- Nothing loops while idle. The only loops allowed exist only while a turn runs:
  - the spinner;
  - the working dots;
  - the stop button's ring.
- `Theme.reduce-motion` (default false) turns every duration into 0ms.

## 6. Surfaces

### 6.1 Window

- `MidnightBackdrop` is the bottom layer.
- Three glass panels sit on top with an 8px frame gap: sidebar, conversation and side pane.
- The top bar is transparent.
- Window controls are 40×30 with rounded hover fills. Close turns red on hover.

### 6.2 Sidebar

- **Brand row:** a monochrome mark, the letter H on a `foreground` square, plus the name "Harness" and the history and collapse buttons.
- **New session:** the sidebar's one primary button. Its main part starts a session in the current workspace. A
  chevron part at its right edge opens a menu (user request 2026-11-04):
  - **New session**: same as the main part.
  - **Open folder…**: OS folder picker (on its own thread). Picking a folder saves it as `general.lastWorkspace` and
    restarts Harness in that folder: the window closes, the normal shutdown runs (running and queued turns end as
    `interrupted`, like any exit), then the same executable starts again with `HARNESS_WORKSPACE=<folder>` and that
    folder as its working directory. One process owns the data directory at a time; the new one starts only after
    the old core thread joined. Cancelling the picker changes nothing. While the shown session has a running turn,
    the item first asks "A turn is running. Restart Harness in another folder?" with Cancel / Open folder.
- **Workspace at startup:** `HARNESS_WORKSPACE` when set; otherwise, when the working directory is the executable's
  own folder (a double-click launch) or the filesystem root (a Finder / Dock launch on macOS, whose working directory
  is `/`), and `general.lastWorkspace` is an existing folder, that folder; otherwise, for a root working directory,
  the user's home folder; otherwise the working directory. The resolved folder is then stored as
  `general.lastWorkspace` before the window opens.
- **PATH on macOS:** an app started from Finder or the Dock gets only `/usr/bin:/bin:/usr/sbin:/sbin`, so agent CLIs,
  `node`, `npm` and `gh` installed by Homebrew, nvm or npm are not found. At startup, before any thread starts,
  Harness asks the user's login shell (`$SHELL`, else `/bin/zsh`, run as `-l -i -c` so both profile and rc files
  load, 5 s limit, no stdin, the value read between marker strings) for its `PATH` and puts the directories it lacks after its own, then appends `/opt/homebrew/bin`, `/usr/local/bin` and
  `~/.local/bin` when missing. Children inherit the result. A failing or slow shell leaves the fallback directories
  only. Other platforms are unchanged.
- **Navigation rows:** Search, Scheduled, Projects, Archived.
  - Each has an 18px line icon.
  - One shared highlight pill slides to the hovered row (`dur-move`).
- **Sessions list:**
  - Group headings are plain subtle text.
  - Unread is a 7px accent dot; pinned is a line pin in `icon-subtle`.
  - Row actions fade in on hover.
- **Footer:** an account chip whose avatar initial sits on `surface-hover`, plus a settings button.

### 6.3 Conversation

- The reading column sits on `glass-deep` with the max width unchanged (768px).
- **Empty state:**
  - The heading "What should we build?" and one subline.
  - One bordered list of three starter prompts. A row shows text plus an arrow on hover; clicking it fills the composer.
  - No mark, no orb, no card grid.
- **User message:** right-aligned, solid `user-bubble` fill, `glass-border`, 18px radius with a 6px tail corner.
- **Assistant text:** plain on the column. The header shows the agent name, and a spinner plus "Streaming…" while it streams.
- **Lists:** drawn dots, not `•`.
- **Tool row:**
  - A 36px glass row: a 16px line icon by tool kind in `icon-subtle`, then the title.
  - On the right: a spinner while running, a filled check (success) when done, a filled x (destructive) when failed, a tag for awaiting permission or denied, and a chevron that rotates when expanded.
  - The expanded detail fades in.
- **Thinking row:** a line brain icon and "Thinking…" or "Thought". The text breathes only while active. When expanded, a 2px rule sits on the left.

### 6.4 Composer

- A raised glass card (`glass-strong`, `radius-2xl`, top highlight, `glass-shadow`).
- On focus the border becomes `accent-line`. There is no glow.
- Inside, from top to bottom:
  - attachment chips: neutral glass pills with a line icon;
  - the text input;
  - one toolbar row.
- Toolbar row, left to right:
  - round glass attach button;
  - agent picker;
  - model, mode and effort pickers;
  - queued tag;
  - usage ring;
  - Steer;
  - Stop or Send.
- Send is the primary (ice) round button. Stop is a round glass button with an expanding ring while a turn runs.

### 6.5 Side pane and settings

- The side pane is a glass panel like the conversation. Its header is the panel title plus expand and close buttons.
- The files search field is a glass pill.
- Settings keep their layout. Groups are glass cards and switches animate their thumb.

### 6.6 Settings › Agents

The page was a long flat list: a whole group for one Setup button, every agent's five setting rows always open, and the same robot icon on every row of every list. It becomes a short page of glass cards where each agent opens on demand.

Section cards share one shape: a heading (title, optional one-line description) above a `card` glass panel (`radius-xl`, `card-border`, top highlight, no shadow). Rows inside are separated by `border` hairlines. An optional footer strip at the bottom of a card sits on `surface` with a hairline above it and holds hints and the add action.

Page order, top to bottom:

1. **Header.** Title "Agents" and its one-line description on the left; "Set up agents…" (outline) on the right. The separate Setup group is removed.
2. **Agents card.** One row per agent CLI, in `AgentsUi.cards` order.
   - **Row header** (at least 60px): the name (semibold) with the version in mono `foreground-subtlest` beside it. Under it, a status line: a 6px dot and words.

     | State | Dot | Words |
     | --- | --- | --- |
     | Installed, signed in | `success` | Installed · Signed in |
     | Installed, not signed in | `warning` | Installed · Not signed in |
     | Installed, sign-in unknown | `foreground-subtlest` | Installed · Sign-in unknown |
     | CLI present, adapter missing | `warning` | Adapter missing |
     | CLI missing | `foreground-subtlest` | Not installed |

   - "Installed", "Adapter missing" and "Not installed" stay separate text elements with exactly those labels (first-run gate).
   - A disabled agent shows its name and status in `foreground-subtle` and cannot be opened.
   - Right side, in order: "Install adapter" (outline, small, only while the adapter is missing), the enable switch, and a chevron (only while enabled) that turns 90° when open.
   - Clicking the header outside its controls opens or closes the details. The header gets the `hover` fill while hovered.
   - **Details.** One agent is open at a time. Opening animates height and opacity over `dur-panel` with ease-out-quint; the chevron turns over `dur-move`.
     - Not signed in: a notice line with `circle-alert` in `warning` and the sign-in hint.
     - Defaults: Mode, Model and Effort selects side by side, each with a small `foreground-subtle` label above it. In the stacked (narrow) layout they form one column.
     - Footer strip: `square-terminal` icon, the launch program in mono ("Found on PATH" when none), the arguments in `foreground-subtlest`, and the environment under them when set. On the right, the test result ("Testing…" subtle, latency in `success-surface-foreground`) and "Test connection" (outline, small). A failed test prints its error under the strip in `destructive-surface-foreground`.
3. **Fallback order card.** 48px rows: position number in mono `foreground-subtlest`, name, the state tag when not available, then move up, move down and remove as 26px icon buttons. No icon in front of the name.
4. **Claude accounts card.** Rows: label over the mono id, a status dot line, "Sign in" (outline, small), "Restart to activate" tag when not loaded, and a `trash-2` icon button. Add row: name field, "Add account" (outline) and a `refresh-cw` icon button. The how-to hint and errors sit in the footer strip.
5. **API providers card.** Rows: "label · model" over the mono endpoint, "No API key" and "Restart to activate" tags, and a `trash-2` icon button. The footer strip holds "Add custom provider" and the hint.

Accessible labels stay: "Set up agents…", "Install adapter for {name}", "Enable {name}", "{name} default mode", "{name} default model", "{name} default effort", "Test {name}", "Move {name} up", "Move {name} down", "Disable {name}", "Sign in to {label}", "Remove {label}", "New account name", "Add Claude account", "Refresh accounts". The chevron is "Show {name} settings" or "Hide {name} settings".

## 7. Ownership

All visual values are owned by `Theme` (`ui/theme/tokens.slint`). Components never contain raw colors, except:

- the canvas colors in `ui/theme/backdrop.slint`;
- the window close button's red hover.

`Theme.pref` keeps its meaning: 0 System, 1 Light, 2 Dark. Rust stays the only writer.

`AgentsUi.open-agent` (agent id, empty for none) is view state. Only the Agents page writes it and Rust never reads or writes it. It lives in the global, not in the row, because Rust replaces the whole cards model on every refresh (a connection test, a toggle), which recreates the rows; the open agent must survive that. It starts empty, so every agent starts closed.

## 8. Acceptance scenarios

Run each scenario against the quick build: `cargo build -p harness-slint --profile quick`. Launch with `HARNESS_UI_CORE=fake` and an isolated `HARNESS_HOME`. Capture screenshots with the scratchpad `cl.ps1` helper.

| # | Scenario | Expected |
| --- | --- | --- |
| A1 | Launch, dark mode, empty session | Midnight canvas faintly lit from the top; three glass panels with hairline borders. No colored light fields. |
| A2 | Hover sidebar rows | The highlight pill slides between rows. |
| A3 | Press and hold any button | The button scales to 0.97 and returns on release. |
| A4 | Open the agent picker in the composer | The menu fades and scales in on `glass-strong`. |
| A5 | Conversation with tool calls (fake core) | Tool rows show line icons, a spinner or fill-glyph status, and a rotating chevron. |
| A6 | Focus the composer | The border turns accent; no glow appears. |
| A7 | Settings → Appearance → Light | Frost canvas, white glass, ink primary buttons. All text readable. |
| A8 | Search the UI sources for the §3.5 bans and for Unicode icons | None remain. |
| A9 | Idle for 10s with no running turn | No element animates. |
| A10 | Settings → Agents | Header with "Set up agents…" on the right; agents card with every agent closed, each showing name, version and a status dot line; fallback, accounts and providers as glass cards without repeated row icons. |
| A11 | Click an agent's header, then another's | The first opens with a height animation; opening the second closes the first. |
| A12 | Open an agent, run "Test connection" (fake core) | The cards model refreshes and the agent stays open. |
| A13 | Settings → Agents in light mode, and at a narrow window | Cards stay readable; at the stacked width the three selects form one column and nothing is clipped. |

## 9. Verification limits

- Rust changes are limited to `select_renderer()`, except for the files §10 names. Slint compile errors surface in `cargo build`.
- Shadows, press scale and rounded clips are checked under femtovg. With `HARNESS_RENDERER=software` they degrade to flat, square shapes, but every surface must stay readable.
- Screenshots are manual evidence.
- Contrast is checked by sampling rendered pixels in screenshots.

## 10. Real-agent chat pass (pre-release)

A pre-release run with the real core found the defects below. The run used Claude Code through `claude-agent-acp` in a scratch git workspace, so they are invisible with the fake core. This section widens the Rust scope of §9 to these files:

- `src/markdown.rs`;
- `src/bridge.rs` and `src/bridge/turns.rs`;
- `src/core/mod.rs` and `src/core/hub.rs`.

Protocols, hub crates and stored data do not change.

### 10.1 Rules

**R1 Chat follows the bottom only while the view is at the bottom.**
- **Owner:** `stick-bottom` in `app.slint`, which is view state.
- **Unsticking:** any user scroll (wheel, touchpad or scrollbar) clears it at once.
- **Re-sticking:** it becomes true again only when `content-y` moves down (toward the bottom) and lands within 2px of it, or when the user presses Send or "Scroll to bottom". A `content-y` change that moves up or stays put never re-sticks.
- **Size changes:** while unstuck, changes in content or viewport size write `content-y` only to clamp it into range. They never move a valid position, so a running wheel animation is not cut off.
- **Root cause:**
  - `ListView.scrolled` is `Flickable.flicked`.
  - It fires when a wheel event starts, before the 180ms wheel animation moves `content-y`.
  - The old check therefore read the pre-scroll position and kept the flag true.
  - The next `content-height` change then snapped the view back. That change comes from row re-estimation during the animation, or from streaming.
  - Second cause, found in the real-agent run: one wheel notch up from the bottom. The animation's first frame is still within 2px of the bottom, so a position-only re-stick check set the flag true again at once. The view stayed 60px up with no "Scroll to bottom" button, and the next streamed chunk pulled it back. The second notch worked because it started 60px away.

**R2 Tool cards show the agent's merged tool state.**
- **Problem:** the hub stores merged tool calls, but its live `ToolCall` and `ToolCallUpdated` events carry the raw ACP delta. Example deltas: title "Read calc.js" with no kind, or only a status.
- **Owner:** the adapter (`core/hub.rs`) keeps one merged `ToolCallView` per tool call in the open session's view. It is seeded from the session snapshot and updated with the same rule as the hub's `merge_tool`: a later non-empty field wins.
- **Events:** status, title and detail events are built from the merged view, never from the raw delta:
  - `ToolUpdated` carries the merged status, so a delta without a status no longer flips a finished tool back to running;
  - `ToolDetail` keeps kind, input, content and locations;
  - `Event::ToolTitle { session, id, title }` carries the merged title.
- **Sources:** `ToolCall`, `ToolCallUpdated` and the permission request all go through the merge.
- **Bridge:** it writes `row.title` only when it differs.
- **Example:**
  - Titles read "Read calc.js", "Edit calc.js" and "node test.js", not the initial "Read File", "Edit" and "Terminal".
  - The icons stay read, edit and terminal; they do not fall back to the wrench.

**R3 Tool output and code have no tab glyphs.**
- Tool card parts and code-block bodies expand tabs to 4-column stops before display. The UI and mono fonts have no tab glyph and render a box.
- Tool text content that is exactly one fenced code block is shown without its fence lines, as mono raw text.

**R4 The permission detail is readable text.**
- If the tool input is a JSON object with a string `description`, the detail is that description.
- Otherwise it is up to 6 `key: value` lines from the object's scalar fields. Each value shows its first line, cut at 160 chars with "…".
- Input that is not JSON is shown as is. No input shows the tool kind. Raw JSON braces are never shown.

**R5 The changed-files label is pluralized.**
- "1 file changed", "N files changed", and the same with " (undone)".

**R6 New sessions get a title from the first prompt.**
- **When:** after the first message of a session whose title is still the hub default "New session". "First" means the adapter view has no turns for it yet.
- **Title:** the first non-empty line of the prompt, with whitespace collapsed, at most 60 chars, ending in "…" when cut.
- **Mechanism:** the adapter calls `rename_session`. The hub session row stays the owner, and the UI picks the title up through `SessionUpdated`.
- **User titles win:** a title the user set before the first message is kept.

**R7 Inline code is colored text, not a capsule.**
- **Problem:** Slint's `Style::Code` draws Consolas at 0.85× on a capsule with a fixed border, one per glyph run. It splits at every line wrap, boxes trailing spaces, and cannot be styled.
- **Rendering:** inline code is rendered in the body font as `Theme.ember` text: `#e8b48c` in dark (about 10.9:1 on the canvas) and `#a45a1f` in light (about 5.2:1).
- **Data:**
  - Markdown text blocks, list items and table cells carry light and dark variants: `MdBlock.text-dark` and `MdRow.cells-dark`.
  - `Theme.dark` selects the variant.
  - A block without inline code reuses its light variant.
- **Copy:** copy and CSV export keep the plain code text.

**R8 The collapsed thought row shows a one-line preview.**
- **Problem:** the preview was the raw thought text in a non-wrapping `Text` centered on a 32px row. Slint still breaks a non-wrapping `Text` at `\n`. Multi-line thoughts (Codex sends paragraphs) became a tall block centered on the row: the first line drew about 50px above it, over the previous tool card, and the row itself looked empty.
- **Owner:** the bridge. While a thought streams, it keeps `row.title` (unused by thinking rows) as the preview: the words of the thought joined by single spaces, built from the start of the text until it reaches 200 chars. Later chunks do not recompute a full preview.
- **UI:** `ThinkingRow` shows the preview, elided, on one line. Expanding still shows the full text.

**R9 Agent-owned terminals are not shown as an id.**
- **Problem:** Harness does not offer the ACP client terminal capability, so a `terminal` content block names a terminal Harness cannot read. Codex sends such blocks with ids of 376 to 5252 chars, and the card printed "Terminal <id>" as a wall of base64.
- **Rule:** tool cards skip `terminal` content blocks. The command stays in the title and in Input; output shows when the agent sends text content or raw output. This replaces the "terminal" part of the phase 9 tool card (9.2B R8).

**R10 A code block inside a list item renders as a code block.**
- **Problem:** a top-level list item is one `StyledText`. `StyledText::from_markdown` rejects code blocks, so an item holding a fenced block fell back to plain text: the whole item showed raw `**`, backticks and fence lines.
- **Rendering:** the list is split at the code block:
  - the item's text before the block stays a list row with its marker;
  - the block follows as a normal code block (highlighting, copy, wrap);
  - item text after the block is a text block, de-indented by the item's content indent;
  - later items continue in a new list block, and an ordered list keeps counting.
- Items without code blocks render as before.

**R11 A thematic break is a divider line.**
- **Problem:** a top-level `---` (Codex uses it between sections) showed as the literal text "---".
- **Rendering:** a `rule` block (`MdBlock.kind` 5) draws a 1px `Theme.border` line, centered in a 9px-high row.

**R12 Chat text can be selected and copied.**
- **Problem:** messages were drawn with `Text` and `StyledText`, which take no pointer selection, so no part of a message could be copied. Slint 1.18 has no selectable rich text; only `TextInput` selects.
- **User messages:** the bubble text is a read-only `TextInput` with the same font, color and wrap. Drag selects, double-click selects a word, Ctrl+C copies.
- **Hover actions:** `StyledText` and `TextInput` take pointer events, so a `TouchArea` behind them lost hover over the text and the message actions hid. Each message row's `TouchArea` wraps its content instead (the row takes its size from that content layout) and sees hover over text too.
- **Assistant messages select in place**, like a browser: drag over the rendered text shows a blue highlight on it, double-click selects a word, Ctrl+C copies the plain text. There is no separate select mode.
  - **Mechanism:** each paragraph, heading and list item keeps its `StyledText` for drawing. A read-only `TextInput` with transparent text lies exactly on top of it, at the same width, font size and family (bold for headings). It holds the plain text that `StyledText` lays out: the paragraphs of the parsed Markdown joined by `\n`, nested list markers ("• ", "◦ ", "▪ ", "N. ") and indents included. Both elements shape through the same text engine, so lines and glyphs line up. Only inline bold or italic runs are wider in `StyledText`, so the highlight can drift by a few pixels after them.
  - **Owner:** the Markdown converter (`markdown.rs`) builds the plain text and the link ranges (byte offsets into the plain text) next to the styled text: `MdBlock.plain` / `MdBlock.links` for text and heading blocks, `MdRow.plain` / `MdRow.links` for list items.
  - **Links:** the overlay takes the clicks, so a click with no selection whose byte offset falls in a link range opens that link (`ChatUi.link-at`). A drag cannot start on a link.
  - **Code blocks:** highlighted code gets the same overlay in the mono font; plain code is a read-only `TextInput`.
  - **Limits:** a selection stays inside one block or list item; tables are not selectable (their copy buttons stay). "Copy response" still copies the whole Markdown source.
- **Selection color:** `Theme.text-selection`, a translucent blue, for every read-only selectable text in chat.

**UI font (changed 2026-11-04).** On Windows the UI font is Segoe UI, except when the resolved UI language is Chinese (`zh-CN`, or `system` with a `zh` OS locale): then it is Microsoft YaHei UI, because DirectWrite's script fallback runs without a locale and picks a Japanese face for Han characters. The family follows the language setting at runtime. Han characters inside chat text in an English UI can therefore show Japanese glyph forms; that is accepted.

**Known adapter behavior, not fixed here:** two assistant messages with no tool call between them arrive as one text stream with no separator. Example: "…any files.Nothing from…".

### 10.2 Acceptance scenarios

Run these against the quick build with the real core (`HARNESS_UI_CORE` unset), an isolated `HARNESS_HOME`, and a scratch git workspace.

| # | Scenario | Expected |
| --- | --- | --- |
| B1 | Claude Code: "read calc.js" | The tool card is titled "Read calc.js" with the read icon, also after the call completes. Expanded output shows line numbers followed by spaces, with no boxes and no fence lines. |
| B2 | Claude Code: fix the bug, then run the tests | The permission card shows the command and a one-sentence description, with no braces. After Allow, the terminal card's title is the command. |
| B3 | The edit completes | The turn footer reads "1 file changed". |
| B4 | First message in a new session | The sidebar and header show the prompt's first line. |
| B5 | A reply with inline code | Code spans show in ember with no boxes and wrap like words. After switching to Light they use the light ember. |
| B6 | Long chat at the bottom, one wheel notch up, then wait for streaming | The view moves the full notch and stays. "Scroll to bottom" appears. Streaming does not pull the view. |
| B7 | Press "Scroll to bottom" during streaming | The view jumps to the bottom and keeps following. |
| B8 | Fake-core gate `phase9-chat-ui` | The changes card label is "1 file changed". |
| B9 | Codex turn with multi-paragraph reasoning | Each collapsed "Thought" row shows its preview on its own line. No text overlaps the card above. |
| B10 | Codex runs a shell command | The expanded card shows Input (and output when sent), with no "Terminal <id>" line. |
| B11 | A reply with "1. **Step** (`cmd`):" followed by an indented fenced block, then "2. …" | Row 1 shows bold and ember code, the block is a code block with no fence lines, and the next row is numbered 2. |
| B12 | A reply with a `---` line between two paragraphs | A thin divider line shows; no "---" text. |
