//! Harness terminal service: a port of `ZCode`'s `TerminalService`
//! (`packages/services/src/terminal/terminalService.ts`) on `portable-pty`
//! (`ConPTY` on Windows, a Unix PTY elsewhere). Wire semantics follow
//! `Worse/.work/phase3/contract.md` r2 §9.3/§9.4.
//!
//! ```text
//! create ─ spawn shell ─ contain tree (Job Object / process group)
//!          ├─ reader thread: read ≤64 KiB → streaming UTF-8 decode → emit Data
//!          └─ waiter thread: wait() → close PTY → drain until EOF or 200 ms idle → emit Exit → forget
//! write / resize / kill: any thread; kill silences the session first (no Exit after kill)
//! ```
//!
//! Every `Data` of a session is emitted before its `Exit`; both go through the
//! session's output lock, so the sink sees them in order.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod decode;
mod kill;
mod shell;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};

pub use decode::Utf8Stream;
pub use shell::{
    default_env, is_executable, resolve_cwd, resolve_env, resolve_shell, windows_pty_info, EnvMap,
    WindowsPtyInfo,
};

use kill::TreeGuard;

/// TS `TERMINAL_UNSUBSCRIBED_OUTPUT_MAX_CHARS`.
pub const UNATTACHED_BUFFER_MAX_UNITS: usize = 1_048_576;
/// node-pty's text for non-positive sizes.
pub const BAD_SIZE_MESSAGE: &str = "resizing must be done using positive cols and rows";

/// Tuning for a [`PtyManager`].
#[derive(Debug, Clone, Copy)]
pub struct PtyConfig {
    /// Bound for output buffered while detached (UTF-16 units, like TS).
    pub unattached_buffer_units: usize,
    /// Unacknowledged UTF-16 units at which a flow-controlled reader pauses.
    pub flow_high_water: usize,
    /// Read buffer size (contract: up to 64 KiB per `$/pty/output`).
    pub read_chunk: usize,
    /// After the shell exits, output still flowing is forwarded until EOF or
    /// this long without data (contract: 200 ms).
    pub exit_idle: Duration,
    /// Unix: `SIGHUP` → `SIGKILL` grace on kill (contract: 2 s).
    pub kill_grace: Duration,
}

impl Default for PtyConfig {
    fn default() -> Self {
        Self {
            unattached_buffer_units: UNATTACHED_BUFFER_MAX_UNITS,
            flow_high_water: 4 * 1024 * 1024,
            read_chunk: 64 * 1024,
            exit_idle: Duration::from_millis(200),
            kill_grace: Duration::from_secs(2),
        }
    }
}

/// Options of [`PtyManager::create`].
#[derive(Debug, Clone, Default)]
pub struct CreateOptions {
    /// Columns; must be positive.
    pub cols: u16,
    /// Rows; must be positive.
    pub rows: u16,
    /// Working directory, used verbatim; `None` = home / OS home / `/`.
    pub cwd: Option<String>,
    /// Shell; `None` = the TS default resolution.
    pub shell: Option<String>,
    /// Shell arguments (TS passes none).
    pub args: Vec<String>,
    /// Complete environment (replaces the process env); `None` = [`default_env`].
    pub env: Option<EnvMap>,
    /// Unix `TERM` name (node-pty `name`); ignored on Windows.
    pub name: Option<String>,
    /// Stream immediately (`true`, the harness-host mode) or buffer until
    /// [`PtyManager::attach`] like the TS emitter.
    pub attached: bool,
    /// Enable ack-based flow control ([`PtyManager::ack`]).
    pub flow_control: bool,
}

/// Result of [`PtyManager::create`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    /// Session id.
    pub id: String,
    /// Shell as used.
    pub shell: String,
    /// Working directory as used.
    pub cwd: String,
    /// Shell process id.
    pub pid: Option<u32>,
    /// TS `windowsPty`.
    pub windows_pty: Option<WindowsPtyInfo>,
}

/// Events delivered to a session's sink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PtyEvent {
    /// Decoded output (never empty).
    Data(String),
    /// The shell exited with this code. Last event of the session.
    Exit(i64),
}

/// Receives events of one session. Called with the session's output lock
/// held, so it must not block or call back into the manager.
pub type PtySink = Arc<dyn Fn(&str, PtyEvent) + Send + Sync>;

/// Errors with TS message texts.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PtyError {
    /// `Terminal not found: <id>`.
    #[error("Terminal not found: {0}")]
    NotFound(String),
    /// Any other TS `Error` text.
    #[error("{0}")]
    App(String),
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

