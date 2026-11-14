//! Caller-facing handles and NDJSON streams of a supervised sidecar.

use std::sync::Arc;

use harness_core::SidecarId;
use tokio::io::{AsyncWriteExt, BufReader, Lines};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::Mutex;

use crate::error::SidecarError;

/// Line-oriented stdout stream of a sidecar process.
pub type SidecarStdout = Lines<BufReader<ChildStdout>>;

/// Shared stdin pipe of a sidecar process.
///
/// The supervisor keeps a clone so the shutdown barrier can close the pipe and
/// signal end-of-input even after the caller took the I/O pair.
#[derive(Clone)]
pub struct SidecarStdin {
    inner: Arc<Mutex<Option<ChildStdin>>>,
}

impl SidecarStdin {
    pub(crate) fn new(stdin: ChildStdin) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Some(stdin))),
        }
    }

    /// Writes one NDJSON line and flushes it.
    ///
    /// # Errors
    ///
    /// Returns [`SidecarError::Io`] when the pipe is already closed or the
    /// write fails.
    pub async fn write_line(&self, line: &str) -> Result<(), SidecarError> {
        let mut guard = self.inner.lock().await;
        let stdin = guard
            .as_mut()
            .ok_or_else(|| SidecarError::Io("stdin is closed".to_owned()))?;
        stdin.write_all(line.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        Ok(())
    }

    /// Closes the pipe and signals end-of-input to the child.
    ///
    /// Returns `true` when this call closed an open pipe; later calls are
    /// no-ops returning `false`.
    pub async fn close(&self) -> bool {
        self.inner.lock().await.take().is_some()
    }

    /// Reports whether the pipe is still open.
    pub async fn is_open(&self) -> bool {
        self.inner.lock().await.is_some()
    }
}

/// The stdin/stdout pair of a sidecar process.
pub struct SidecarIo {
    /// Closable stdin pipe used to send NDJSON lines.
    pub stdin: SidecarStdin,
    /// Line-oriented stdout pipe used to receive NDJSON lines.
    pub stdout: SidecarStdout,
}

impl SidecarIo {
    /// Reads the next NDJSON line, returning `None` at end of stream.
    ///
    /// # Errors
    ///
    /// Returns [`SidecarError::Io`] when the pipe cannot be read.
    pub async fn recv_line(&mut self) -> Result<Option<String>, SidecarError> {
        Ok(self.stdout.next_line().await?)
    }
}

/// Caller-facing handle of a running sidecar process.
pub struct SidecarHandle {
    id: SidecarId,
    pid: Option<u32>,
    stdin: Option<SidecarStdin>,
    stdout: Option<SidecarStdout>,
}

impl SidecarHandle {
    pub(crate) fn new(
        id: SidecarId,
        pid: Option<u32>,
        stdin: SidecarStdin,
        stdout: SidecarStdout,
    ) -> Self {
        Self {
            id,
            pid,
            stdin: Some(stdin),
            stdout: Some(stdout),
        }
    }

    /// Returns the identifier the process was started with.
    #[must_use]
    pub fn id(&self) -> &SidecarId {
        &self.id
    }

    /// Returns the operating system process identifier, when the platform
    /// reported one at spawn time.
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Takes the stdin/stdout pair of the process.
    ///
    /// Returns `None` when the pair was already taken; the supervisor keeps
    /// ownership of the process either way.
    pub fn take_io(&mut self) -> Option<SidecarIo> {
        let stdin = self.stdin.take()?;
        let stdout = self.stdout.take()?;
        Some(SidecarIo { stdin, stdout })
    }

    /// Reports whether the stdin/stdout pair is still held by the handle.
    #[must_use]
    pub fn has_io(&self) -> bool {
        self.stdin.is_some() && self.stdout.is_some()
    }
}

impl std::fmt::Debug for SidecarHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SidecarHandle")
            .field("id", &self.id)
            .field("pid", &self.pid)
            .field("has_io", &self.has_io())
            .finish_non_exhaustive()
    }
}
