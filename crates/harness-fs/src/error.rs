//! Errors raised by file-system operations.
//!
//! The wire contract distinguishes two error families that the TS bridge
//! turns back into exactly the errors the TS services would throw:
//!
//! * [`FsError::Io`]: a Node-style system error (`errno` + `syscall` + `path`),
//!   rebuilt by the bridge as `"<errno>: <description>, <syscall> '<path>'"`.
//! * [`FsError::App`]: a TS `throw new Error(message)` with identical text.

use std::io;
use std::path::Path;

/// A file-system failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FsError {
    /// OS-level failure with libuv naming.
    #[error("{errno}: {}, {syscall} '{path}'", describe(errno))]
    Io {
        /// libuv errno name (`ENOENT`, `EACCES`, ...).
        errno: &'static str,
        /// libuv syscall the TS code path fails in (`stat`, `scandir`, ...).
        syscall: &'static str,
        /// The path as given by the caller.
        path: String,
    },
    /// Application error with the TS message text.
    #[error("{message}")]
    App {
        /// Message identical to the TS implementation.
        message: String,
    },
}

impl FsError {
    /// Maps an OS error raised by `syscall` on `path` to libuv naming.
    #[must_use]
    pub fn from_io(error: &io::Error, syscall: &'static str, path: &Path) -> Self {
        Self::Io {
            errno: errno_for(error, syscall),
            syscall,
            path: path.to_string_lossy().into_owned(),
        }
    }

    /// Application error with the given message.
    #[must_use]
    pub fn app(message: impl Into<String>) -> Self {
        Self::App {
            message: message.into(),
        }
    }

    /// `Path is not a file: <path>` (TS `readTextFile` wording).
    #[must_use]
    pub fn not_a_file(path: &str) -> Self {
        Self::app(format!("Path is not a file: {path}"))
    }

    /// The libuv errno name for I/O errors, `None` for application errors.
    #[must_use]
    pub fn errno(&self) -> Option<&'static str> {
        match self {
            Self::Io { errno, .. } => Some(errno),
            Self::App { .. } => None,
        }
    }
}

/// libuv `uv_strerror` descriptions for the errno names this crate emits.
#[must_use]
pub fn describe(errno: &str) -> &'static str {
    match errno {
        "ENOENT" => "no such file or directory",
        "ENOTDIR" => "not a directory",
        "EACCES" => "permission denied",
        "EPERM" => "operation not permitted",
        "EISDIR" => "illegal operation on a directory",
        "EEXIST" => "file already exists",
        "EBUSY" => "resource busy or locked",
        "ELOOP" => "too many symbolic links encountered",
        "EINVAL" => "invalid argument",
        "EMFILE" => "too many open files",
        "ENOTEMPTY" => "directory not empty",
        "ENAMETOOLONG" => "name too long",
        _ => "i/o error",
    }
}

/// Maps an OS error to the libuv errno name Node would report.
fn errno_for(error: &io::Error, syscall: &str) -> &'static str {
    if let Some(raw) = error.raw_os_error() {
        if let Some(name) = raw_errno(raw, syscall) {
            return name;
        }
    }
    match error.kind() {
        io::ErrorKind::NotFound => "ENOENT",
        io::ErrorKind::PermissionDenied => "EACCES",
        io::ErrorKind::AlreadyExists => "EEXIST",
        io::ErrorKind::InvalidInput => "EINVAL",
        _ => "EIO",
    }
}

#[cfg(windows)]
fn raw_errno(raw: i32, syscall: &str) -> Option<&'static str> {
    // libuv uv_translate_sys_error 的 Windows 映射；rename/unlink 上的拒绝访问在 Node 中报 EPERM。
    Some(match raw {
        2 | 3 | 15 | 53 | 67 | 161 | 1231 => "ENOENT",
        5 if matches!(syscall, "rename" | "unlink" | "rmdir") => "EPERM",
        5 | 1920 => "EACCES",
        32 | 33 | 170 => "EBUSY",
        80 | 183 => "EEXIST",
        87 | 123 | 1113 => "EINVAL",
        // ERROR_DIRECTORY：目录名称无效，即路径不是目录。
        267 => "ENOTDIR",
        145 => "ENOTEMPTY",
        206 => "ENAMETOOLONG",
        1921 => "ELOOP",
        4 => "EMFILE",
        _ => return None,
    })
}

#[cfg(not(windows))]
fn raw_errno(raw: i32, _syscall: &str) -> Option<&'static str> {
    Some(match raw {
        1 => "EPERM",
        2 => "ENOENT",
        13 => "EACCES",
        16 => "EBUSY",
        17 => "EEXIST",
        20 => "ENOTDIR",
        21 => "EISDIR",
        22 => "EINVAL",
        24 => "EMFILE",
        #[cfg(target_os = "linux")]
        39 => "ENOTEMPTY",
        #[cfg(target_os = "linux")]
        40 => "ELOOP",
        #[cfg(target_os = "linux")]
        36 => "ENAMETOOLONG",
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        66 => "ENOTEMPTY",
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        62 => "ELOOP",
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        63 => "ENAMETOOLONG",
        _ => return None,
    })
}
