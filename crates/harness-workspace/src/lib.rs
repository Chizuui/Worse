//! Workspace services for one workspace root (spec `docs/specs/harness/phase-8-core.md` §15.3).
//!
//! [`Workspace`] implements [`WorkspaceServices`]. It is the single owner of its terminals (`PtyManager`),
//! watches (`WatchHub`), search index cache and running searches. Every heavy part starts on first use:
//! `Workspace::open` only canonicalizes the root.

mod file_search;
mod git;
mod git_ops;
mod paths;

pub use git_ops::{LOG_DEPTH_MAX, LOG_PAGE_MAX};

pub use file_search::WorkspaceFileSearch;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use harness_core_api::diff::FileDiff;
use harness_core_api::workspace::{
    BranchInfo, CommitResult, FileContent, FileHit, GitStatus, SearchText, TerminalCreate,
    TerminalInfo, TextMatch, TextSearchResult, WorkspaceEventBus, WorkspaceEventKind,
    WorkspaceEventStream, WorkspaceServices, WorkspaceStats,
};
use harness_core_api::{ApiError, ApiResult, BoxFuture};
use harness_pty::{CreateOptions, PtyConfig, PtyEvent, PtyManager, PtySink};
use harness_search::{CancellationToken, EntryType, IndexCache, SearchError, TextSearchOptions};
use harness_watch::{IgnoreRules, WatchOptions, WatchSignal, WatchSink};
use tokio::io::AsyncReadExt;

use crate::paths::Root;

/// Default `read_file` limit.
const DEFAULT_READ_BYTES: u64 = 1024 * 1024;
/// Hard `read_file` limit.
const MAX_READ_BYTES: u64 = 8 * 1024 * 1024;
/// Default `search_text` result cap.
const DEFAULT_MAX_RESULTS: u32 = 2000;

