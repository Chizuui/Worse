//! Perfetto trace recorder for the Harness shell.
//!
//! The crate owns one recorder per process and encodes a `perfetto.protos.Trace`
//! by hand (see `docs/specs/harness/performance-recorder.md`), so recording
//! needs no native SDK, no `protoc` and no dependency in the release build.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod model;
mod proto;
mod trace;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

pub use model::Counter;
use model::{State, Value};

/// Guard for an open span. Dropping it closes the span on the same track.
pub struct Span {
    open: bool,
}

impl Span {
    /// Closes the span. Calling it twice is harmless.
    pub fn end(mut self) {
        self.close();
    }

    fn close(&mut self) {
        if self.open {
            self.open = false;
            with_state(model::State::end);
        }
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        self.close();
    }
}

/// What a finished recording produced.
#[derive(Clone, Debug)]
pub struct TraceWritten {
    pub path: PathBuf,
    pub bytes: u64,
    pub slices: usize,
    pub duration: Duration,
    pub truncated: bool,
}

#[derive(Debug)]
pub enum PerfError {
    AlreadyRecording,
    NotRecording,
    Io(std::io::Error),
}

impl std::fmt::Display for PerfError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyRecording => formatter.write_str("a recording is already running"),
            Self::NotRecording => formatter.write_str("no recording is running"),
            Self::Io(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for PerfError {}

impl From<std::io::Error> for PerfError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();
static ACTIVE: AtomicBool = AtomicBool::new(false);

fn state() -> &'static Mutex<State> {
    STATE.get_or_init(|| Mutex::new(State::new()))
}

fn lock() -> MutexGuard<'static, State> {
    state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn with_state(apply: impl FnOnce(&mut State)) {
    if !ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    let mut guard = lock();
    if guard.active {
        apply(&mut guard);
    }
}

/// Whether a recording is running. One relaxed atomic load: cheap enough for
/// every frame.
#[must_use]
pub fn is_recording() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// Starts a recording. Fails when one is already running.
///
/// # Errors
///
/// Returns [`PerfError::AlreadyRecording`] when a recording is in progress.
pub fn start() -> Result<(), PerfError> {
    let mut guard = lock();
    if guard.active {
        return Err(PerfError::AlreadyRecording);
    }
    guard.begin_recording(Instant::now());
    ACTIVE.store(true, Ordering::Relaxed);
    Ok(())
}

/// Seconds recorded so far, or `None` when no recording runs.
#[must_use]
pub fn elapsed() -> Option<Duration> {
    if !is_recording() {
        return None;
    }
    Some(lock().elapsed())
}

/// Slice count buffered so far.
#[must_use]
pub fn slice_count() -> usize {
    if !is_recording() {
        return 0;
    }
    lock().events.len()
}

/// Stops the recording and writes the trace under `directory`.
///
/// # Errors
///
/// Returns [`PerfError::NotRecording`] when nothing is being recorded, and
/// [`PerfError::Io`] when the directory or the file cannot be written.
pub fn stop(directory: &Path) -> Result<TraceWritten, PerfError> {
    let mut guard = lock();
    if !guard.active {
        return Err(PerfError::NotRecording);
    }
    guard.active = false;
    ACTIVE.store(false, Ordering::Relaxed);

    let duration = guard.elapsed();
    let encoded = trace::encode(
        &guard.events,
        &guard.threads,
        guard.start_realtime_ns,
        u64::from(std::process::id()),
    );
    let truncated = guard.overflowed;
    let path = write_trace(directory, &encoded.bytes)?;
    Ok(TraceWritten {
        path,
        bytes: u64::try_from(encoded.bytes.len()).unwrap_or(0),
        slices: encoded.slices,
        duration,
        truncated,
    })
}

fn write_trace(directory: &Path, bytes: &[u8]) -> Result<PathBuf, PerfError> {
    std::fs::create_dir_all(directory)?;
    let path = directory.join(trace_file_name());
    std::fs::write(&path, bytes)?;
    Ok(path)
}

fn trace_file_name() -> String {
    format!("{}.perfetto-trace", local_stamp())
}

fn local_stamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let seconds_of_day = now % 86_400;
    let (year, month, day) = civil_from_days(i64::try_from(now / 86_400).unwrap_or(0));
    format!(
        "{year:04}-{month:02}-{day:02}-{:02}{:02}{:02}",
        seconds_of_day / 3600,
        (seconds_of_day % 3600) / 60,
        seconds_of_day % 60
    )
}

fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Opens a span on the current thread's track. Closing it is mandatory: it
/// happens on drop.
#[must_use]
pub fn span(name: &'static str) -> Span {
    span_with(name, Vec::new())
}

/// Opens a span carrying annotations.
#[must_use]
pub fn span_with(name: &'static str, annotations: Vec<(&'static str, AnnotatedValue)>) -> Span {
    if !is_recording() {
        return Span { open: false };
    }
    with_state(|state| {
        state.begin(
            name,
            annotations
                .into_iter()
                .map(|(key, value)| (key, value.into_value()))
                .collect(),
        );
    });
    Span { open: true }
}

/// Records a point in time on the current thread's track.
pub fn instant(name: &'static str) {
    with_state(|state| state.instant(name));
}

/// Records a counter sample.
pub fn counter(counter: Counter, value: i64) {
    with_state(|state| state.counter(counter, Value::Int(value)));
}

/// Records a fractional counter sample.
pub fn counter_float(counter: Counter, value: f64) {
    with_state(|state| state.counter(counter, Value::Float(value)));
}

/// Annotation value at a call site.
#[derive(Clone, Copy, Debug)]
pub enum AnnotatedValue {
    Int(i64),
    Float(f64),
    Text(&'static str),
}

impl AnnotatedValue {
    const fn into_value(self) -> Value {
        match self {
            Self::Int(value) => Value::Int(value),
            Self::Float(value) => Value::Float(value),
            Self::Text(value) => Value::Text(value),
        }
    }
}

#[cfg(test)]
mod tests;
