//! Ports of `resolveTerminalShell`, `resolveTerminalCwd`, `resolveTerminalEnv`
//! and `resolveTerminalWindowsPtyInfo` from `ZCode`'s `terminalService.ts`.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;

#[cfg(windows)]
const PATH_DELIMITER: char = ';';
#[cfg(not(windows))]
const PATH_DELIMITER: char = ':';

/// Ordered environment with platform key semantics (case-insensitive keys on
/// Windows, like Node's `process.env`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvMap {
    entries: BTreeMap<String, (String, String)>,
}

impl EnvMap {
    fn fold(key: &str) -> String {
        if cfg!(windows) {
            key.to_ascii_uppercase()
        } else {
            key.to_owned()
        }
    }

    /// The current process environment (non-UTF-8 entries are converted lossily).
    #[must_use]
    pub fn from_process() -> Self {
        let mut env = Self::default();
        for (key, value) in std::env::vars_os() {
            env.set(&key.to_string_lossy(), &value.to_string_lossy());
        }
        env
    }

    /// Builds from pairs.
    #[must_use]
    pub fn from_pairs<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let mut env = Self::default();
        for (key, value) in pairs {
            env.set(key, value);
        }
        env
    }

    /// Value of `key`.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries.get(&Self::fold(key)).map(|(_, v)| v.as_str())
    }

    /// Sets `key`, keeping the original key spelling when it already exists.
    pub fn set(&mut self, key: &str, value: &str) {
        let folded = Self::fold(key);
        match self.entries.get_mut(&folded) {
            Some(entry) => value.clone_into(&mut entry.1),
            None => {
                self.entries
                    .insert(folded, (key.to_owned(), value.to_owned()));
            }
        }
    }

    /// Removes `key`.
    pub fn remove(&mut self, key: &str) {
        self.entries.remove(&Self::fold(key));
    }

    /// `(key, value)` pairs with their original spelling.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.values().map(|(k, v)| (k.as_str(), v.as_str()))
    }
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        // Node 在 Windows 上的 accessSync(X_OK) 只检查存在性。
        let _ = meta;
        true
    }
}

/// TS `isExecutable`: a path with a separator is checked directly, a bare
/// name against every `PATH` entry.
#[must_use]
pub fn is_executable(command: &str, env: &EnvMap) -> bool {
    if command.contains('/') || command.contains('\\') {
        return is_executable_file(Path::new(command));
    }
    let Some(path_env) = env.get("PATH").filter(|p| !p.is_empty()) else {
        return false;
    };
    path_env
        .split(PATH_DELIMITER)
        .filter(|dir| !dir.is_empty())
        .any(|dir| is_executable_file(&Path::new(dir).join(command)))
}

/// TS `resolveTerminalShell`.
///
/// # Errors
/// The TS error text when no candidate is executable.
pub fn resolve_shell(env: &EnvMap) -> Result<String, String> {
    if cfg!(windows) {
        // 与 TS 一致：优先 pwsh（PowerShell 5.1 的 PSReadLine 在 ConPTY 下重绘问题更多），再回退系统 shell。
        let candidates = [
            Some("pwsh.exe"),
            Some("powershell.exe"),
            env.get("ComSpec"),
            Some("cmd.exe"),
        ];
        for candidate in candidates.into_iter().flatten() {
            if !candidate.is_empty() && is_executable(candidate, env) {
                return Ok(candidate.to_owned());
            }
        }
        return Err("No usable Windows shell found for terminal startup".to_owned());
    }
    let candidates = [
        env.get("SHELL"),
        Some("/bin/zsh"),
        Some("/bin/bash"),
        Some("/bin/sh"),
    ];
    for candidate in candidates.into_iter().flatten() {
        if !candidate.is_empty() && is_executable(candidate, env) {
            return Ok(candidate.to_owned());
        }
    }
    Err("No usable shell found for terminal startup".to_owned())
}

fn home_dir(env: &EnvMap) -> Option<String> {
    let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    env.get(key)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .or_else(|| std::env::home_dir().map(|p| p.to_string_lossy().into_owned()))
}

/// TS `resolveTerminalCwd`: the requested directory, else `HOME`, else the
/// OS home, else `/`.
///
/// # Errors
/// The TS error text when nothing is a directory.
pub fn resolve_cwd(cwd: Option<&str>, env: &EnvMap) -> Result<String, String> {
    let home = home_dir(env);
    let candidates = [cwd, env.get("HOME"), home.as_deref(), Some("/")];
    for candidate in candidates.into_iter().flatten() {
        if !candidate.is_empty() && Path::new(candidate).is_dir() {
            return Ok(candidate.to_owned());
        }
    }
    Err("No usable working directory found for terminal startup".to_owned())
}