#[derive(Default)]
struct OutState {
    attached: bool,
    pending: VecDeque<PtyEvent>,
    pending_units: usize,
    flow_control: bool,
    unacked: usize,
    silenced: bool,
}

struct Session {
    id: String,
    sink: PtySink,
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    tree: Mutex<TreeGuard>,
    out: Mutex<OutState>,
    flow: Condvar,
    exited: Mutex<bool>,
    exited_cv: Condvar,
}

impl Session {
    /// Emits or buffers one event, in order, under the output lock.
    fn emit(&self, config: &PtyConfig, event: PtyEvent) {
        let mut out = lock(&self.out);
        if out.silenced {
            return;
        }
        if out.attached {
            if let PtyEvent::Data(text) = &event {
                if out.flow_control {
                    out.unacked += utf16_len(text);
                }
            }
            (self.sink)(&self.id, event);
            return;
        }
        if let PtyEvent::Data(text) = &event {
            out.pending_units += utf16_len(text);
        }
        out.pending.push_back(event);
        // 与 TS 一致：无订阅者时有界缓存，超限丢弃最旧的输出块（保留至少一块）。
        while out.pending_units > config.unattached_buffer_units && out.pending.len() > 1 {
            match out.pending.pop_front() {
                Some(PtyEvent::Data(text)) => out.pending_units -= utf16_len(&text),
                Some(exit @ PtyEvent::Exit(_)) => {
                    out.pending.push_front(exit);
                    break;
                }
                None => break,
            }
        }
    }

