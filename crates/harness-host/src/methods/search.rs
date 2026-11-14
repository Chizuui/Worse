//! `search` service handlers (RPC channel `file`, plus Harness-native `search.text`).

use serde::Deserialize;
use serde_json::{json, Value};

use harness_search::{
    search_entries, search_text, TextSearchOptions, WORKSPACE_FILE_SEARCH_DISPLAY_CAP,
};

use super::{parse, to_value, RequestCtx};
use crate::rpc::RpcError;

const INVALID_QUERY: &str = "Invalid workspace file search query or limit";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RootParams {
    root_path: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RangeParams {
    root_path: String,
    offset: f64,
    length: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TextParams {
    root_path: String,
    query: String,
    is_regex: Option<bool>,
    case_sensitive: Option<bool>,
    max_results: Option<f64>,
    max_file_bytes: Option<f64>,
    workspace_identity: Option<String>,
}

/// JS `Math.max(0, Math.trunc(value))` as `usize`.
fn non_negative(value: f64) -> usize {
    if value.is_nan() || value <= 0.0 {
        return 0;
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let v = value.trunc().min(usize::MAX as f64) as usize;
    v
}

#[allow(clippy::needless_pass_by_value)] // 签名由 Handler 函数指针类型固定。
pub(super) fn search_workspace_files(
    ctx: &RequestCtx<'_>,
    params: Value,
) -> Result<Value, RpcError> {
    let root_path = params
        .get("rootPath")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("rootPath must be a string"))?
        .to_owned();
    let Some(query) = params.get("query").and_then(Value::as_str) else {
        return Err(RpcError::app(INVALID_QUERY));
    };
    // TS：limit ?? 1000，非有限数（含非 number 类型）报 AppError。
    #[allow(clippy::cast_precision_loss)]
    let requested = match params.get("limit") {
        None | Some(Value::Null) => WORKSPACE_FILE_SEARCH_DISPLAY_CAP as f64,
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(_) => f64::NAN,
    };
    if !requested.is_finite() {
        return Err(RpcError::app(INVALID_QUERY));
    }
    let limit = non_negative(requested).min(WORKSPACE_FILE_SEARCH_DISPLAY_CAP);
    if limit == 0 {
        return Ok(json!([]));
    }
    let identity = params.get("workspaceIdentity").and_then(Value::as_str);
    let refresh = params.get("refresh").and_then(Value::as_bool) == Some(true);
    let index = ctx
        .services
        .index_cache
        .get(&root_path, identity, refresh, &ctx.cancel)?;
    to_value(&search_entries(&index, &root_path, query, limit))
}

pub(super) fn list_length(ctx: &RequestCtx<'_>, params: Value) -> Result<Value, RpcError> {
    let p: RootParams = parse(params)?;
    let index = ctx
        .services
        .index_cache
        .get(&p.root_path, None, false, &ctx.cancel)?;
    Ok(json!(index.packed_len()))
}

pub(super) fn list_range(ctx: &RequestCtx<'_>, params: Value) -> Result<Value, RpcError> {
    let p: RangeParams = parse(params)?;
    let index = ctx
        .services
        .index_cache
        .get(&p.root_path, None, false, &ctx.cancel)?;
    let slice = index.packed_range(non_negative(p.offset), non_negative(p.length));
    Ok(Value::String(slice))
}

pub(super) fn text(ctx: &RequestCtx<'_>, params: Value) -> Result<Value, RpcError> {
    let p: TextParams = parse(params)?;
    let options = TextSearchOptions {
        query: p.query,
        is_regex: p.is_regex == Some(true),
        case_sensitive: p.case_sensitive == Some(true),
        max_results: p
            .max_results
            .map_or(harness_search::text::DEFAULT_MAX_RESULTS, non_negative),
        max_file_bytes: p
            .max_file_bytes
            .map_or(harness_search::text::DEFAULT_MAX_FILE_BYTES, |v| {
                non_negative(v) as u64
            }),
    };
    let index = ctx.services.index_cache.get(
        &p.root_path,
        p.workspace_identity.as_deref(),
        false,
        &ctx.cancel,
    )?;
    let summary = search_text(
        &index,
        &p.root_path,
        &options,
        &ctx.cancel,
        &mut |matches| {
            (ctx.progress)(json!({ "matches": matches }));
        },
    )?;
    to_value(&summary)
}
