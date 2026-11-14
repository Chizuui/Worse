//! Primary-root status probe, port of `readPrimaryRootStatus` in
//! `packages/services/src/project/index.ts`: one in-flight stat per path shared by concurrent
//! reads, `unavailable` when it has not answered 30 s after it started, no result cache.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// Same budget as `PRIMARY_ROOT_STATUS_TIMEOUT_MS`.
pub const PRIMARY_ROOT_STATUS_TIMEOUT: Duration = Duration::from_secs(30);

/// `VirtualProject["primaryRootStatus"]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootStatus {
    /// A directory.
    Available,
    /// Not found, not a directory, or a file.
    Missing,
    /// Any other error, or no answer in time.
    Unavailable,
}

impl RootStatus {
    /// Wire text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Missing => "missing",
            Self::Unavailable => "unavailable",
        }
    }
}

struct Flight {
    started: Instant,
    result: Mutex<Option<RootStatus>>,
    done: Condvar,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Shared in-flight stats.
#[derive(Default)]
pub struct RootProbe {
    in_flight: Arc<Mutex<HashMap<String, Arc<Flight>>>>,
}

/// Maps a stat failure like libuv + the TS code: ENOENT/ENOTDIR -> missing, else unavailable.
fn classify_error(error: &io::Error) -> RootStatus {
    if matches!(error.kind(), io::ErrorKind::NotFound) {
        return RootStatus::Missing;
    }
    #[cfg(windows)]
    {
        // libuv 把 ERROR_INVALID_DRIVE(15)、ERROR_INVALID_NAME(123)、ERROR_BAD_PATHNAME(161)
        // 映射为 ENOENT，ERROR_DIRECTORY(267) 映射为 ENOTDIR；TS 均视为 missing。
        if matches!(error.raw_os_error(), Some(2 | 3 | 15 | 123 | 161 | 267)) {
            return RootStatus::Missing;
        }
    }
    #[cfg(unix)]
    {
        if matches!(error.raw_os_error(), Some(code) if code == 20) {
            return RootStatus::Missing;
        }
    }
    RootStatus::Unavailable
}

fn stat_status(path: &str) -> RootStatus {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => RootStatus::Available,
        Ok(_) => RootStatus::Missing,
        Err(error) => classify_error(&error),
    }
}

impl RootProbe {
    fn flight(&self, path: &str) -> Arc<Flight> {
        let mut map = lock(&self.in_flight);
        if let Some(existing) = map.get(path) {
            return Arc::clone(existing);
        }
        let flight = Arc::new(Flight {
            started: Instant::now(),
            result: Mutex::new(None),
            done: Condvar::new(),
        });
        map.insert(path.to_owned(), Arc::clone(&flight));
        drop(map);
        let worker = Arc::clone(&flight);
        let registry = Arc::clone(&self.in_flight);
        let owned = path.to_owned();
        let spawned = std::thread::Builder::new()
            .name("store-root-stat".to_owned())
            .spawn(move || {
                let status = stat_status(&owned);
                *lock(&worker.result) = Some(status);
                worker.done.notify_all();
                let mut map = lock(&registry);
                if map
                    .get(&owned)
                    .is_some_and(|current| Arc::ptr_eq(current, &worker))
                {
                    map.remove(&owned);
                }
            });
        if spawned.is_err() {
            // 线程创建失败时按“无法访问”结束本次检查，不留下永不完成的 in-flight 记录。
            *lock(&flight.result) = Some(RootStatus::Unavailable);
            lock(&self.in_flight).remove(path);
        }
        flight
    }

    fn wait(flight: &Flight) -> RootStatus {
        let deadline = flight.started + PRIMARY_ROOT_STATUS_TIMEOUT;
        let mut result = lock(&flight.result);
        loop {
            if let Some(status) = *result {
                return status;
            }
            let now = Instant::now();
            if now >= deadline {
                return RootStatus::Unavailable;
            }
            result = flight
                .done
                .wait_timeout(result, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Statuses for `paths` (in order); all stats run in parallel.
    #[must_use]
    pub fn statuses(&self, paths: &[&str]) -> Vec<RootStatus> {
        let flights: Vec<Arc<Flight>> = paths.iter().map(|path| self.flight(path)).collect();
        flights.iter().map(|flight| Self::wait(flight)).collect()
    }
}

/// Whether TS (`stat` relative to the Host cwd) and Rust (cwd = harness-host binary dir) would
/// resolve the path the same way.
#[must_use]
pub fn is_absolute(path: &str) -> bool {
    Path::new(path).is_absolute()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_dirs_files_and_missing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "x").unwrap();
        let probe = RootProbe::default();
        let d = dir.path().to_string_lossy().into_owned();
        let f = file.to_string_lossy().into_owned();
        let m = dir.path().join("nope").to_string_lossy().into_owned();
        let under_file = file.join("sub").to_string_lossy().into_owned();
        let got = probe.statuses(&[&d, &f, &m, &under_file, &d]);
        assert_eq!(
            got,
            vec![
                RootStatus::Available,
                RootStatus::Missing,
                RootStatus::Missing,
                RootStatus::Missing,
                RootStatus::Available
            ]
        );
    }
}
