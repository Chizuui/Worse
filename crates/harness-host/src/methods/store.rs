//! `store` service handlers: read-only task-index reads (spec phase-3c-store.md).
//!
//! Results are `{"json": "<text>"}` because the key order must match the TS objects exactly and
//! `serde_json::Value` maps are only order-preserving when `preserve_order` happens to be
//! unified into the build; the bridge parses the text (result shape `jsonText`).

use serde_json::{json, Value};

use super::RequestCtx;
use crate::rpc::RpcError;

fn answer(result: Result<String, harness_store::StoreError>) -> Result<Value, RpcError> {
    // 任何 Rust 无法逐字复现 TS 的情况都映射为 -32603，由 bridge 回放到 TS（规格 S3）。
    result
        .map(|text| json!({ "json": text }))
        .map_err(|error| RpcError::internal(error.to_string()))
}

/// `store.readTree` (`project.readTree`).
#[allow(clippy::needless_pass_by_value)] // 签名由 Handler 函数指针类型固定。
pub(super) fn read_tree(ctx: &RequestCtx<'_>, params: Value) -> Result<Value, RpcError> {
    answer(ctx.services.store.read_tree(&params, None))
}

/// `store.readProjects` (`project.readProjects`).
#[allow(clippy::needless_pass_by_value)] // 签名由 Handler 函数指针类型固定。
pub(super) fn read_projects(ctx: &RequestCtx<'_>, params: Value) -> Result<Value, RpcError> {
    answer(ctx.services.store.read_projects(&params, None))
}

/// `store.readGroups` (`task-groups.read`).
pub(super) fn read_groups(ctx: &RequestCtx<'_>, _params: Value) -> Result<Value, RpcError> {
    answer(ctx.services.store.read_groups(None))
}
