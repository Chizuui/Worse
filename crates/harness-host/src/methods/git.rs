//! `git` service handlers (RPC channel `git`, read-only).

use serde::Deserialize;
use serde_json::Value;

use harness_git::{ChangeSource, DiffQuery, GitError};

use super::{parse, to_value, RequestCtx};
use crate::rpc::RpcError;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepoParams {
    workspace_path: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChangesParams {
    workspace_path: String,
    source_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IgnoredParams {
    workspace_path: String,
    paths: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiffParams {
    workspace_path: String,
    path: String,
    staged: Option<bool>,
    source_id: Option<String>,
}

impl From<GitError> for RpcError {
    fn from(error: GitError) -> Self {
        match error {
            GitError::App(message) => Self::app(message),
            GitError::Cancelled => Self::cancelled(),
        }
    }
}

fn workspace(params: Value) -> Result<String, RpcError> {
    Ok(parse::<RepoParams>(params)?.workspace_path)
}

/// JS `typeof v === "number"` value, otherwise `None`.
fn number(params: &Value, key: &str) -> Option<f64> {
    params.get(key).and_then(Value::as_f64)
}

pub(super) fn get_repository_summary(
    ctx: &RequestCtx<'_>,
    params: Value,
) -> Result<Value, RpcError> {
    let path = workspace(params)?;
    to_value(
        &ctx.services
            .git
            .repository_summary(&path, &|| ctx.cancel.is_cancelled())?,
    )
}

pub(super) fn get_workspace_repository_info(
    ctx: &RequestCtx<'_>,
    params: Value,
) -> Result<Value, RpcError> {
    let path = workspace(params)?;
    to_value(
        &ctx.services
            .git
            .workspace_repository_info(&path, &|| ctx.cancel.is_cancelled())?,
    )
}

pub(super) fn get_local_branches(ctx: &RequestCtx<'_>, params: Value) -> Result<Value, RpcError> {
    let path = workspace(params)?;
    to_value(
        &ctx.services
            .git
            .local_branches(&path, &|| ctx.cancel.is_cancelled())?,
    )
}

pub(super) fn get_commit_graph(ctx: &RequestCtx<'_>, params: Value) -> Result<Value, RpcError> {
    let max_count = number(&params, "maxCount");
    let skip = number(&params, "skip");
    let path = workspace(params)?;
    to_value(
        &ctx.services
            .git
            .commit_graph(&path, max_count, skip, &|| ctx.cancel.is_cancelled())?,
    )
}

pub(super) fn get_changes(ctx: &RequestCtx<'_>, params: Value) -> Result<Value, RpcError> {
    let p: ChangesParams = parse(params)?;
    let source = match p.source_id.as_str() {
        "staged" => ChangeSource::Staged,
        "unstaged" => ChangeSource::Unstaged,
        other => {
            return Err(RpcError::invalid_params(format!(
                "unsupported sourceId: {other}"
            )))
        }
    };
    to_value(
        &ctx.services
            .git
            .changes(&p.workspace_path, source, &|| ctx.cancel.is_cancelled())?,
    )
}

pub(super) fn get_ignored_paths(ctx: &RequestCtx<'_>, params: Value) -> Result<Value, RpcError> {
    let p: IgnoredParams = parse(params)?;
    to_value(
        &ctx.services
            .git
            .ignored_paths(&p.workspace_path, &p.paths, &|| ctx.cancel.is_cancelled())?,
    )
}

pub(super) fn get_diff(ctx: &RequestCtx<'_>, params: Value) -> Result<Value, RpcError> {
    let p: DiffParams = parse(params)?;
    let query = DiffQuery {
        workspace_path: p.workspace_path,
        path: p.path,
        staged: p.staged,
        source_id: p.source_id,
    };
    to_value(
        &ctx.services
            .git
            .diff(&query, &|| ctx.cancel.is_cancelled())?,
    )
}

pub(super) fn get_branch_comparison(
    ctx: &RequestCtx<'_>,
    params: Value,
) -> Result<Value, RpcError> {
    let path = workspace(params)?;
    to_value(
        &ctx.services
            .git
            .branch_comparison(&path, &|| ctx.cancel.is_cancelled())?,
    )
}

pub(super) fn get_identity(ctx: &RequestCtx<'_>, params: Value) -> Result<Value, RpcError> {
    let path = workspace(params)?;
    to_value(
        &ctx.services
            .git
            .identity(&path, &|| ctx.cancel.is_cancelled())?,
    )
}

pub(super) fn refresh(ctx: &RequestCtx<'_>, params: Value) -> Result<Value, RpcError> {
    let truthy = |key: &str| params.get(key).is_some_and(js_truthy);
    let include_identity = truthy("includeIdentity");
    let include_branch_comparison = truthy("includeBranchComparison");
    let path = workspace(params)?;
    to_value(&ctx.services.git.refresh(
        &path,
        include_identity,
        include_branch_comparison,
        &|| ctx.cancel.is_cancelled(),
    )?)
}

/// JS truthiness (the TS code tests `params.includeIdentity` with `?:`).
fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}
