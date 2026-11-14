//! `fs` service handlers (RPC channel `file`, plus Harness-native `fs.*`).

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{parse, to_value, RequestCtx};
use crate::rpc::RpcError;

const DEFAULT_MEDIA_PREVIEW_BYTES: u64 = 4 * 1024 * 1024;
const MAX_MEDIA_PREVIEW_BYTES: u64 = 8 * 1024 * 1024;
const BINARY_PREVIEW_BYTES: u64 = 25 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PathParams {
    path: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReaddirParams {
    path: String,
    include_hidden: Option<bool>,
}

#[derive(Deserialize)]
struct PathsParams {
    paths: Vec<String>,
}

#[derive(Deserialize)]
struct WindowParams {
    path: String,
    offset: Option<f64>,
    length: Option<f64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreviewParams {
    path: String,
    max_bytes: Option<f64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WriteAtomicParams {
    path: String,
    content_base64: String,
    mode: Option<u32>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IdentityParams {
    path: String,
    workspace_identity: Option<String>,
}

pub(super) fn readdir(_: &RequestCtx<'_>, params: Value) -> Result<Value, RpcError> {
    let p: ReaddirParams = parse(params)?;
    let entries = harness_fs::readdir(&p.path, p.include_hidden == Some(true))?;
    to_value(&entries)
}

pub(super) fn stat(_: &RequestCtx<'_>, params: Value) -> Result<Value, RpcError> {
    let p: PathParams = parse(params)?;
    to_value(&harness_fs::stat(&p.path)?)
}

pub(super) fn check_files_exist(_: &RequestCtx<'_>, params: Value) -> Result<Value, RpcError> {
    let p: PathsParams = parse(params)?;
    to_value(&harness_fs::check_files_exist(&p.paths)?)
}

pub(super) fn resolve_path(_: &RequestCtx<'_>, params: Value) -> Result<Value, RpcError> {
    let p: PathParams = parse(params)?;
    Ok(Value::String(harness_fs::realpath(&p.path)?))
}

pub(super) fn read_text_file(_: &RequestCtx<'_>, params: Value) -> Result<Value, RpcError> {
    let p: WindowParams = parse(params)?;
    to_value(&harness_fs::read_text(&p.path, p.offset, p.length)?)
}

pub(super) fn read_file_range(_: &RequestCtx<'_>, params: Value) -> Result<Value, RpcError> {
    let p: WindowParams = parse(params)?;
    if p.offset.is_none() || p.length.is_none() {
        return Err(RpcError::invalid_params("offset and length are required"));
    }
    let bytes = harness_fs::read_range(&p.path, p.offset, p.length)?;
    Ok(json!({"base64": STANDARD.encode(bytes)}))
}

pub(super) fn read_media_preview(_: &RequestCtx<'_>, params: Value) -> Result<Value, RpcError> {
    let p: PreviewParams = parse(params)?;
    let max = harness_fs::clamp_js_length(
        p.max_bytes,
        DEFAULT_MEDIA_PREVIEW_BYTES,
        MAX_MEDIA_PREVIEW_BYTES,
    );
    let (bytes, total) = harness_fs::read_bytes_limited(&p.path, max)?;
    Ok(json!({
        "path": p.path,
        "mediaType": harness_fs::media_type_for_path(&p.path),
        "dataBase64": STANDARD.encode(bytes),
        "totalBytes": total,
    }))
}

pub(super) fn read_binary_preview(_: &RequestCtx<'_>, params: Value) -> Result<Value, RpcError> {
    let p: PreviewParams = parse(params)?;
    let max = harness_fs::clamp_js_length(p.max_bytes, BINARY_PREVIEW_BYTES, BINARY_PREVIEW_BYTES);
    let (bytes, total) = harness_fs::read_bytes_limited(&p.path, max)?;
    Ok(json!({
        "path": p.path,
        "dataBase64": STANDARD.encode(bytes),
        "totalBytes": total,
    }))
}

pub(super) fn write_file_atomic(_: &RequestCtx<'_>, params: Value) -> Result<Value, RpcError> {
    let p: WriteAtomicParams = parse(params)?;
    let data = STANDARD
        .decode(p.content_base64.as_bytes())
        .map_err(|e| RpcError::invalid_params(format!("contentBase64: {e}")))?;
    harness_fs::atomic_write(&p.path, &data, p.mode)?;
    Ok(Value::Null)
}

pub(super) fn path_identity(_: &RequestCtx<'_>, params: Value) -> Result<Value, RpcError> {
    let p: IdentityParams = parse(params)?;
    let real_path = harness_fs::realpath(&p.path)?;
    let identity_key = match p.workspace_identity.as_deref().map(str::trim) {
        Some(identity) if !identity.is_empty() => identity.to_owned(),
        _ => real_path.clone(),
    };
    Ok(json!({
        "realPath": real_path,
        "identityKey": identity_key,
        "caseInsensitive": cfg!(any(windows, target_os = "macos")),
    }))
}
