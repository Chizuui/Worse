//! Supervisor of the tunnel helper (docs/plans/android-apk-and-tunnel.md §3.1).
//!
//! One child process: `<tunnel_bin> http <port> [--name <subdomain>]`, stdout and stderr piped, no console
//! window. The public URL is the first `https://…` token in that output; when the helper prints none within
//! [`TunnelConfig::timeout`] the configured `<subdomain>.<tunnel_domain>` is used, else the state becomes an
//! error. Child death is always an error state, never silence. The user's real tunnel helper is never spawned by
//! tests: they generate a `.cmd` stub in a temp directory.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Lines of raw output kept for the "Raw output" expander.
const MAX_LOG: usize = 50;
/// Default wait for the first URL line.
pub const START_TIMEOUT: Duration = Duration::from_secs(30);
/// `CREATE_NO_WINDOW` (winbase.h): the helper is a console program started from a GUI app.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// What the tunnel is doing; drives the Settings status line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum State {
    #[default]
    Off,
    Starting,
    Up {
        url: String,
    },
    Error {
        message: String,
    },
}

impl State {
    /// True while a child process is expected to be alive.
    #[cfg(test)]
    #[must_use]
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Starting | Self::Up { .. })
    }
}

/// One `start` request.
#[derive(Clone, Debug)]
pub struct TunnelConfig {
    /// Configured helper path (`RemoteSettings::tunnel_bin`).
    pub bin: String,
    /// Local port of the remote server.
    pub port: u16,
    /// Named tunnel subdomain; empty = quick tunnel (random host).
    pub subdomain: String,
    /// Zone used when the helper prints no URL at all.
    pub tunnel_domain: String,
    /// How long to wait for a URL before failing.
    pub timeout: Duration,
}

impl TunnelConfig {
    /// Config with the default 30 s URL timeout.
    #[must_use]
    pub fn new(bin: &str, port: u16, subdomain: &str, tunnel_domain: &str) -> Self {
        Self {
            bin: bin.to_owned(),
            port,
            subdomain: subdomain.trim().to_owned(),
            tunnel_domain: tunnel_domain.trim().to_owned(),
            timeout: START_TIMEOUT,
        }
    }

    /// `https://<subdomain>.<tunnel_domain>` when both are set.
    #[must_use]
    fn fallback_url(&self) -> Option<String> {
        let sub = self.subdomain.trim().trim_matches('.');
        let domain = self.tunnel_domain.trim().trim_matches('.');
        (!sub.is_empty() && !domain.is_empty()).then(|| format!("https://{sub}.{domain}"))
    }
}

#[derive(Default)]
struct Shared {
    state: State,
    log: VecDeque<String>,
    child: Option<Child>,
}

/// Handle of the tunnel supervisor; cheap to clone, all clones share one child process.
#[derive(Clone, Default)]
pub struct Tunnel {
    shared: Arc<Mutex<Shared>>,
    /// Set while `stop` runs, so the monitor does not report an intended exit as an error.
    stopping: Arc<AtomicBool>,
}

impl Tunnel {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Current state (`Off` once stopped).
    #[must_use]
    pub fn state(&self) -> State {
        lock(&self.shared).state.clone()
    }

    /// Raw output, oldest first (at most [`MAX_LOG`] lines).
    #[must_use]
    pub fn log(&self) -> Vec<String> {
        lock(&self.shared).log.iter().cloned().collect()
    }

