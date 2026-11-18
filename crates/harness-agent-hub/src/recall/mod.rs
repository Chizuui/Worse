//! Local memory that follows the user across sessions and agents ("recall").
//!
//! `MEMORY.md` (see [`crate::memory`]) is one hand-kept document per scope. This store holds many small
//! entries instead, so they can be searched and only the relevant ones are put into a prompt:
//!
//! - **memories**: single facts, preferences, decisions and how-tos, saved by an agent (`memory_add`) or by
//!   the user in Settings;
//! - **episodes**: one automatic digest per session (what was asked, how it ended, which files changed), so a
//!   new session, or another agent, can pick up where an earlier one stopped.
//!
//! Three projects shaped the design; none of their code is used:
//!
//! - *memvid*: one portable file, append-only records with tombstones, a lexical index and a time index, no
//!   embedding model. Here: `recall.jsonl`, one JSON record per line, folded into the live set on load.
//! - *supermemory*: atomic memories with a kind, a scope ("container"), an always-present profile next to
//!   what is recalled for a query, supersede instead of edit (`replaces`), expiry instead of delete.
//! - *Hermes Agent*: a bounded block injected when a session starts and fixed for that session, duplicate and
//!   secret checks on write, a fence that says recalled text is not user input.
//!
//! Synchronous on purpose, like [`crate::memory`]: the MCP server binary has no async runtime, and the hub
//! calls these through `spawn_blocking`.

mod block;
mod guard;
mod search;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use block::{age, episode_text, recall_block, EpisodeTurn, RecallInput, INSTRUCTIONS};
pub use guard::{redact, secret_reason};
pub use search::{search, tokens, Hit, Query};

use crate::memory::LockGuard;

/// Directory under the Harness home.
const DIR: &str = "memory";
const LOG: &str = "recall.jsonl";
const LOCK: &str = "recall.lock";
const CONFIG: &str = "config.json";

/// Longest text of one memory, in characters: a memory is one fact, not a document.
pub const MAX_TEXT_CHARS: usize = 1_000;
/// Episodes are dropped this long after the session last finished a turn.
pub const EPISODE_TTL_MS: i64 = 180 * 24 * 60 * 60 * 1000;
/// The log is rewritten without its dead records when at least this many are dead and they outnumber the
/// live ones.
const COMPACT_MIN_DEAD: usize = 64;

/// What a memory is about. Only `Episode` is written automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Something true about the project or the environment.
    Fact,
    /// How the user wants things done. Pinned by default.
    Preference,
    /// A choice that was made, with its reason.
    Decision,
    /// A procedure that worked.
    Howto,
    /// Digest of one session (automatic).
    Episode,
}

impl Kind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fact => "fact",
            Self::Preference => "preference",
            Self::Decision => "decision",
            Self::Howto => "howto",
            Self::Episode => "episode",
        }
    }

    /// Parses a kind an agent or the UI names; episodes cannot be added by hand.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "fact" => Some(Self::Fact),
            "preference" | "pref" => Some(Self::Preference),
            "decision" => Some(Self::Decision),
            "howto" | "how-to" | "procedure" => Some(Self::Howto),
            _ => None,
        }
    }
}

/// One live memory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Memory {
    pub id: String,
    /// Unix milliseconds of the write.
    pub at: i64,
    pub kind: Kind,
    pub text: String,
    /// `None` = global (every project); `Some(key)` = the project with that [`project_key`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Always put into the recall block of this scope, whatever the request is about.
    #[serde(default, skip_serializing_if = "is_false")]
    pub pinned: bool,
    /// Harness session that wrote it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Display name of the agent that wrote it ("Claude Code"); `None` when the user did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Session title, for episodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// A later record with the same key takes this one's place (episodes: `session:<id>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Id of the memory this one supersedes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces: Option<String>,
    /// Unix milliseconds after which the memory is no longer live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<i64>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    !*value
}

/// One line of the log.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
enum Record {
    Put(Memory),
    Forget { id: String, at: i64 },
    Pin { id: String, at: i64, pinned: bool },
}

/// A memory to save.
#[derive(Debug, Clone, Default)]
pub struct NewMemory {
    pub kind: Option<Kind>,
    pub text: String,
    pub project: Option<String>,
    pub pinned: Option<bool>,
    pub session: Option<String>,
    pub agent: Option<String>,
    pub replaces: Option<String>,
    /// Days until the memory expires.
    pub ttl_days: Option<u32>,
}

/// Result of [`Store::add`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Added {
    New(String),
    /// The same text is already stored in this scope under this id; nothing was written.
    Duplicate(String),
}

impl Added {
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::New(id) | Self::Duplicate(id) => id,
        }
    }
}

/// What the hub does on its own. Read from `config.json` each time, so a change in Settings applies to the
/// next session without a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Config {
    /// Put the recall block into the first prompt of a fresh agent session.
    pub recall: bool,
    /// Write an episode when a turn completes.
    pub capture: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            recall: true,
            capture: true,
        }
    }
}