/// Tuning of a workspace.
#[derive(Debug, Clone)]
pub struct WorkspaceConfig {
    /// Watcher debounce (spec: 150 ms).
    pub watch_debounce: Duration,
    /// Watcher max wait (spec: 1 s).
    pub watch_max_wait: Duration,
    /// Environment for terminals; `None` = the process environment.
    pub terminal_env: Option<Vec<(String, String)>>,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            watch_debounce: Duration::from_millis(150),
            watch_max_wait: Duration::from_secs(1),
            terminal_env: None,
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Inner {
    root: Root,
    config: WorkspaceConfig,
    bus: WorkspaceEventBus,
    pty: OnceLock<PtyManager>,
    watch: OnceLock<harness_watch::WatchHub>,
    index: OnceLock<IndexCache>,
    index_built: AtomicBool,
    /// Live terminal ids; output of an id not in here is dropped (no events after `terminal_close`).
    terminals: Mutex<HashSet<String>>,
    watches: Mutex<HashSet<String>>,
    /// Running searches: id → (generation, token). The generation tells a finished search whether the entry
    /// is still its own (a new search with the same id replaces and cancels the old one).
    searches: Mutex<HashMap<String, (u64, CancellationToken)>>,
    search_generation: AtomicU64,
    shut_down: AtomicBool,
    git: harness_git::GitService,
}

/// Services of one workspace root. Cheap to clone (shared state).
#[derive(Clone)]
pub struct Workspace {
    inner: Arc<Inner>,
}

impl Workspace {
    /// Opens `root` (must be an existing directory). No thread, process or index is started.
    ///
    /// # Errors
    ///
    /// `InvalidArgument` when the root is missing or not a directory.
    pub async fn open(root: PathBuf, config: WorkspaceConfig) -> ApiResult<Self> {
        let root = tokio::task::spawn_blocking(move || Root::open(&root))
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))??;
        Ok(Self {
            inner: Arc::new(Inner {
                root,
                config,
                bus: WorkspaceEventBus::new(),
                pty: OnceLock::new(),
                watch: OnceLock::new(),
                index: OnceLock::new(),
                index_built: AtomicBool::new(false),
                terminals: Mutex::new(HashSet::new()),
                watches: Mutex::new(HashSet::new()),
                searches: Mutex::new(HashMap::new()),
                search_generation: AtomicU64::new(0),
                shut_down: AtomicBool::new(false),
                git: harness_git::GitService::new(),
            }),
        })
    }

    fn check_open(&self) -> ApiResult<()> {
        if self.inner.shut_down.load(Ordering::SeqCst) {
            Err(ApiError::ShutDown)
        } else {
            Ok(())
        }
    }

    fn root_str(&self) -> String {
        self.inner.root.as_str().to_owned()
    }

    /// Branch switcher data: current branch (or detached), upstream with ahead / behind, local branches
    /// (phase-9-parity.md §9.2.5 R19).
    ///
    /// # Errors
    ///
    /// `InvalidArgument` outside a repository; git failures.
    pub async fn git_branch_list(&self) -> ApiResult<harness_core_api::workspace::GitBranches> {
        self.check_open()?;
        git_ops::branch_list(self.inner.root.as_str()).await
    }

    /// `git switch [-c] branch`. Local changes that would be overwritten give `SwitchOutcome::Blocked` and
    /// change nothing.
    ///
    /// # Errors
    ///
    /// `InvalidArgument` for a bad name; other git failures (unknown branch, existing name on create).
    pub async fn git_switch(
        &self,
        branch: String,
        create: bool,
    ) -> ApiResult<harness_core_api::workspace::SwitchOutcome> {
        self.check_open()?;
        git_ops::switch(self.inner.root.as_str(), &branch, create).await
    }

    /// Commit; `include_unstaged` first stages every change of the workspace folder (`git add -A -- .`).
    ///
    /// # Errors
    ///
    /// `InvalidArgument` for an empty message or nothing to commit; git failures (hooks).
    pub async fn git_commit_with(
        &self,
        message: String,
        include_unstaged: bool,
    ) -> ApiResult<CommitResult> {
        self.check_open()?;
        git_ops::commit_with(self.inner.root.as_str(), &message, include_unstaged).await
    }

    /// `git push` of `branch` (default: current) to `remote` (default: the upstream's, else `origin`, else the
    /// first remote); sets the upstream when the branch has none. `progress` gets Started, then Finished or Failed.
    ///
    /// # Errors
    ///
    /// `Conflict` when detached or without remote; git failures (rejected, auth: no prompt is shown).
    pub async fn git_push(
        &self,
        remote: Option<String>,
        branch: Option<String>,
        progress: &(dyn Fn(harness_core_api::workspace::PushProgress) + Send + Sync),
    ) -> ApiResult<harness_core_api::workspace::PushResult> {
        self.check_open()?;
        git_ops::push(self.inner.root.as_str(), remote, branch, progress).await
    }

    /// One page of the commit graph (all branches, topological order, lanes from the newest commit).
    ///
    /// # Errors
    ///
    /// Git failures (an empty repository is an empty page).
    pub async fn git_log(
        &self,
        skip: u32,
        limit: u32,
    ) -> ApiResult<harness_core_api::workspace::GitLogPage> {
        self.check_open()?;
        git_ops::log(self.inner.root.as_str(), skip, limit).await
    }

    /// Runs blocking work off the async runtime.
    async fn blocking<T, F>(&self, f: F) -> ApiResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&Inner) -> ApiResult<T> + Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || f(&inner))
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?
    }

    async fn index(&self, refresh: bool) -> ApiResult<Arc<harness_search::WorkspaceIndex>> {
        self.blocking(move |inner| {
            let cache = inner.index.get_or_init(IndexCache::new);
            let index = cache
                .get(
                    inner.root.as_str(),
                    None,
                    refresh,
                    &CancellationToken::new(),
                )
                .map_err(search_error)?;
            inner.index_built.store(true, Ordering::SeqCst);
            Ok(index)
        })
        .await
    }

    fn pty(&self) -> &PtyManager {
        self.inner
            .pty
            .get_or_init(|| PtyManager::new(PtyConfig::default()))
    }

    fn watch_hub(&self) -> &harness_watch::WatchHub {
        self.inner.watch.get_or_init(|| {
            harness_watch::WatchHub::new(harness_watch::HubConfig {
                debounce: self.inner.config.watch_debounce,
                max_wait: self.inner.config.watch_max_wait,
                ..harness_watch::HubConfig::default()
            })
        })
    }

    async fn do_read_file(&self, path: String, max_bytes: u64) -> ApiResult<FileContent> {
        let (absolute, relative) = self.resolve(path).await?;
        let limit = match max_bytes {
            0 => DEFAULT_READ_BYTES,
            n => n.min(MAX_READ_BYTES),
        };
        let file = tokio::fs::File::open(&absolute)
            .await
            .map_err(|e| io_error(&relative, &e))?;
        let metadata = file.metadata().await.map_err(|e| io_error(&relative, &e))?;
        if !metadata.is_file() {
            return Err(ApiError::InvalidArgument(format!("not a file: {relative}")));
        }
        let size = metadata.len();
        let mut bytes = Vec::with_capacity(usize::try_from(size.min(limit)).unwrap_or(0));
        file.take(limit)
            .read_to_end(&mut bytes)
            .await
            .map_err(|e| io_error(&relative, &e))?;
        let binary = harness_fs::is_probably_binary(&bytes[..bytes.len().min(8192)]);
        Ok(FileContent {
            path: relative,
            text: if binary {
                String::new()
            } else {
                String::from_utf8_lossy(&bytes).into_owned()
            },
            size,
            truncated: size > limit,
            binary,
        })
    }

    async fn resolve(&self, path: String) -> ApiResult<(PathBuf, String)> {
        self.blocking(move |inner| inner.root.resolve(&path)).await
    }

    async fn do_search_files(&self, query: String, limit: u32) -> ApiResult<Vec<FileHit>> {
        self.check_open()?;
        let index = self.index(false).await?;
        let root = self.root_str();
        let limit = usize::try_from(limit.clamp(1, 1000)).unwrap_or(1000);
        self.blocking(move |_| {
            Ok(harness_search::search_entries(&index, &root, &query, limit)
                .into_iter()
                .map(|entry| FileHit {
                    name: entry.name,
                    path: entry.relative_path.replace('\\', "/"),
                    is_dir: entry.kind == EntryType::Directory,
                })
                .collect())
        })
        .await
    }

    async fn do_search_text(&self, request: SearchText) -> ApiResult<TextSearchResult> {
        self.check_open()?;
        if request.query.is_empty() {
            return Err(ApiError::InvalidArgument("search query is empty".into()));
        }
        let token = CancellationToken::new();
        let generation = self.inner.search_generation.fetch_add(1, Ordering::SeqCst);
        {
            let mut searches = lock(&self.inner.searches);
            if let Some((_, previous)) =
                searches.insert(request.search_id.clone(), (generation, token.clone()))
            {
                previous.cancel();
            }
        }
        let result = async {
            let index = self.index(false).await?;
            let root = self.root_str();
            let bus = self.inner.bus.clone();
            let max_results = match request.max_results {
                0 => DEFAULT_MAX_RESULTS,
                n => n,
            };
            let options = TextSearchOptions {
                query: request.query.clone(),
                is_regex: request.is_regex,
                case_sensitive: request.case_sensitive,
                max_results: usize::try_from(max_results).unwrap_or(usize::MAX),
                max_file_bytes: 2 * 1024 * 1024,
            };
            let search_id = request.search_id.clone();
            let cancel = token.clone();
            self.blocking(move |_| {
                let mut all: Vec<TextMatch> = Vec::new();
                let mut on_progress = |batch: Vec<harness_search::TextMatch>| {
                    let matches: Vec<TextMatch> = batch.into_iter().map(text_match).collect();
                    all.extend(matches.iter().cloned());
                    bus.emit(&WorkspaceEventKind::SearchProgress {
                        search_id: search_id.clone(),
                        matches,
                    });
                };
                let summary =
                    harness_search::search_text(&index, &root, &options, &cancel, &mut on_progress)
                        .map_err(search_error)?;
                if summary.cancelled || cancel.is_cancelled() {
                    return Err(ApiError::Internal("cancelled".into()));
                }
                Ok(TextSearchResult {
                    matches: all,
                    match_count: summary.match_count as u64,
                    file_count: summary.file_count as u64,
                    truncated: summary.truncated,
                })
            })
            .await
        }
        .await;
        let mut searches = lock(&self.inner.searches);
        if searches
            .get(&request.search_id)
            .is_some_and(|(g, _)| *g == generation)
        {
            searches.remove(&request.search_id);
        }
        result
    }

    async fn do_terminal_create(&self, request: TerminalCreate) -> ApiResult<TerminalInfo> {
        self.check_open()?;
        if request.cols == 0 || request.rows == 0 {
            return Err(ApiError::InvalidArgument(
                "terminal size must be positive".into(),
            ));
        }
        let cwd = match request.cwd {
            Some(cwd) if !cwd.trim().is_empty() => self.resolve(cwd).await?.0,
            _ => PathBuf::from(self.inner.root.as_str()),
        };
        let env = self.inner.config.terminal_env.as_ref().map(|pairs| {
            harness_pty::EnvMap::from_pairs(pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        });
        let options = CreateOptions {
            cols: request.cols,
            rows: request.rows,
            cwd: Some(cwd.to_string_lossy().into_owned()),
            shell: request.shell.filter(|s| !s.trim().is_empty()),
            args: Vec::new(),
            env,
            name: Some("xterm-256color".into()),
            attached: true,
            flow_control: false,
        };
        let inner = Arc::clone(&self.inner);
        let sink: PtySink = Arc::new(move |id: &str, event: PtyEvent| {
            // Called under the session's output lock: only a set lookup and an unbounded send (no blocking).
            let live = lock(&inner.terminals).contains(id);
            if !live {
                return;
            }
            match event {
                PtyEvent::Data(data) => inner.bus.emit(&WorkspaceEventKind::TerminalOutput {
                    id: id.to_owned(),
                    data,
                }),
                PtyEvent::Exit(code) => {
                    lock(&inner.terminals).remove(id);
                    inner.bus.emit(&WorkspaceEventKind::TerminalExited {
                        id: id.to_owned(),
                        code,
                    });
                }
            }
        });
        let this = self.clone();
        // ConPTY start and shell spawn block; keep them off the runtime threads.
        let created = self
            .blocking(move |inner| {
                // The id is unknown until `create` returns, but output may arrive before that: hold the
                // terminals lock across `create` so the sink waits for the id to be registered.
                let mut terminals = lock(&inner.terminals);
                let created = this
                    .pty()
                    .create(options, sink)
                    .map_err(|e| ApiError::Internal(e.to_string()))?;
                terminals.insert(created.id.clone());
                Ok(created)
            })
            .await?;
        Ok(TerminalInfo {
            id: created.id,
            shell: created.shell,
            cwd: created.cwd,
            pid: created.pid,
        })
    }

    fn terminal_known(&self, id: &str) -> ApiResult<&PtyManager> {
        self.check_open()?;
        let pty = self
            .inner
            .pty
            .get()
            .ok_or_else(|| ApiError::NotFound(format!("terminal {id}")))?;
        if !lock(&self.inner.terminals).contains(id) {
            return Err(ApiError::NotFound(format!("terminal {id}")));
        }
        Ok(pty)
    }

    async fn do_watch_start(&self) -> ApiResult<String> {
        self.check_open()?;
        let root = self.root_str();
        let this = self.clone();
        self.blocking(move |inner| {
            let rules = IgnoreRules::build(&root, true, &[".git/".to_owned()])
                .map_err(ApiError::Internal)?;
            let bus = inner.bus.clone();
            let root_for_sink = inner.root.clone();
            let sink: WatchSink = Arc::new(move |id: &str, signal: WatchSignal| match signal {
                WatchSignal::Change { event, rescan } => {
                    let paths = event
                        .changed_path
                        .and_then(|p| root_for_sink.relative(&p))
                        .into_iter()
                        .collect();
                    bus.emit(&WorkspaceEventKind::FilesChanged {
                        watch_id: id.to_owned(),
                        paths,
                        rescan,
                    });
                }
                WatchSignal::Closed { reason } => {
                    bus.emit(&WorkspaceEventKind::WatchClosed {
                        watch_id: id.to_owned(),
                        reason: reason.wire(),
                    });
                }
            });
            let id = this
                .watch_hub()
                .watch(
                    &root,
                    WatchOptions {
                        recursive: true,
                        ignore: Some(rules),
                    },
                    sink,
                )
                .map_err(|e| ApiError::Internal(e.to_string()))?;
            lock(&inner.watches).insert(id.clone());
            Ok(id)
        })
        .await
    }
}

