//! Error type.

/// Settings store failures.
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    /// File system failure.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// The file is not valid JSON or has the wrong shape.
    #[error("invalid settings file {path}: {message}")]
    Invalid { path: String, message: String },
    /// The file was written by a newer Harness (`schemaVersion` greater than ours): read-only.
    #[error("settings.json has schemaVersion {found}; this Harness understands {supported} and will not overwrite it")]
    NewerSchema { found: u64, supported: u64 },
    /// Data dir relocation refused or failed (the pointer is unchanged).
    #[error("data dir relocation: {0}")]
    Relocation(String),
}