    /// Spawns the helper. `Err` when the binary cannot be started; the state is an error then too.
    ///
    /// # Errors
    ///
    /// The spawn error of the helper process.
    pub fn start(&self, cfg: &TunnelConfig) -> Result<(), String> {
        self.stop();
        let bin = resolve_bin(&cfg.bin);
        let mut command = command_for(&bin, cfg);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let message = format!("cannot start {}: {error}", bin.display());
                self.fail(message.clone());
                return Err(message);
            }
        };
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        {
            let mut shared = lock(&self.shared);
            shared.log.clear();
            shared.state = State::Starting;
            shared.child = Some(child);
        }
        self.stopping.store(false, Ordering::SeqCst);
        if let Some(pipe) = stdout {
            self.reader(pipe);
        }
        if let Some(pipe) = stderr {
            self.reader(pipe);
        }
        self.timeout_thread(cfg.timeout, cfg.fallback_url());
        self.monitor_thread();
        Ok(())
    }

    /// Kills the child tree and returns to `Off`.
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        let child = {
            let mut shared = lock(&self.shared);
            shared.state = State::Off;
            shared.child.take()
        };
        if let Some(mut child) = child {
            kill_tree(&mut child);
            let _ = child.wait();
        }
        self.stopping.store(false, Ordering::SeqCst);
    }

    fn fail(&self, message: String) {
        let mut shared = lock(&self.shared);
        shared.state = State::Error { message };
    }

    /// Drains one output pipe; a `https://…` token while still starting becomes the public URL.
    fn reader(&self, pipe: impl Read + Send + 'static) {
        let shared = Arc::clone(&self.shared);
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                let mut inner = lock(&shared);
                if inner.log.len() >= MAX_LOG {
                    inner.log.pop_front();
                }
                inner.log.push_back(line.clone());
                if inner.state == State::Starting {
                    if let Some(url) = first_url(&line) {
                        inner.state = State::Up { url };
                    }
                }
            }
        });
    }

    /// Fails an unchanged `Starting` state after `timeout`, using the configured fallback URL when present.
    fn timeout_thread(&self, timeout: Duration, fallback: Option<String>) {
        let shared = Arc::clone(&self.shared);
        std::thread::spawn(move || {
            std::thread::sleep(timeout);
            let mut inner = lock(&shared);
            if inner.state != State::Starting {
                return;
            }
            inner.state = match fallback {
                Some(url) => State::Up { url },
                None => State::Error {
                    message: format!(
                        "no public URL from the tunnel within {} s",
                        timeout.as_secs()
                    ),
                },
            };
        });
    }

    /// Turns an unexpected child exit into an error state.
    fn monitor_thread(&self) {
        let shared = Arc::clone(&self.shared);
        let stopping = Arc::clone(&self.stopping);
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(250));
            let mut inner = lock(&shared);
            let Some(child) = inner.child.as_mut() else {
                return;
            };
            let exited = match child.try_wait() {
                Ok(Some(status)) => Some(status.to_string()),
                Ok(None) => None,
                Err(error) => Some(error.to_string()),
            };
            let Some(status) = exited else {
                continue;
            };
            inner.child = None;
            if !stopping.load(Ordering::SeqCst) {
                inner.state = State::Error {
                    message: format!("the tunnel exited ({status})"),
                };
            }
            return;
        });
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            kill_tree(&mut child);
            let _ = child.wait();
        }
    }
}

fn lock(shared: &Arc<Mutex<Shared>>) -> std::sync::MutexGuard<'_, Shared> {
    match shared.lock() {
        Ok(guard) => guard,
        // A panicking reader thread must not disable the page; the state is still readable.
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Configured path → the file actually executed: on Windows `.exe` is appended when the path has no
/// extension and the extensionless path is not a file (`C:\tools\cftunnel` → `C:\tools\cftunnel.exe`).
#[must_use]
pub fn resolve_bin(bin: &str) -> PathBuf {
    let path = PathBuf::from(bin.trim());
    if !cfg!(windows) || path.extension().is_some() || path.is_file() {
        return path;
    }
    let mut name = path.into_os_string();
    name.push(".exe");
    PathBuf::from(name)
}

fn command_for(bin: &PathBuf, cfg: &TunnelConfig) -> Command {
    let mut args: Vec<String> = vec!["http".to_owned(), cfg.port.to_string()];
    if !cfg.subdomain.is_empty() {
        args.push("--name".to_owned());
        args.push(cfg.subdomain.clone());
    }
    // `.cmd` / `.bat` helpers (the test stub, user scripts) need a shell on Windows.
    let ext = bin
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if cfg!(windows) && (ext == "cmd" || ext == "bat") {
        let mut command = Command::new("cmd");
        command.arg("/c").arg(bin);
        command.args(&args);
        return command;
    }
    let mut command = Command::new(bin);
    command.args(&args);
    command
}

/// Kills the helper and every process it started (`taskkill /T /F` on Windows).
fn kill_tree(child: &mut Child) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    let _ = child.kill();
}