fn io_error(path: &str, error: &std::io::Error) -> ApiError {
    if error.kind() == std::io::ErrorKind::NotFound {
        ApiError::NotFound(path.to_owned())
    } else {
        ApiError::Internal(format!("{path}: {error}"))
    }
}

fn search_error(error: SearchError) -> ApiError {
    match error {
        SearchError::Cancelled => ApiError::Internal("cancelled".into()),
        SearchError::InvalidPattern(text) => ApiError::InvalidArgument(text),
        SearchError::Fs(e) => ApiError::Internal(e.to_string()),
    }
}

fn text_match(m: harness_search::TextMatch) -> TextMatch {
    TextMatch {
        path: m.relative_path.replace('\\', "/"),
        line: m.line,
        column: m.column as u64,
        text: m.text,
    }
}

impl WorkspaceServices for Workspace {
    fn root(&self) -> String {
        self.root_str()
    }

    fn read_file(&self, path: String, max_bytes: u64) -> BoxFuture<'_, ApiResult<FileContent>> {
        Box::pin(async move {
            self.check_open()?;
            self.do_read_file(path, max_bytes).await
        })
    }

    fn open_external(&self, path: String) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(async move {
            self.check_open()?;
            let (absolute, _) = self.resolve(path).await?;
            if !tokio::fs::try_exists(&absolute).await.unwrap_or(false) {
                return Err(ApiError::NotFound(absolute.display().to_string()));
            }
            git::open_with_os(&absolute)
        })
    }

    fn search_files(&self, query: String, limit: u32) -> BoxFuture<'_, ApiResult<Vec<FileHit>>> {
        Box::pin(self.do_search_files(query, limit))
    }

    fn search_text(&self, request: SearchText) -> BoxFuture<'_, ApiResult<TextSearchResult>> {
        Box::pin(self.do_search_text(request))
    }

    fn cancel_search(&self, search_id: String) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(async move {
            if let Some((_, token)) = lock(&self.inner.searches).remove(&search_id) {
                token.cancel();
            }
            Ok(())
        })
    }

    fn git_status(&self) -> BoxFuture<'_, ApiResult<GitStatus>> {
        Box::pin(async move {
            self.check_open()?;
            self.blocking(|inner| git::status(&inner.git, inner.root.as_str()))
                .await
        })
    }

    fn git_file_diff(&self, path: String, staged: bool) -> BoxFuture<'_, ApiResult<FileDiff>> {
        Box::pin(async move {
            self.check_open()?;
            let (_, relative) = self.resolve(path).await?;
            self.blocking(move |inner| {
                git::file_diff(&inner.git, inner.root.as_str(), &relative, staged)
            })
            .await
        })
    }

    fn git_stage(&self, paths: Vec<String>) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(async move {
            self.check_open()?;
            let relative = self.relative_paths(paths).await?;
            git::stage(self.inner.root.as_str(), &relative).await
        })
    }

    fn git_unstage(&self, paths: Vec<String>) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(async move {
            self.check_open()?;
            let relative = self.relative_paths(paths).await?;
            git::unstage(self.inner.root.as_str(), &relative).await
        })
    }

    fn git_commit(&self, message: String) -> BoxFuture<'_, ApiResult<CommitResult>> {
        Box::pin(async move {
            self.check_open()?;
            git::commit(self.inner.root.as_str(), &message).await
        })
    }

    fn git_branches(&self) -> BoxFuture<'_, ApiResult<Vec<BranchInfo>>> {
        Box::pin(async move {
            self.check_open()?;
            self.blocking(|inner| git::branches(&inner.git, inner.root.as_str()))
                .await
        })
    }

    fn terminal_create(&self, request: TerminalCreate) -> BoxFuture<'_, ApiResult<TerminalInfo>> {
        Box::pin(self.do_terminal_create(request))
    }

    fn terminal_write(&self, id: String, data: String) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(async move {
            self.terminal_known(&id)?;
            self.blocking(move |inner| {
                inner
                    .pty
                    .get()
                    .ok_or_else(|| ApiError::NotFound(format!("terminal {id}")))?
                    .write(&id, data.as_bytes())
                    .map_err(|e| ApiError::NotFound(e.to_string()))
            })
            .await
        })
    }

    fn terminal_resize(&self, id: String, cols: u16, rows: u16) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(async move {
            if cols == 0 || rows == 0 {
                return Err(ApiError::InvalidArgument(
                    "terminal size must be positive".into(),
                ));
            }
            self.terminal_known(&id)?
                .resize(&id, cols, rows)
                .map_err(|e| ApiError::NotFound(e.to_string()))
        })
    }

    fn terminal_close(&self, id: String) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(async move {
            self.terminal_known(&id)?;
            // Removed first: the sink drops anything the dying shell still prints (no events after close).
            lock(&self.inner.terminals).remove(&id);
            let pty_id = id.clone();
            let killed = {
                let inner = Arc::clone(&self.inner);
                tokio::task::spawn_blocking(move || {
                    inner.pty.get().is_some_and(|p| p.kill(&pty_id))
                })
                .await
                .map_err(|e| ApiError::Internal(e.to_string()))?
            };
            if killed {
                Ok(())
            } else {
                Err(ApiError::NotFound(format!("terminal {id}")))
            }
        })
    }

    fn watch_start(&self) -> BoxFuture<'_, ApiResult<String>> {
        Box::pin(self.do_watch_start())
    }

    fn watch_stop(&self, watch_id: String) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(async move {
            let removed = lock(&self.inner.watches).remove(&watch_id);
            let stopped = self
                .inner
                .watch
                .get()
                .is_some_and(|hub| hub.unwatch(&watch_id));
            if removed || stopped {
                Ok(())
            } else {
                Err(ApiError::NotFound(format!("watch {watch_id}")))
            }
        })
    }

    fn stats(&self) -> WorkspaceStats {
        WorkspaceStats {
            terminals: u32::try_from(lock(&self.inner.terminals).len()).unwrap_or(u32::MAX),
            watches: u32::try_from(lock(&self.inner.watches).len()).unwrap_or(u32::MAX),
            searches_running: u32::try_from(lock(&self.inner.searches).len()).unwrap_or(u32::MAX),
            index_built: self.inner.index_built.load(Ordering::SeqCst),
            pty_started: self.inner.pty.get().is_some(),
            watcher_started: self.inner.watch.get().is_some(),
        }
    }

    fn subscribe(&self) -> WorkspaceEventStream {
        self.inner.bus.subscribe()
    }

    fn shutdown(&self) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(async move {
            if self.inner.shut_down.swap(true, Ordering::SeqCst) {
                return Ok(());
            }
            for (_, (_, token)) in lock(&self.inner.searches).drain() {
                token.cancel();
            }
            lock(&self.inner.terminals).clear();
            lock(&self.inner.watches).clear();
            let inner = Arc::clone(&self.inner);
            tokio::task::spawn_blocking(move || {
                if let Some(pty) = inner.pty.get() {
                    pty.dispose_all();
                }
                if let Some(hub) = inner.watch.get() {
                    hub.dispose_all();
                }
            })
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?;
            self.inner.bus.close();
            Ok(())
        })
    }
}

impl Workspace {
    async fn relative_paths(&self, paths: Vec<String>) -> ApiResult<Vec<String>> {
        if paths.is_empty() {
            return Err(ApiError::InvalidArgument("no paths given".into()));
        }
        self.blocking(move |inner| {
            paths
                .iter()
                .map(|p| inner.root.resolve(p).map(|(_, rel)| rel))
                .collect()
        })
        .await
    }
}