/// Stable key of a project directory: `/` separators, no trailing separator, lower case on Windows (its
/// paths are case-insensitive). The hub and the MCP helper get the same workspace path, so the same key.
#[must_use]
pub fn project_key(path: &str) -> String {
    let mut key = path.trim().replace('\\', "/");
    while key.len() > 1 && key.ends_with('/') {
        key.pop();
    }
    if cfg!(windows) {
        key = key.to_lowercase();
    }
    key
}

/// Unix milliseconds now.
#[must_use]
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn new_id() -> String {
    let hex = uuid::Uuid::new_v4().simple().to_string();
    format!("m{}", &hex[..10])
}

/// Same text for the duplicate check: case and runs of whitespace do not count.
fn normalized(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// The store: `<home>/memory/recall.jsonl`.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// `home` = Harness data directory.
    #[must_use]
    pub fn new(home: impl AsRef<Path>) -> Self {
        Self {
            dir: home.as_ref().join(DIR),
        }
    }

    /// Path of the log file (for "open folder" and tests).
    #[must_use]
    pub fn log_path(&self) -> PathBuf {
        self.dir.join(LOG)
    }

    /// The switches; defaults when the file is missing or unreadable.
    #[must_use]
    pub fn config(&self) -> Config {
        fs::read_to_string(self.dir.join(CONFIG))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Saves the switches.
    ///
    /// # Errors
    ///
    /// I/O errors.
    pub fn set_config(&self, config: Config) -> std::io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let text = serde_json::to_string_pretty(&config).unwrap_or_else(|_| "{}".into());
        fs::write(self.dir.join(CONFIG), text)
    }

    /// Every live memory, oldest first. A missing file is an empty store; a line that does not parse (a torn
    /// last line after a crash) is skipped.
    #[must_use]
    pub fn load(&self) -> Vec<Memory> {
        self.fold(now_ms()).0
    }

    /// Live memories and the number of dead records in the file.
    fn fold(&self, now: i64) -> (Vec<Memory>, usize) {
        let Ok(text) = fs::read_to_string(self.log_path()) else {
            return (Vec::new(), 0);
        };
        let mut live: Vec<Memory> = Vec::new();
        let mut lines = 0usize;
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            lines += 1;
            let Ok(record) = serde_json::from_str::<Record>(line) else {
                continue;
            };
            match record {
                Record::Put(memory) => {
                    live.retain(|m| {
                        m.id != memory.id
                            && memory.replaces.as_deref() != Some(m.id.as_str())
                            && !(memory.key.is_some() && m.key == memory.key)
                    });
                    live.push(memory);
                }
                Record::Forget { id, .. } => live.retain(|m| m.id != id),
                Record::Pin { id, pinned, .. } => {
                    if let Some(memory) = live.iter_mut().find(|m| m.id == id) {
                        memory.pinned = pinned;
                    }
                }
            }
        }
        live.retain(|m| m.expires.map_or(true, |at| at > now));
        let dead = lines.saturating_sub(live.len());
        (live, dead)
    }

    fn append(&self, records: &[Record]) -> std::io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let _guard = LockGuard::acquire(&self.dir.join(LOCK))?;
        let mut out = String::new();
        // A crash can leave the file without a final newline: start on a fresh line so the torn record does
        // not swallow this one.
        let needs_newline = ends_without_newline(&self.log_path());
        if needs_newline {
            out.push('\n');
        }
        for record in records {
            out.push_str(&serde_json::to_string(record).map_err(std::io::Error::other)?);
            out.push('\n');
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_path())?;
        file.write_all(out.as_bytes())?;
        file.sync_data()?;
        drop(file);
        self.compact_locked();
        Ok(())
    }

    /// Rewrites the log with the live records only (caller holds the lock). Failure leaves the old file.
    fn compact_locked(&self) {
        let (live, dead) = self.fold(now_ms());
        if dead < COMPACT_MIN_DEAD || dead <= live.len() {
            return;
        }
        let mut out = String::new();
        for memory in live {
            if let Ok(line) = serde_json::to_string(&Record::Put(memory)) {
                out.push_str(&line);
                out.push('\n');
            }
        }
        let tmp = self.dir.join(format!(".recall.{}.tmp", std::process::id()));
        let written = fs::File::create(&tmp).and_then(|mut file| {
            file.write_all(out.as_bytes())?;
            file.sync_all()
        });
        if written.is_ok() {
            let _ = fs::rename(&tmp, self.log_path());
        } else {
            let _ = fs::remove_file(&tmp);
        }
    }

    /// Saves a memory. The same text already stored in the same scope is not stored twice.
    ///
    /// # Errors
    ///
    /// A message for the caller: empty or too long text, text that looks like a credential, or an I/O error.
    pub fn add(&self, new: NewMemory) -> Result<Added, String> {
        let text = new.text.trim().to_owned();
        if text.is_empty() {
            return Err("text is empty".into());
        }
        if text.chars().count() > MAX_TEXT_CHARS {
            return Err(format!(
                "text is longer than {MAX_TEXT_CHARS} characters: save one fact per memory, in a sentence or two"
            ));
        }
        if let Some(reason) = secret_reason(&text) {
            return Err(format!(
                "not saved: the text {reason}. Never store credentials in memory"
            ));
        }
        let kind = new.kind.unwrap_or(Kind::Fact);
        if kind == Kind::Episode {
            return Err("episodes are written by Harness, not added".into());
        }
        let live = self.load();
        let same = normalized(&text);
        if let Some(existing) = live.iter().find(|m| {
            m.kind != Kind::Episode
                && m.project == new.project
                && normalized(&m.text) == same
                && new.replaces.as_deref() != Some(m.id.as_str())
        }) {
            return Ok(Added::Duplicate(existing.id.clone()));
        }
        if let Some(old) = &new.replaces {
            if !live.iter().any(|m| &m.id == old && m.kind != Kind::Episode) {
                return Err(format!("no memory with id {old} to replace"));
            }
        }
        let now = now_ms();
        let memory = Memory {
            id: new_id(),
            at: now,
            kind,
            text,
            project: new.project,
            pinned: new.pinned.unwrap_or(kind == Kind::Preference),
            session: new.session,
            agent: new.agent,
            title: None,
            key: None,
            replaces: new.replaces,
            expires: new
                .ttl_days
                .map(|days| now + i64::from(days) * 24 * 60 * 60 * 1000),
        };
        let id = memory.id.clone();
        self.append(&[Record::Put(memory)])
            .map_err(|e| format!("write failed: {e}"))?;
        Ok(Added::New(id))
    }

    /// Writes the episode of a session, taking the place of its earlier one. Credentials in `text` are
    /// redacted, not refused: the digest is cut from the conversation, nobody chose its words.
    ///
    /// # Errors
    ///
    /// I/O errors.
    pub fn put_episode(
        &self,
        session: &str,
        project: &str,
        title: &str,
        agent: &str,
        text: &str,
    ) -> std::io::Result<()> {
        let now = now_ms();
        let text: String = redact(text).chars().take(MAX_TEXT_CHARS * 2).collect();
        let memory = Memory {
            id: new_id(),
            at: now,
            kind: Kind::Episode,
            text,
            project: Some(project_key(project)),
            pinned: false,
            session: Some(session.to_owned()),
            agent: Some(agent.to_owned()).filter(|a| !a.is_empty()),
            title: Some(redact(title)).filter(|t| !t.trim().is_empty()),
            key: Some(format!("session:{session}")),
            replaces: None,
            expires: Some(now + EPISODE_TTL_MS),
        };
        self.append(&[Record::Put(memory)])
    }

    /// Forgets the memory with this id, or, when no id matches, the memories whose whole text is `target`.
    /// Returns how many were forgotten.
    ///
    /// # Errors
    ///
    /// I/O errors.
    pub fn forget(&self, target: &str) -> std::io::Result<usize> {
        let target = target.trim();
        let live = self.load();
        let mut ids: Vec<String> = live
            .iter()
            .filter(|m| m.id == target)
            .map(|m| m.id.clone())
            .collect();
        if ids.is_empty() && !target.is_empty() {
            let same = normalized(target);
            ids = live
                .iter()
                .filter(|m| normalized(&m.text) == same)
                .map(|m| m.id.clone())
                .collect();
        }
        if ids.is_empty() {
            return Ok(0);
        }
        let at = now_ms();
        let records: Vec<Record> = ids
            .iter()
            .map(|id| Record::Forget { id: id.clone(), at })
            .collect();
        self.append(&records)?;
        Ok(ids.len())
    }

    /// Pins or unpins a memory; false when the id is unknown.
    ///
    /// # Errors
    ///
    /// I/O errors.
    pub fn set_pinned(&self, id: &str, pinned: bool) -> std::io::Result<bool> {
        if !self.load().iter().any(|m| m.id == id) {
            return Ok(false);
        }
        self.append(&[Record::Pin {
            id: id.to_owned(),
            at: now_ms(),
            pinned,
        }])?;
        Ok(true)
    }
}

/// True when the file has content that does not end in a newline (only its last byte is read).
fn ends_without_newline(path: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    if file.seek(SeekFrom::End(-1)).is_err() {
        return false;
    }
    let mut last = [0u8; 1];
    file.read_exact(&mut last).is_ok() && last[0] != b'\n'
}

/// The memories a session in `project` may see: the global ones and that project's.
#[must_use]
pub fn visible<'a>(memories: &'a [Memory], project: Option<&str>) -> Vec<&'a Memory> {
    memories
        .iter()
        .filter(|m| m.project.is_none() || m.project.as_deref() == project)
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
