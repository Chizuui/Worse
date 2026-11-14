//! Method table: wire method name -> blocking handler.
//!
//! Handlers run on tokio's blocking pool. Their `params` and results are
//! exactly the TS service argument objects and return values (contract §2).

mod fs;
mod git;
mod pty;
mod search;
mod store;
mod watch;

use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use harness_git::GitService;
use harness_search::{CancellationToken, IndexCache};

use crate::rpc::RpcError;

/// Process-wide service state shared by all requests.
#[derive(Default)]
pub struct Services {
    /// Workspace index cache (search service).
    pub index_cache: IndexCache,
    /// Git service (binary probe cache, collapsed-untracked memory).
    pub git: GitService,
    /// File watchers (`watch.*`).
    pub watch: harness_watch::WatchHub,
    /// Terminal sessions (`pty.*`).
    pub pty: harness_pty::PtyManager,
    /// Read-only task-index reads (`store.*`).
    pub store: harness_store::StoreService,
}

impl Services {
    /// Closes every watcher and kills every PTY session without emitting
    /// notifications (contract r2 section 9.4). Blocking.
    pub fn dispose_sessions(&self) {
        self.watch.dispose_all();
        self.pty.dispose_all();
    }
}

/// Runs once, right after the request's response was queued.
pub type AfterResponse = Box<dyn FnOnce() + Send>;

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Per-request context.
pub struct RequestCtx<'a> {
    /// Shared services.
    pub services: &'a Services,
    /// Cancellation for this request.
    pub cancel: CancellationToken,
    /// Sends one `$/progress` value for this request.
    pub progress: &'a (dyn Fn(Value) + Send + Sync),
    /// The FIFO output channel (server notifications of long-lived sessions).
    pub out: &'a UnboundedSender<Value>,
    /// Hook run after the response is queued (opens a session's notification gate).
    pub after_response: &'a Mutex<Option<AfterResponse>>,
}

/// Holds a session's notifications until its creating response was queued,
/// so `watch.subscribe` / `pty.spawn` responses always precede their events
/// (contract r2 section 9.1). Sends happen under the gate lock, so order is kept.
pub(crate) struct Notifier {
    out: UnboundedSender<Value>,
    held: Mutex<Option<Vec<Value>>>,
}

impl Notifier {
    pub(crate) fn new(out: UnboundedSender<Value>) -> Self {
        Self {
            out,
            held: Mutex::new(Some(Vec::new())),
        }
    }

    pub(crate) fn send(&self, message: Value) {
        let mut held = lock(&self.held);
        match held.as_mut() {
            Some(buffer) => buffer.push(message),
            None => {
                let _ = self.out.send(message);
            }
        }
    }

    pub(crate) fn open(&self) {
        let mut held = lock(&self.held);
        if let Some(buffer) = held.take() {
            for message in buffer {
                let _ = self.out.send(message);
            }
        }
    }
}

/// Per-session FIFO key (contract r2 change 3): requests naming the same
/// `watchId` / `sessionId` run in arrival order.
#[must_use]
pub fn session_key(method: &str, params: &Value) -> Option<String> {
    let field = match method.split_once('.')? {
        ("watch", _) => "watchId",
        ("pty", _) => "sessionId",
        _ => return None,
    };
    let id = params.get(field)?.as_str()?;
    Some(format!("{field}:{id}"))
}

/// A blocking method handler.
pub type Handler = fn(&RequestCtx<'_>, Value) -> Result<Value, RpcError>;

/// `(method, service, handler)` rows.
const METHODS: &[(&str, &str, Handler)] = &[
    ("file.readdir", "fs", fs::readdir),
    ("file.stat", "fs", fs::stat),
    ("file.checkFilesExist", "fs", fs::check_files_exist),
    ("file.resolvePath", "fs", fs::resolve_path),
    ("file.readTextFile", "fs", fs::read_text_file),
    ("file.readFileRange", "fs", fs::read_file_range),
    ("file.readMediaPreview", "fs", fs::read_media_preview),
    ("file.readBinaryPreview", "fs", fs::read_binary_preview),
    ("fs.writeFileAtomic", "fs", fs::write_file_atomic),
    ("fs.pathIdentity", "fs", fs::path_identity),
    (
        "file.searchWorkspaceFiles",
        "search",
        search::search_workspace_files,
    ),
    (
        "file.listWorkspaceFilesLength",
        "search",
        search::list_length,
    ),
    ("file.listWorkspaceFilesRange", "search", search::list_range),
    ("search.text", "search", search::text),
    (
        "git.getRepositorySummary",
        "git",
        git::get_repository_summary,
    ),
    (
        "git.getWorkspaceRepositoryInfo",
        "git",
        git::get_workspace_repository_info,
    ),
    ("git.getLocalBranches", "git", git::get_local_branches),
    ("git.getCommitGraph", "git", git::get_commit_graph),
    ("git.getChanges", "git", git::get_changes),
    ("git.getIgnoredPaths", "git", git::get_ignored_paths),
    ("git.getDiff", "git", git::get_diff),
    ("git.getBranchComparison", "git", git::get_branch_comparison),
    ("git.getIdentity", "git", git::get_identity),
    ("git.refresh", "git", git::refresh),
    ("watch.subscribe", "watch", watch::subscribe),
    ("watch.unsubscribe", "watch", watch::unsubscribe),
    ("pty.spawn", "pty", pty::spawn),
    ("pty.write", "pty", pty::write),
    ("pty.resize", "pty", pty::resize),
    ("pty.kill", "pty", pty::kill),
    ("store.readTree", "store", store::read_tree),
    ("store.readProjects", "store", store::read_projects),
    ("store.readGroups", "store", store::read_groups),
];

/// Looks up a handler.
#[must_use]
pub fn lookup(method: &str) -> Option<Handler> {
    METHODS
        .iter()
        .find(|(name, _, _)| *name == method)
        .map(|(_, _, handler)| *handler)
}

/// Every implemented method name (advertised in `initialize`).
#[must_use]
pub fn method_names() -> Vec<&'static str> {
    METHODS.iter().map(|(name, _, _)| *name).collect()
}

/// Services with at least one implemented method.
#[must_use]
pub fn service_names() -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for (_, service, _) in METHODS {
        if !out.contains(service) {
            out.push(service);
        }
    }
    out
}

/// Deserializes `params` into `T`, mapping failures to `-32602`.
pub(crate) fn parse<T: DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    serde_json::from_value(params).map_err(|e| RpcError::invalid_params(e.to_string()))
}

/// Serializes a result, mapping failures to `-32603`.
pub(crate) fn to_value<T: serde::Serialize>(value: &T) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(|e| RpcError::internal(e.to_string()))
}