    /// Blocks the reader while flow control says the consumer is behind.
    fn wait_for_credit(&self, config: &PtyConfig) {
        let mut out = lock(&self.out);
        while out.flow_control
            && out.attached
            && !out.silenced
            && out.unacked >= config.flow_high_water
        {
            out = self.flow.wait(out).unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn silence(&self) {
        let mut out = lock(&self.out);
        out.silenced = true;
        out.pending.clear();
        self.flow.notify_all();
    }

    fn close_pty(&self) {
        let writer = lock(&self.writer).take();
        let master = lock(&self.master).take();
        drop(writer);
        drop(master);
    }

    fn mark_exited(&self) {
        *lock(&self.exited) = true;
        self.exited_cv.notify_all();
    }

    /// Waits until the shell was reaped or `deadline` passes.
    fn wait_exited(&self, deadline: Instant) -> bool {
        let mut exited = lock(&self.exited);
        while !*exited {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            exited = self
                .exited_cv
                .wait_timeout(exited, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }

    /// First kill stage: silence, hang up the tree, close the PTY.
    fn begin_kill(&self) {
        self.silence();
        lock(&self.tree).hang_up();
        if let Err(error) = lock(&self.killer).kill() {
            tracing::debug!(%error, id = self.id, "pty child kill");
        }
        self.close_pty();
    }

    /// Second stage: reap, escalating after the grace period.
    fn finish_kill(&self, deadline: Instant) {
        if !self.wait_exited(deadline) {
            tracing::warn!(id = self.id, "shell ignored hang-up; force kill");
            lock(&self.tree).force();
            self.wait_exited(Instant::now() + Duration::from_secs(1));
        }
        lock(&self.tree).force();
    }
}

struct ManagerInner {
    config: PtyConfig,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    next_id: AtomicU64,
}

/// Owns every terminal session.
pub struct PtyManager {
    inner: Arc<ManagerInner>,
}

impl Default for PtyManager {
    fn default() -> Self {
        Self::new(PtyConfig::default())
    }
}

impl Drop for PtyManager {
    fn drop(&mut self) {
        self.dispose_all();
    }
}

impl PtyManager {
    /// New manager.
    #[must_use]
    pub fn new(config: PtyConfig) -> Self {
        Self {
            inner: Arc::new(ManagerInner {
                config,
                sessions: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(0),
            }),
        }
    }

    /// Spawns a shell on a new PTY.
    ///
    /// # Errors
    /// [`BAD_SIZE_MESSAGE`], the TS no-shell / no-cwd texts, or
    /// `Failed to start terminal with shell '<shell>' in '<cwd>': <reason>`.
    #[allow(clippy::too_many_lines)] // 线性流程：校验 → spawn → 收容进程树 → 启动读/等待线程。
    pub fn create(&self, options: CreateOptions, sink: PtySink) -> Result<Created, PtyError> {
        if options.cols == 0 || options.rows == 0 {
            return Err(PtyError::App(BAD_SIZE_MESSAGE.to_owned()));
        }
        let mut env = options.env.unwrap_or_else(default_env);
        let shell = match options.shell {
            Some(shell) => shell,
            None => resolve_shell(&env).map_err(PtyError::App)?,
        };
        let cwd = match options.cwd {
            Some(cwd) => cwd,
            None => resolve_cwd(None, &env).map_err(PtyError::App)?,
        };
        if cfg!(not(windows)) {
            // node-pty（Unix）把 name 写入子进程的 TERM。
            if let Some(name) = &options.name {
                env.set("TERM", name);
            }
        }
        let fail = |reason: String| {
            PtyError::App(format!(
                "Failed to start terminal with shell '{shell}' in '{cwd}': {reason}"
            ))
        };
        let size = PtySize {
            rows: options.rows,
            cols: options.cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = native_pty_system()
            .openpty(size)
            .map_err(|e| fail(e.to_string()))?;
        let mut command = CommandBuilder::new(&shell);
        command.args(&options.args);
        command.cwd(&cwd);
        command.env_clear();
        for (key, value) in env.iter() {
            command.env(key, value);
        }
        let mut child = pair
            .slave
            .spawn_command(command)
            .map_err(|e| fail(e.to_string()))?;
        let tree = TreeGuard::contain(child.as_ref());
        // 只保留 master：slave 端句柄留在本进程会导致 shell 退出后读端永远等不到 EOF。
        drop(pair.slave);
        let pid = child.process_id();
        let id = self
            .inner
            .next_id
            .fetch_add(1, Ordering::Relaxed)
            .to_string();
        let killer = child.clone_killer();
        let reader = pair.master.try_clone_reader();
        let writer = pair.master.take_writer();
        let (reader, writer) = match (reader, writer) {
            (Ok(reader), Ok(writer)) => (reader, writer),
            (Err(error), _) | (_, Err(error)) => {
                let mut tree = tree;
                tree.force();
                let _ = child.kill();
                let _ = child.wait();
                return Err(fail(error.to_string()));
            }
        };
        let session = Arc::new(Session {
            id: id.clone(),
            sink,
            writer: Mutex::new(Some(writer)),
            master: Mutex::new(Some(pair.master)),
            killer: Mutex::new(killer),
            tree: Mutex::new(tree),
            out: Mutex::new(OutState {
                attached: options.attached,
                flow_control: options.flow_control,
                ..OutState::default()
            }),
            flow: Condvar::new(),
            exited: Mutex::new(false),
            exited_cv: Condvar::new(),
        });
        lock(&self.inner.sessions).insert(id.clone(), Arc::clone(&session));

        let config = self.inner.config;
        let (activity_tx, activity_rx) = mpsc::channel::<ReaderSignal>();
        let reader_session = Arc::clone(&session);
        let spawned_reader = std::thread::Builder::new()
            .name(format!("harness-pty-read-{id}"))
            .spawn(move || read_loop(&reader_session, &config, reader, &activity_tx));
        let waiter_session = Arc::clone(&session);
        let manager = Arc::downgrade(&self.inner);
        let spawned_waiter = std::thread::Builder::new()
            .name(format!("harness-pty-wait-{id}"))
            .spawn(move || {
                let code = match child.wait() {
                    Ok(status) => exit_code(&status),
                    Err(error) => {
                        tracing::warn!(%error, "pty wait failed");
                        -1
                    }
                };
                waiter_session.mark_exited();
                finish_session(&manager, &waiter_session, &config, code, &activity_rx);
            });
        if let Err(error) = spawned_reader.and(spawned_waiter) {
            self.kill(&id);
            return Err(fail(error.to_string()));
        }
        tracing::info!(id, shell, cwd, ?pid, "terminal created");
        Ok(Created {
            id,
            shell,
            cwd,
            pid,
            windows_pty: windows_pty_info(),
        })
    }

    fn get(&self, id: &str) -> Result<Arc<Session>, PtyError> {
        lock(&self.inner.sessions)
            .get(id)
            .cloned()
            .ok_or_else(|| PtyError::NotFound(id.to_owned()))
    }

    /// Writes raw input.
    ///
    /// # Errors
    /// `NotFound`, or the write error text.
    pub fn write(&self, id: &str, data: &[u8]) -> Result<(), PtyError> {
        let session = self.get(id)?;
        let mut writer = lock(&session.writer);
        let Some(writer) = writer.as_mut() else {
            return Err(PtyError::NotFound(id.to_owned()));
        };
        writer
            .write_all(data)
            .and_then(|()| writer.flush())
            .map_err(|e| PtyError::App(e.to_string()))
    }

    /// Resizes the PTY.
    ///
    /// # Errors
    /// `NotFound`, [`BAD_SIZE_MESSAGE`], or the platform error text.
    pub fn resize(&self, id: &str, cols: u16, rows: u16) -> Result<(), PtyError> {
        let session = self.get(id)?;
        if cols == 0 || rows == 0 {
            return Err(PtyError::App(BAD_SIZE_MESSAGE.to_owned()));
        }
        let master = lock(&session.master);
        let Some(master) = master.as_ref() else {
            return Err(PtyError::NotFound(id.to_owned()));
        };
        master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| PtyError::App(e.to_string()))
    }

    /// Starts streaming a detached session: replays buffered output in
    /// order, then goes live.
    ///
    /// # Errors
    /// `NotFound`.
    pub fn attach(&self, id: &str) -> Result<(), PtyError> {
        let session = self.get(id)?;
        let mut out = lock(&session.out);
        if out.attached {
            return Ok(());
        }
        out.attached = true;
        out.pending_units = 0;
        let pending: Vec<PtyEvent> = out.pending.drain(..).collect();
        let mut exited = false;
        for event in pending {
            match &event {
                PtyEvent::Data(text) if out.flow_control => out.unacked += utf16_len(text),
                PtyEvent::Exit(_) => exited = true,
                PtyEvent::Data(_) => {}
            }
            (session.sink)(&session.id, event);
        }
        if exited {
            out.silenced = true;
        }
        session.flow.notify_all();
        drop(out);
        if exited {
            lock(&self.inner.sessions).remove(id);
        }
        Ok(())
    }

    /// Flow control: the consumer processed `units` UTF-16 units.
    ///
    /// # Errors
    /// `NotFound`.
    pub fn ack(&self, id: &str, units: usize) -> Result<(), PtyError> {
        let session = self.get(id)?;
        let mut out = lock(&session.out);
        out.unacked = out.unacked.saturating_sub(units);
        session.flow.notify_all();
        Ok(())
    }

    /// Kills the session's whole process tree and reaps it. No event is
    /// emitted for the session afterwards (TS disposes its emitters). Unknown
    /// ids are a no-op; returns whether the id was active.
    pub fn kill(&self, id: &str) -> bool {
        let Some(session) = lock(&self.inner.sessions).remove(id) else {
            return false;
        };
        session.begin_kill();
        session.finish_kill(Instant::now() + self.inner.config.kill_grace);
        tracing::info!(id, "terminal killed");
        true
    }

    /// Kills every session (host shutdown). The hang-up goes to all trees
    /// first, so the grace period is shared instead of serial.
    pub fn dispose_all(&self) {
        let sessions: Vec<Arc<Session>> =
            lock(&self.inner.sessions).drain().map(|(_, s)| s).collect();
        for session in &sessions {
            session.begin_kill();
        }
        let deadline = Instant::now() + self.inner.config.kill_grace;
        for session in &sessions {
            session.finish_kill(deadline);
        }
    }

    /// Active session ids.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        lock(&self.inner.sessions).keys().cloned().collect()
    }
}

/// Where a sideloaded `conpty.dll` would come from: next to the executable,
/// else the first `PATH` entry holding one (`None` = the OS `ConPTY`).
///
/// portable-pty loads `conpty.dll` by bare name and prefers it over the inbox
/// `ConPTY`; the Windows 10 inbox conhost paces VT output in ~16 ms frames,
/// which is visible as typing latency. Diagnostic only: it mirrors the two
/// search locations the bridge controls, not every `LoadLibrary` rule.
#[must_use]
pub fn conpty_dll() -> Option<std::path::PathBuf> {
    if cfg!(not(windows)) {
        return None;
    }
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("conpty.dll")));
    let on_path = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .map(|dir| dir.join("conpty.dll"));
    beside.into_iter().chain(on_path).find(|dll| dll.is_file())
}

/// node-pty exit codes: Unix `WEXITSTATUS`, `0` when killed by a signal;
/// Windows the unsigned `GetExitCodeProcess` value.
fn exit_code(status: &portable_pty::ExitStatus) -> i64 {
    if status.signal().is_some() {
        0
    } else {
        i64::from(status.exit_code())
    }
}

enum ReaderSignal {
    Data,
    Eof,
}

/// `ConPTY`'s startup cursor query.
const STARTUP_DSR: &str = "\x1b[6n";
/// Cursor at the origin: a fresh terminal, like node-pty (which does not ask
/// `ConPTY` to inherit the cursor).
const STARTUP_DSR_REPLY: &str = "\x1b[1;1R";

enum DsrStep {
    NeedMore,
    Answered(String),
    NotDsr(String),
}

/// portable-pty 固定以 `PSEUDOCONSOLE_INHERIT_CURSOR` 创建 ConPTY，conhost 启动时先发
/// `ESC[6n` 并阻塞全部输出直到收到光标位置回复；node-pty 不带该标志。这里只拦截启动时的
/// 第一个查询并回复 (1,1)，与 node-pty 的新终端语义一致；之后程序发出的查询照常交给客户端。
struct PendingDsr {
    head: String,
}

impl PendingDsr {
    fn new() -> Option<Self> {
        cfg!(windows).then(|| Self {
            head: String::new(),
        })
    }

