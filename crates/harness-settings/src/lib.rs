//! Harness application settings (spec `docs/specs/harness/phase-9-parity.md` §9.1.1–9.1.2).
//!
//! - [`SettingsStore`]: the single owner of `<data dir>/settings.json` (typed [`Settings`], unknown keys preserved,
//!   atomic writes, change events, migration from the Phase 8 `ui-settings.json`).
//! - [`relocation`]: resolves the data dir (`HARNESS_HOME` / `~/.harness` plus the `relocated.json` pointer) and
//!   moves it (copy → verify → switch pointer), safe when interrupted.
//! - [`fsx`]: atomic writes and the credentials-file deny list, shared with `harness-agent-config`.
//! - [`json`]: an order-preserving JSON tree so rewritten files keep their key order.
//! - [`shortcuts`]: accelerator normalisation and conflict detection.

#![allow(clippy::missing_errors_doc)]

mod error;
pub mod fsx;
pub mod json;
mod model;
pub mod relocation;
pub mod shortcuts;
mod store;

pub use error::SettingsError;
pub use model::{
    AgentPrefs, AppearanceSettings, ComposerSend, FollowUp, GeneralSettings, Language,
    MemorySettings, NotificationSettings, ProxySettings, ResearchSettings, Settings,
    TerminalSettings, ThemePref, UpdateSettings, SCHEMA_VERSION,
};
pub use store::{SaveOutcome, SettingsStore, LEGACY_FILE, SETTINGS_FILE};
