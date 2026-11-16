//! Working notes of one Harness session: relevant files with line pointers, findings, decisions and next
//! steps, kept by whichever agent works on the conversation.
//!
//! The notes are injected into the prompt when another agent takes the session over (quota failover) or a
//! native session starts fresh (the agent CLI compacted its own context), and the agent maintains them
//! through the `session_notes_read` / `session_notes_write` tools of the `harness-memory` MCP server.
//!
//! Synchronous on purpose, like [`crate::memory`]: the MCP server binary has no async runtime.

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

/// Longest notes text accepted by [`write`], in characters. Longer text has to be condensed first.
pub const MAX_NOTES_CHARS: usize = 16_000;

/// Directory under `HARNESS_HOME` holding one `<session>.md` per session.
const DIR: &str = "session-notes";

/// Prompt instructions carried with the notes.
const INSTRUCTIONS: &str = "These are the working notes of this conversation, kept up to date by \
whichever agent worked on it (Harness switches agents on quota failover and a native session can start \
fresh). Trust them as a starting point, but verify every file/line pointer before you edit. Keep them \
current: after each meaningful discovery or decision, call the `session_notes_write` tool of the \
`harness-memory` MCP server with the full notes text (relevant files with line pointers, root causes, \
decisions, what is done, what is next). Read them back with `session_notes_read` if your context was \
compacted or you are unsure what was already established. Never put secrets (API keys, tokens, \
passwords) in the notes.";

/// `..\..\x`-safe file name of a session id: ASCII alphanumerics, `-` and `_` survive, everything else
/// becomes `_`, so the notes of one session can never escape [`DIR`].
fn file_name(session_id: &str) -> Option<String> {
    if session_id.trim().is_empty() {
        return None;
    }
    let sanitized: String = session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    Some(format!("{sanitized}.md"))
}

/// Path of the notes file of `session_id`.
///
/// # Errors
///
/// The session id is empty.
pub fn path(home: &Path, session_id: &str) -> Result<PathBuf, String> {
    let name = file_name(session_id).ok_or_else(|| "session id is empty".to_owned())?;
    Ok(home.join(DIR).join(name))
}

/// Notes of `session_id`; missing or unreadable file = empty notes (never fails).
#[must_use]
pub fn read(home: &Path, session_id: &str) -> String {
    let Ok(path) = path(home, session_id) else {
        return String::new();
    };
    match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => String::new(),
        Err(_) => String::new(),
    }
}

/// Replaces the notes of `session_id`; empty `content` clears them.
///
/// # Errors
///
/// Empty session id, notes longer than [`MAX_NOTES_CHARS`], or I/O errors.
pub fn write(home: &Path, session_id: &str, content: &str) -> Result<(), String> {
    let path = path(home, session_id)?;
    let chars = content.chars().count();
    if chars > MAX_NOTES_CHARS {
        return Err(format!(
            "notes are {chars} characters, the limit is {MAX_NOTES_CHARS}: condense them (drop resolved \
items and detail that is cheap to re-read) and write the full text again"
        ));
    }
    let dir = path
        .parent()
        .ok_or_else(|| "notes path has no parent".to_owned())?;
    fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let text = match content.trim_end() {
        "" => String::new(),
        body => format!("{body}\n"),
    };
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    {
        let mut file =
            fs::File::create(&tmp).map_err(|e| format!("creating {}: {e}", tmp.display()))?;
        file.write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|e| format!("writing {}: {e}", tmp.display()))?;
    }
    fs::rename(&tmp, &path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("replacing {}: {e}", path.display())
    })
}

/// Prompt block carrying the notes into a fresh agent context (never empty, never fails).
#[must_use]
pub fn block(home: &Path, session_id: &str) -> String {
    let notes = read(home, session_id);
    let notes = if notes.trim().is_empty() {
        "(no notes yet)"
    } else {
        notes.trim_end()
    };
    format!("<harness-session-notes>\n{INSTRUCTIONS}\n\n{notes}\n</harness-session-notes>")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn temp_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "harness-session-notes-{}-{tag}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn round_trip_replaces_the_notes() {
        let home = temp_home("round-trip");
        assert_eq!(read(&home, "ses-1"), "");
        write(&home, "ses-1", "files:\n- a.rs:12").unwrap();
        assert_eq!(read(&home, "ses-1"), "files:\n- a.rs:12\n");
        write(&home, "ses-1", "next: test").unwrap();
        assert_eq!(read(&home, "ses-1"), "next: test\n");
        assert_eq!(read(&home, "ses-2"), "");
        write(&home, "ses-1", "").unwrap();
        assert_eq!(read(&home, "ses-1"), "");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn hostile_session_id_stays_inside_the_notes_dir() {
        let home = temp_home("hostile");
        let notes = path(&home, "..\\..\\x").unwrap();
        assert_eq!(notes, home.join(DIR).join("______x.md"));
        assert_eq!(notes.parent(), Some(home.join(DIR).as_path()));
        write(&home, "..\\..\\x", "note").unwrap();
        assert_eq!(read(&home, "..\\..\\x"), "note\n");
        assert!(path(&home, "   ").is_err());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn notes_over_the_cap_are_rejected() {
        let home = temp_home("cap");
        let longest = "x".repeat(MAX_NOTES_CHARS);
        write(&home, "ses-cap", &longest).unwrap();
        let error = write(&home, "ses-cap", &format!("{longest}x")).unwrap_err();
        assert!(error.contains(&MAX_NOTES_CHARS.to_string()), "{error}");
        assert_eq!(read(&home, "ses-cap"), format!("{longest}\n"));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn block_always_carries_the_instructions() {
        let home = temp_home("block");
        let empty = block(&home, "ses-none");
        assert!(empty.starts_with("<harness-session-notes>\n"));
        assert!(empty.ends_with("</harness-session-notes>"));
        assert!(empty.contains("(no notes yet)"));
        assert!(empty.contains("session_notes_write"));
        write(&home, "ses-none", "root cause: pool.rs:88").unwrap();
        let filled = block(&home, "ses-none");
        assert!(filled.contains("root cause: pool.rs:88"));
        assert!(!filled.contains("(no notes yet)"));
        let _ = fs::remove_dir_all(&home);
    }
}
