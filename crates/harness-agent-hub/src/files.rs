//! Undo / reapply of a turn's workspace file changes (phase-9-parity.md §9.2.1 file rules).
//!
//! Single source of file contents: the git trees of the 8.2 turn snapshots. Restoring writes blobs with
//! temp file + rename and deletes paths absent from the target; it never runs `git checkout`/`reset` and never
//! touches the index, HEAD or any path outside the changed list.

use std::path::{Component, Path};

use harness_core_api::{
    ApiError, ApiResult, ChangeDirection, FileAction, FileChangePlan, FileRestoreReport,
    PlannedFile, TurnId,
};

use crate::hub::HubCtx;
use crate::snapshot;
use crate::store;
use crate::util::new_id;

/// The trees a restore goes between.
#[derive(Debug, Clone)]
pub(crate) struct Trees {
    /// State to restore.
    pub target: String,
    /// State the files must still be in to be safe.
    pub expected: String,
}

/// Trees of one turn for `direction`, or the reason none are available.
pub(crate) async fn turn_trees(
    ctx: &HubCtx,
    workspace: &Path,
    turn: &TurnId,
    direction: ChangeDirection,
) -> ApiResult<Result<Trees, String>> {
    let lookup = turn.clone();
    let row = ctx
        .store
        .run(move |c| store::load_snapshot(c, &lookup))
        .await?;
    let Some(row) = row else {
        return Ok(Err("no snapshot was taken for this turn".into()));
    };
    let (Some(before), Some(after)) = (row.before_tree, row.after_tree) else {
        return Ok(Err(row
            .error
            .unwrap_or_else(|| "the turn has no complete snapshot".into())));
    };
    for tree in [&before, &after] {
        if !snapshot::exists(workspace, tree).await {
            return Ok(Err("snapshot no longer in the repository".into()));
        }
    }
    Ok(Ok(match direction {
        ChangeDirection::Undo => Trees {
            target: before,
            expected: after,
        },
        ChangeDirection::Reapply => Trees {
            target: after,
            expected: before,
        },
    }))
}

/// Trees for rewinding from `first` (inclusive) to the end: target = `before` of the first removed turn,
/// expected = `after` of the last removed turn that has one.
pub(crate) async fn rewind_trees(
    ctx: &HubCtx,
    workspace: &Path,
    removed: &[TurnId],
) -> ApiResult<Result<Trees, String>> {
    let ids = removed.to_vec();
    let rows = ctx
        .store
        .run(move |c| {
            ids.iter()
                .map(|t| store::load_snapshot(c, t))
                .collect::<Result<Vec<_>, _>>()
        })
        .await?;
    let Some(before) = rows
        .first()
        .and_then(|r| r.as_ref())
        .and_then(|r| r.before_tree.clone())
    else {
        return Ok(Err("the first removed turn has no snapshot".into()));
    };
    let Some(after) = rows
        .iter()
        .rev()
        .find_map(|r| r.as_ref().and_then(|r| r.after_tree.clone()))
    else {
        return Ok(Err("the removed turns have no after snapshot".into()));
    };
    for tree in [&before, &after] {
        if !snapshot::exists(workspace, tree).await {
            return Ok(Err("snapshot no longer in the repository".into()));
        }
    }
    Ok(Ok(Trees {
        target: before,
        expected: after,
    }))
}

/// Per-path plan between the trees.
pub(crate) async fn plan(workspace: &Path, trees: &Trees) -> ApiResult<Vec<PlannedFile>> {
    Ok(plan_state(workspace, trees).await?.0)
}

/// Per-path plan plus whether every planned file already equals the target (`FileChangePlan.applied`).
async fn plan_state(workspace: &Path, trees: &Trees) -> ApiResult<(Vec<PlannedFile>, bool)> {
    let mut applied = true;
    let paths = snapshot::changed_paths(workspace, &trees.target, &trees.expected)
        .await
        .map_err(ApiError::Internal)?;
    let mut files = Vec::new();
    for path in paths {
        let target = snapshot::blob_of(workspace, &trees.target, &path)
            .await
            .map_err(ApiError::Internal)?;
        let expected = snapshot::blob_of(workspace, &trees.expected, &path)
            .await
            .map_err(ApiError::Internal)?;
        let current = snapshot::hash_file(workspace, &path)
            .await
            .map_err(ApiError::Internal)?;
        applied &= current == target;
        let safe = current == expected;
        let reason = (!safe).then(|| {
            match (&expected, &current) {
                (Some(_), None) => "deleted since the turn",
                (None, Some(_)) => "created since the turn",
                _ => "modified since the turn",
            }
            .to_owned()
        });
        files.push(PlannedFile {
            path,
            action: if target.is_some() {
                FileAction::Restore
            } else {
                FileAction::Delete
            },
            safe,
            reason,
        });
    }
    Ok((files, applied))
}

/// The full plan of one turn.
pub(crate) async fn plan_turn(
    ctx: &HubCtx,
    workspace: &Path,
    turn: &TurnId,
    direction: ChangeDirection,
) -> ApiResult<FileChangePlan> {
    match turn_trees(ctx, workspace, turn, direction).await? {
        Ok(trees) => {
            let (files, applied) = plan_state(workspace, &trees).await?;
            Ok(FileChangePlan {
                turn_id: turn.clone(),
                direction,
                available: true,
                reason: None,
                files,
                applied,
            })
        }
        Err(reason) => Ok(FileChangePlan {
            turn_id: turn.clone(),
            direction,
            available: false,
            reason: Some(reason),
            files: Vec::new(),
            applied: false,
        }),
    }
}

/// Applies a plan: writes target blobs / deletes paths. `paths = None` = all; unsafe ones only with
/// `include_unsafe`.
pub(crate) async fn apply(
    workspace: &Path,
    trees: &Trees,
    files: &[PlannedFile],
    paths: Option<&[String]>,
    include_unsafe: bool,
) -> FileRestoreReport {
    let mut report = FileRestoreReport::default();
    for file in files {
        if paths.is_some_and(|p| !p.contains(&file.path)) {
            continue;
        }
        if !file.safe && !include_unsafe {
            report.skipped.push((
                file.path.clone(),
                file.reason.clone().unwrap_or_else(|| "unsafe".into()),
            ));
            continue;
        }
        match restore_one(workspace, trees, file).await {
            Ok(()) => report.applied.push(file.path.clone()),
            Err(error) => report.skipped.push((file.path.clone(), error)),
        }
    }
    report
}

async fn restore_one(workspace: &Path, trees: &Trees, file: &PlannedFile) -> Result<(), String> {
    let relative = Path::new(&file.path);
    if relative
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err("path outside the workspace".into());
    }
    let full = workspace.join(relative);
    match file.action {
        FileAction::Delete => match tokio::fs::remove_file(&full).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        },
        FileAction::Restore => {
            let blob = snapshot::blob_of(workspace, &trees.target, &file.path)
                .await?
                .ok_or("path not in the snapshot")?;
            let bytes = snapshot::read_blob(workspace, &blob).await?;
            if let Some(parent) = full.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            let name = full
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let temp = full.with_file_name(format!(".{name}.{}", new_id("restore")));
            tokio::fs::write(&temp, &bytes)
                .await
                .map_err(|e| e.to_string())?;
            if let Err(error) = tokio::fs::rename(&temp, &full).await {
                let _ = tokio::fs::remove_file(&temp).await;
                return Err(error.to_string());
            }
            Ok(())
        }
    }
}