fn is_utf8_locale(value: Option<&str>) -> bool {
    value.is_some_and(|v| {
        let lower = v.to_ascii_lowercase();
        lower.contains("utf8") || lower.contains("utf-8")
    })
}

fn is_missing_or_c_locale(value: Option<&str>) -> bool {
    let normalized = value.unwrap_or("").trim().to_ascii_uppercase();
    normalized.is_empty() || normalized == "C" || normalized == "POSIX"
}

const DARWIN_GUI_FALLBACK_PATHS: [&str; 8] = [
    "/opt/homebrew/bin",
    "/opt/homebrew/sbin",
    "/usr/local/bin",
    "/usr/local/sbin",
    "/usr/bin",
    "/bin",
    "/usr/sbin",
    "/sbin",
];

fn merge_path_entries(entries: &[Option<&str>]) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut merged: Vec<&str> = Vec::new();
    for value in entries.iter().flatten() {
        for entry in value.split(PATH_DELIMITER) {
            let trimmed = entry.trim();
            if trimmed.is_empty() || !seen.insert(trimmed) {
                continue;
            }
            merged.push(trimmed);
        }
    }
    merged.join(&PATH_DELIMITER.to_string())
}

/// TS `resolveTerminalEnv`: interactive `TERM`/`COLORTERM`, macOS GUI `PATH`
/// fallback, UTF-8 locale only where missing or C/POSIX.
#[must_use]
pub fn resolve_env(env: &EnvMap) -> EnvMap {
    let mut next = env.clone();
    let fallback_locale = [env.get("LC_ALL"), env.get("LC_CTYPE"), env.get("LANG")]
        .into_iter()
        .find(|v| is_utf8_locale(*v))
        .flatten()
        .map_or_else(
            || {
                if cfg!(target_os = "macos") {
                    "en_US.UTF-8".to_owned()
                } else {
                    "C.UTF-8".to_owned()
                }
            },
            str::to_owned,
        );
    if cfg!(target_os = "macos") {
        let mut entries = vec![env.get("PATH")];
        entries.extend(DARWIN_GUI_FALLBACK_PATHS.iter().map(|p| Some(*p)));
        next.set("PATH", &merge_path_entries(&entries));
    }
    next.set("TERM", "xterm-256color");
    let colorterm = next
        .get("COLORTERM")
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or("truecolor")
        .to_owned();
    next.set("COLORTERM", &colorterm);
    if next.get("CI") == Some("1") && env.get("TERM") == Some("dumb") {
        next.remove("CI");
    }
    if is_missing_or_c_locale(next.get("LANG")) {
        next.set("LANG", &fallback_locale);
    }
    if is_missing_or_c_locale(next.get("LC_CTYPE")) {
        next.set("LC_CTYPE", &fallback_locale);
    }
    if next.get("LC_ALL").is_some() && is_missing_or_c_locale(next.get("LC_ALL")) {
        next.set("LC_ALL", &fallback_locale);
    }
    next
}

/// Contract r2 default env for callers that send none: the process env plus
/// `TERM=xterm-256color` and `COLORTERM` defaulted to `truecolor`. (The bridge
/// always sends the full TS `resolveTerminalEnv` result instead.)
#[must_use]
pub fn default_env() -> EnvMap {
    let mut env = EnvMap::from_process();
    env.set("TERM", "xterm-256color");
    let colorterm = env
        .get("COLORTERM")
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or("truecolor")
        .to_owned();
    env.set("COLORTERM", &colorterm);
    env
}

/// TS `TerminalWindowsPtyInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowsPtyInfo {
    /// Always `"conpty"` (`ConPTY` is the only backend).
    pub backend: &'static str,
    /// Windows build (the third field of Node `os.release()`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_number: Option<u32>,
}

/// TS `resolveTerminalWindowsPtyInfo`: `Some` only on Windows.
#[must_use]
pub fn windows_pty_info() -> Option<WindowsPtyInfo> {
    #[cfg(windows)]
    {
        Some(WindowsPtyInfo {
            backend: "conpty",
            build_number: Some(windows_version::OsVersion::current().build),
        })
    }
    #[cfg(not(windows))]
    {
        None
    }
}
