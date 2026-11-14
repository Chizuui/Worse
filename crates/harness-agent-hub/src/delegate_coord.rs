//! Subagent coordination state (phase-9-parity.md §9.8.1): notes to the parent, finished-turn report marks and
//! advisory path claims. Owned by [`crate::delegate::Registry`]; written only by `delegate_server` handlers and
//! the `delegate::track` task.

use std::collections::HashMap;
use std::sync::Mutex;

use harness_core_api::{AgentId, SessionId, TurnId};
use tokio::sync::Notify;

/// Longest note a subagent can leave for its parent.
pub(crate) const MAX_NOTE_CHARS: usize = 4_000;

#[derive(Debug, Clone)]
pub(crate) struct Note {
    pub(crate) child: SessionId,
    pub(crate) text: String,
    pub(crate) at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mark {
    Pending,
    Reported,
}

#[derive(Debug, Clone)]
struct TurnMark {
    parent: SessionId,
    child: SessionId,
    mark: Mark,
}

/// One advisory claim on a workspace-relative path.
#[derive(Debug, Clone)]
pub(crate) struct Claim {
    pub(crate) workspace: String,
    pub(crate) path: String,
    pub(crate) holder: SessionId,
    pub(crate) title: String,
    pub(crate) agent: AgentId,
    pub(crate) note: String,
}

/// Outcome of one `claim_paths` call.
#[derive(Debug, Default)]
pub(crate) struct ClaimOutcome {
    pub(crate) granted: Vec<String>,
    /// (requested path, conflicting claim)
    pub(crate) conflicts: Vec<(String, Claim)>,
    pub(crate) all: Vec<Claim>,
}

#[derive(Default)]
pub(crate) struct Coordination {
    notes: Mutex<HashMap<SessionId, Vec<Note>>>,
    turns: Mutex<HashMap<TurnId, TurnMark>>,
    claims: Mutex<Vec<Claim>>,
    /// Woken (all waiters) whenever a note or a pending finished turn appears.
    pub(crate) changed: Notify,
}

impl Coordination {
    pub(crate) fn add_note(&self, parent: &SessionId, note: Note) {
        if let Ok(mut notes) = self.notes.lock() {
            notes.entry(parent.clone()).or_default().push(note);
        }
        self.changed.notify_waiters();
    }

    /// A child turn ended (from `track`). Keeps an earlier `Reported` mark (blocking `delegate` already
    /// returned that answer).
    pub(crate) fn turn_finished(&self, parent: &SessionId, child: &SessionId, turn: &TurnId) {
        if let Ok(mut turns) = self.turns.lock() {
            turns.entry(turn.clone()).or_insert_with(|| TurnMark {
                parent: parent.clone(),
                child: child.clone(),
                mark: Mark::Pending,
            });
        }
        self.changed.notify_waiters();
    }

    /// A blocking `delegate` returned this turn's answer itself.
    pub(crate) fn mark_reported(&self, parent: &SessionId, child: &SessionId, turn: &TurnId) {
        if let Ok(mut turns) = self.turns.lock() {
            turns.insert(
                turn.clone(),
                TurnMark {
                    parent: parent.clone(),
                    child: child.clone(),
                    mark: Mark::Reported,
                },
            );
        }
    }

    /// Whether `parent` has a finished subagent turn that was not reported yet.
    pub(crate) fn has_pending(&self, parent: &SessionId) -> bool {
        self.turns.lock().is_ok_and(|turns| {
            turns
                .values()
                .any(|m| m.mark == Mark::Pending && &m.parent == parent)
        })
    }

    /// Takes the unreported finished turns and unread notes of `parent` (optionally only from `only`), marking
    /// them reported / read.
    pub(crate) fn take_ready(
        &self,
        parent: &SessionId,
        only: Option<&[SessionId]>,
    ) -> (Vec<(SessionId, TurnId)>, Vec<Note>) {
        let wanted = |child: &SessionId| !matches!(only, Some(o) if !o.contains(child));
        let mut finished = Vec::new();
        if let Ok(mut turns) = self.turns.lock() {
            for (turn, mark) in turns.iter_mut() {
                if mark.mark == Mark::Pending && &mark.parent == parent && wanted(&mark.child) {
                    mark.mark = Mark::Reported;
                    finished.push((mark.child.clone(), turn.clone()));
                }
            }
        }
        let mut notes = Vec::new();
        if let Ok(mut all) = self.notes.lock() {
            if let Some(list) = all.get_mut(parent) {
                let (take, keep): (Vec<Note>, Vec<Note>) =
                    list.drain(..).partition(|n| wanted(&n.child));
                *list = keep;
                notes = take;
            }
        }
        notes.sort_by_key(|n| n.at);
        (finished, notes)
    }

    /// Grants every requested path that does not overlap another session's claim in the same workspace.
    pub(crate) fn claim(&self, request: &Claim, paths: &[String]) -> ClaimOutcome {
        let mut outcome = ClaimOutcome::default();
        let Ok(mut claims) = self.claims.lock() else {
            return outcome;
        };
        for path in paths {
            let conflict = claims.iter().find(|c| {
                c.workspace == request.workspace
                    && c.holder != request.holder
                    && overlaps(&c.path, path)
            });
            if let Some(held) = conflict {
                outcome.conflicts.push((path.clone(), held.clone()));
                continue;
            }
            let mine = claims.iter().any(|c| {
                c.workspace == request.workspace && c.holder == request.holder && &c.path == path
            });
            if !mine {
                let mut claim = request.clone();
                claim.path.clone_from(path);
                claims.push(claim);
            }
            outcome.granted.push(path.clone());
        }
        outcome.all = claims
            .iter()
            .filter(|c| c.workspace == request.workspace)
            .cloned()
            .collect();
        outcome
    }

    /// Releases `paths` of `holder` (all of them when `paths` is empty); returns how many were released.
    pub(crate) fn release(&self, holder: &SessionId, paths: &[String]) -> usize {
        let Ok(mut claims) = self.claims.lock() else {
            return 0;
        };
        let before = claims.len();
        claims.retain(|c| !(&c.holder == holder && (paths.is_empty() || paths.contains(&c.path))));
        before - claims.len()
    }

    pub(crate) fn claims_in(&self, workspace: &str) -> Vec<Claim> {
        self.claims
            .lock()
            .map(|c| {
                c.iter()
                    .filter(|c| c.workspace == workspace)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Equal, or one lies inside the other (whole path components).
fn overlaps(a: &str, b: &str) -> bool {
    a == b
        || a.is_empty()
        || b.is_empty()
        || b.starts_with(&format!("{a}/"))
        || a.starts_with(&format!("{b}/"))
}

/// Workspace-relative claim key (§9.8.1 normalization). `.` or an empty path means the whole workspace.
pub(crate) fn normalize(path: &str) -> Result<String, String> {
    let raw = path.trim().replace('\\', "/");
    if raw.starts_with('/') || raw.chars().nth(1) == Some(':') {
        return Err(format!("{path}: use a path relative to the workspace"));
    }
    let mut parts = Vec::new();
    for part in raw.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err(format!("{path}: `..` is not allowed")),
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    Ok(if cfg!(windows) {
        joined.to_lowercase()
    } else {
        joined
    })
}
