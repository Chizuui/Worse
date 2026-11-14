//! Directory identity at creation vs at flush (TS `targetIdentity`: stat
//! follows symlinks, compares dev/ino/isDirectory).

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Identity {
    dev: u64,
    ino: u64,
    is_dir: bool,
}

impl Identity {
    #[cfg(unix)]
    pub(crate) fn of(path: &Path) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(path).ok()?;
        Some(Self {
            dev: meta.dev(),
            ino: meta.ino(),
            is_dir: meta.is_dir(),
        })
    }

    #[cfg(windows)]
    pub(crate) fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        // 句柄只在读取身份时短暂打开，避免长期持有目录句柄阻止用户删除/重命名目录。
        let handle = winapi_util::Handle::from_path_any(path).ok()?;
        let info = winapi_util::file::information(&handle).ok()?;
        Some(Self {
            dev: info.volume_serial_number(),
            ino: info.file_index(),
            is_dir: meta.is_dir(),
        })
    }

    #[cfg(not(any(unix, windows)))]
    pub(crate) fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        Some(Self {
            dev: 0,
            ino: 0,
            is_dir: meta.is_dir(),
        })
    }
}
