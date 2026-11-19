# File safety — content-based file type checks

Status: implemented (item 4 of `plans_harness_worse.md`)
Owner crate: `crates/harness-fs` (`src/filetype.rs`)
Related: `docs/specs/harness/security-panel.md` (dependency and sandbox side of the same panel)

## 1. Problem

Agents write files into the workspace. A tool that claims to write `notes.txt` can write a Windows
executable, a script with a shebang, or an archive. The extension is agent-controlled text, the
content is not; the user only ever sees the name in the diff view, the permission card and file
search. Harness therefore inspects the first bytes of the file and warns when the content does not
match what the name claims.

Harness never blocks on a mismatch. It reports it; the user still decides.

## 2. Detector choice

Two detectors sit behind one interface (`harness_fs::filetype`):

| Detector | Cargo feature | Default | Cost |
| --- | --- | --- | --- |
| magic bytes + heuristics | always on | yes | no extra dependency, microseconds per file |
| Magika (ML model) | `magika` | off | `ort` 2.0.0-rc.12 + ONNX Runtime, ~3 MB model |

Why Magika is off by default:

- The `magika` crate (1.1.0, Apache-2.0, code and model weights both Apache-2.0 as stated by the
  upstream repository `LICENSE` and model README) does **not** bundle an inference runtime. It
  depends on `ort =2.0.0-rc.12`, which either downloads ONNX Runtime at build time
  (`ort/download-binaries`) or needs `libonnxruntime` present per platform. That breaks the
  single-exe packaging rule (`docs/specs/harness/desktop-window-and-packaging.md`) and adds a
  native library per target.
- Upstream `main` is migrating from ONNX Runtime to a pure-Rust runtime; until that ships in a
  published crate the dependency is a moving target.
- The warnings we need — executable, script, archive, image, media, encrypted-looking data — are
  decided by magic bytes with full confidence. Magika's advantage is fine-grained text/code
  labelling (which source language), which is not a safety signal.

So the shipped default is a self-contained detector; turning on `magika` swaps the backend without
touching callers. When the pure-Rust Magika release lands, the feature can become the default.

## 3. Where checks run

| Surface | Path | What the user sees |
| --- | --- | --- |
| Diff panel | `apps/harness-slint/src/panels/worker.rs` (`render_diff`, `with_content_warning`) | warning appended to the file notice: `Content looks like: Windows executable` |
| File search | `apps/harness-slint/src/panels/worker.rs` (search summary) | `N files have content that does not match their names` appended to the file-search summary |
| Files an agent wrote | same as diff panel — the diff view is where agent writes show up | as above |

The permission card is the third surface: `with_content_warnings` in
`apps/harness-slint/src/core/hub.rs` runs `harness_workspace::filetype::inspect` on up to four
existing files among the tool call's locations and appends `⚠ <name>: Content looks like: …` lines to
the card detail.

Checks are synchronous and cheap enough to run inline in the panel worker thread; the detector reads
at most 8 KiB per file (`MAX_SNIFF_BYTES`).

### 3.1 Thresholds and rules

- Files smaller than 8 bytes are `unknown` and never warn (too little signal).
- A warning needs a *confident* detection (magic-byte match or a shebang) **and** a mismatch with the
  extension group: an executable/archive/media/image/script kind whose extension group is `text`,
  or a text kind whose extension group is `executable`.
- Two extensions always count as text and never warn on text content: none — the rule is symmetric,
  see the table in §3.2.
- A file with no extension warns only when it is executable content (PE/ELF/Mach-O); shebang scripts
  without extension are normal in `bin/` style layouts and do not warn.
- Symlinks and non-regular files are not inspected.
- Read errors produce `unknown` plus a note; they never produce a warning.

### 3.2 Kinds and extension groups

| Kind | Signal | Extension group |
| --- | --- | --- |
| `pe` | `MZ` + PE header at `e_lfanew` | `.exe .dll .sys .scr .com .cpl .ocx` |
| `elf` | `\x7fELF` | none (no extension) |
| `macho` | `\xfe\xed\xfa\xce` / `\xcf\xfa\xed\xfe` / fat `\xca\xfe\xba\xbe` | none |
| `script` | `#!` shebang | `.sh .bash .zsh .ps1 .py .rb .pl .js .bat .cmd` |
| `archive` | ZIP `PK\x03\x04`, gzip, bzip2, xz, zstd, 7z, RAR, tar at 257 | `.zip .gz .bz2 .xz .zst .7z .rar .tar .jar .whl .docx .xlsx .apk` |
| `image` | PNG, JPEG, GIF, BMP, WEBP, TIFF, ICO | `.png .jpg .jpeg .gif .bmp .webp .tif .tiff .ico` |
| `media` | MP3, OGG, FLAC, RIFF/WAV/AVI, MP4/`ftyp`, Matroska | `.mp3 .ogg .flac .wav .mp4 .mkv .avi .webm` |
| `document` | `%PDF`, PostScript | `.pdf .ps` |
| `text` | no NUL byte and mostly printable (`harness_fs::is_probably_binary` false) | everything else |
| `binary` | binary but matching no signature | `.bin`, unknown |

Mismatch matrix (warning, `!` = warn):

| Content kind | ext `text`-ish | ext `executable` | ext other (`archive`/`image`/…) | no extension |
| --- | --- | --- | --- | --- |
| `pe`/`elf`/`macho` | ! | ok | ! | ! |
| `script` | ok | ! | ! | ok |
| `archive`/`image`/`media`/`document` | ! | ! | ok | ! (except `archive` without name) |
| `text` | ok | ! | ! | ok |
| `binary` | ! | ok | ok | ! |

`executable` covers `.exe .dll .so .dylib .sys .bat .cmd .ps1 .sh .py`.

## 4. Cache

`filetype::ContentCache` keyed by absolute path, invalidated by `(mtime, len)`. Bounded to 512
entries with LRU eviction, one mutex, no background thread. A hit costs one `stat`. The cache lives in
whichever owner asks (panel worker, permission path); no global state in the crate.

## 5. Acceptance scenarios

1. `cargo test -p harness-fs` covers: PE bytes named `.txt` warn with `Content looks like: Windows
   executable`; a real text file named `.txt` does not warn; a shebang script named `.png` warns; a
   PNG named `.txt` warns; an 8-byte file never warns; an ELF without extension warns; the cache
   reports once per `(mtime, len)`.
2. Manual: rename `harness-slint.exe` to `notes.txt`, ask an agent to edit it, open the diff view —
   the notice line shows the warning; approve is still possible. With approvals on, the permission
   card shows the same warning line (§3).
3. File search for the same file appends `1 file has content that does not match its name` to the
   summary.
4. With `--features magika` the same inputs produce the same kinds (the ML backend also returns
   `pebin` for PE, `elf`, `macho`, `zip`…), and the binary size grows by ~15 MB when ONNX Runtime is
   linked; without the feature nothing changes.

## 6. Out of scope

- Sandboxing, quarantine and blocking writes: that is the permission policy's job
  (`docs/specs/harness/phase-9-parity.md`).
- Full `file(1)` or libmagic parity; Magika is the escalation path.
- Content scanning for malware signatures (see `security-panel.md` for the dependency surface).