    fn filter(&mut self, text: &str) -> DsrStep {
        self.head.push_str(text);
        if let Some(rest) = self.head.strip_prefix(STARTUP_DSR) {
            return DsrStep::Answered(rest.to_owned());
        }
        if STARTUP_DSR.starts_with(self.head.as_str()) {
            return DsrStep::NeedMore;
        }
        DsrStep::NotDsr(std::mem::take(&mut self.head))
    }
}

fn read_loop(
    session: &Session,
    config: &PtyConfig,
    mut reader: Box<dyn Read + Send>,
    activity: &mpsc::Sender<ReaderSignal>,
) {
    let mut buf = vec![0u8; config.read_chunk.max(1024)];
    let mut decoder = Utf8Stream::default();
    let mut startup = PendingDsr::new();
    loop {
        session.wait_for_credit(config);
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let mut text = decoder.push(&buf[..n]);
                if let Some(pending) = startup.as_mut() {
                    match pending.filter(&text) {
                        DsrStep::NeedMore => continue,
                        DsrStep::Answered(rest) => {
                            startup = None;
                            if let Some(writer) = lock(&session.writer).as_mut() {
                                let _ = writer
                                    .write_all(STARTUP_DSR_REPLY.as_bytes())
                                    .and_then(|()| writer.flush());
                            }
                            text = rest;
                        }
                        DsrStep::NotDsr(all) => {
                            startup = None;
                            text = all;
                        }
                    }
                }
                if !text.is_empty() {
                    session.emit(config, PtyEvent::Data(text));
                    let _ = activity.send(ReaderSignal::Data);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            // Unix PTY 在所有 slave 关闭后返回 EIO，这是正常的流结束。
            Err(_) => break,
        }
    }
    let tail = decoder.finish();
    if !tail.is_empty() {
        session.emit(config, PtyEvent::Data(tail));
    }
    let _ = activity.send(ReaderSignal::Eof);
}

fn finish_session(
    manager: &Weak<ManagerInner>,
    session: &Arc<Session>,
    config: &PtyConfig,
    code: i64,
    activity: &mpsc::Receiver<ReaderSignal>,
) {
    // Windows ConPTY 在伪控制台关闭前不会给读端 EOF；先关 PTY，再把剩余输出转发到 EOF
    // 或 200ms 无数据为止（后台进程可能一直持有 Unix slave），保证 Exit 排在全部 Data 之后。
    session.close_pty();
    loop {
        match activity.recv_timeout(config.exit_idle) {
            Ok(ReaderSignal::Data) => {}
            Ok(ReaderSignal::Eof) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // 流控暂停时读线程并非空闲，而是在等消费者确认，不能据此提前结束。
                let out = lock(&session.out);
                let paused = out.flow_control
                    && out.attached
                    && !out.silenced
                    && out.unacked >= config.flow_high_water;
                if !paused {
                    break;
                }
            }
        }
    }
    session.emit(config, PtyEvent::Exit(code));
    {
        let mut out = lock(&session.out);
        if !out.attached && !out.silenced {
            // 未订阅时 Exit 留在缓冲区等待 attach 回放，会话保留以便 attach。
            return;
        }
        out.silenced = true;
    }
    // 退出后进程树里可能仍有后台进程：与 kill 同样回收（Windows 关闭 Job，Unix 挂断进程组）。
    lock(&session.tree).hang_up();
    if let Some(manager) = manager.upgrade() {
        let mut sessions = lock(&manager.sessions);
        if sessions
            .get(&session.id)
            .is_some_and(|s| Arc::ptr_eq(s, session))
        {
            sessions.remove(&session.id);
        }
    }
    tracing::info!(id = session.id, code, "terminal exited");
}