/// First `https://…` token of a line, without trailing punctuation the helper may append.
#[must_use]
pub fn first_url(line: &str) -> Option<String> {
    let mut rest = line;
    while let Some(start) = rest.find("https://") {
        let tail = &rest[start..];
        let url: String = tail
            .chars()
            .take_while(|&c| {
                !c.is_whitespace()
                    && !matches!(
                        c,
                        '"' | '\'' | '<' | '>' | ')' | '(' | ']' | '[' | '}' | '{' | ',' | ';'
                    )
            })
            .collect();
        let url = url.trim_end_matches(['.', '!', '?', ':']).to_owned();
        let host = url.strip_prefix("https://").unwrap_or_default();
        if !host.is_empty() && host.contains('.') {
            return Some(url);
        }
        rest = &tail[1..];
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use std::time::Instant;

    fn temp_dir(tag: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "harness-tunnel-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// Writes a `cmd` stub; `body` runs inside `@echo off`.
    /// A fake tunnel binary. `body` is batch; on Unix it becomes an executable `sh` script (`ping -n 30` → `sleep 30`,
    /// `exit /b N` → `exit N`).
    fn stub(dir: &PathBuf, name: &str, body: &str) -> PathBuf {
        #[cfg(windows)]
        {
            let path = dir.join(name);
            std::fs::write(&path, format!("@echo off\r\n{body}\r\n")).expect("write stub");
            path
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = dir.join(name).with_extension("sh");
            let script = body
                .replace("\r\n", "\n")
                .replace("ping -n 30 127.0.0.1 >nul", "sleep 30")
                .replace("exit /b ", "exit ");
            std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).expect("write stub");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod stub");
            path
        }
    }

    fn wait_for(tunnel: &Tunnel, want: impl Fn(&State) -> bool, timeout: Duration) -> State {
        let deadline = Instant::now() + timeout;
        loop {
            let state = tunnel.state();
            if want(&state) || Instant::now() >= deadline {
                return state;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn first_url_takes_the_tunnel_link() {
        assert_eq!(
            first_url("INF +--------------------------------------------------------------------------------------------+"),
            None
        );
        assert_eq!(
            first_url("2024-01-01 INF |  https://harness.trycloudflare.com  |").as_deref(),
            Some("https://harness.trycloudflare.com")
        );
        // Trailing punctuation and quotes are not part of the URL.
        assert_eq!(
            first_url("Visit https://abc-def.trycloudflare.com, then pair.").as_deref(),
            Some("https://abc-def.trycloudflare.com")
        );
        assert_eq!(
            first_url(r#"{"url":"https://sub.example.com"}"#).as_deref(),
            Some("https://sub.example.com")
        );
        // Text without a dotted host is not a tunnel URL.
        assert_eq!(first_url("see https://localhost"), None);
        assert_eq!(first_url("http://plain.example.com"), None);
    }

    #[test]
    fn resolve_bin_appends_exe_only_when_needed() {
        if !cfg!(windows) {
            assert_eq!(
                resolve_bin("/usr/bin/cloudflared"),
                PathBuf::from("/usr/bin/cloudflared")
            );
            return;
        }
        // Extensionless and not a file → `.exe` (e.g. `C:\tools\cftunnel`).
        assert_eq!(
            resolve_bin(r"D:\harness-missing-cftunnel"),
            PathBuf::from(r"D:\harness-missing-cftunnel.exe")
        );
        // Already has an extension → unchanged.
        assert_eq!(
            resolve_bin(r"C:\tools\cftunnel.exe"),
            PathBuf::from(r"C:\tools\cftunnel.exe")
        );
        // An existing extensionless file is used as it is.
        let dir = temp_dir("resolve");
        let plain = dir.join("tunnel");
        std::fs::write(&plain, b"x").expect("write");
        assert_eq!(resolve_bin(&plain.to_string_lossy()), plain);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stub_url_line_goes_up_and_stop_returns_off() {
        let dir = temp_dir("up");
        let path = stub(
            &dir,
            "fake-cftunnel.cmd",
            "echo starting tunnel\r\necho https://stub-sub.trycloudflare.com is up\r\nping -n 30 127.0.0.1 >nul",
        );
        let tunnel = Tunnel::new();
        let cfg = TunnelConfig::new(&path.to_string_lossy(), 7878, "stub-sub", "");
        tunnel.start(&cfg).expect("start");
        let state = wait_for(&tunnel, State::is_running, Duration::from_secs(5));
        assert!(state.is_running(), "{state:?}");
        let state = wait_for(
            &tunnel,
            |s| matches!(s, State::Up { .. }),
            Duration::from_secs(10),
        );
        assert_eq!(
            state,
            State::Up {
                url: "https://stub-sub.trycloudflare.com".into()
            },
            "log: {:?}",
            tunnel.log()
        );
        assert!(tunnel.log().len() <= MAX_LOG);
        tunnel.stop();
        assert_eq!(tunnel.state(), State::Off);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn child_death_is_an_error_state() {
        let dir = temp_dir("death");
        let path = stub(&dir, "dying.cmd", "echo no url here\r\nexit /b 3");
        let tunnel = Tunnel::new();
        tunnel
            .start(&TunnelConfig::new(&path.to_string_lossy(), 7878, "", ""))
            .expect("start");
        let state = wait_for(
            &tunnel,
            |s| matches!(s, State::Error { .. }),
            Duration::from_secs(10),
        );
        assert!(matches!(state, State::Error { .. }), "{state:?}");
        tunnel.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_url_times_out_into_the_configured_fallback() {
        let dir = temp_dir("timeout");
        let path = stub(&dir, "silent.cmd", "ping -n 30 127.0.0.1 >nul");
        let tunnel = Tunnel::new();
        let mut cfg = TunnelConfig::new(&path.to_string_lossy(), 7878, "harness", "");
        cfg.timeout = Duration::from_millis(600);
        tunnel.start(&cfg).expect("start");
        let state = wait_for(
            &tunnel,
            |s| !matches!(s, State::Starting),
            Duration::from_secs(10),
        );
        // No zone configured: that is an error, not a silent `Starting` forever.
        assert!(matches!(state, State::Error { .. }), "{state:?}");
        tunnel.stop();

        let path = stub(&dir, "silent2.cmd", "ping -n 30 127.0.0.1 >nul");
        let mut cfg = TunnelConfig::new(&path.to_string_lossy(), 7878, "harness", "example.com");
        cfg.timeout = Duration::from_millis(600);
        tunnel.start(&cfg).expect("start");
        let state = wait_for(
            &tunnel,
            |s| !matches!(s, State::Starting),
            Duration::from_secs(10),
        );
        assert_eq!(
            state,
            State::Up {
                url: "https://harness.example.com".into()
            }
        );
        tunnel.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_binary_reports_an_error() {
        let tunnel = Tunnel::new();
        let missing = std::env::temp_dir().join("harness-no-such-tunnel-binary");
        let started = tunnel.start(&TunnelConfig::new(&missing.to_string_lossy(), 7878, "", ""));
        assert!(started.is_err());
        assert!(matches!(tunnel.state(), State::Error { .. }));
    }
}
