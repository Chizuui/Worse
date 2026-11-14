//! `pty` service handlers (RPC channel `terminal`, contract r2 §9.3).

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};

use harness_pty::{CreateOptions, EnvMap, PtyError, PtyEvent, PtySink, BAD_SIZE_MESSAGE};

use super::{lock, parse, Notifier, RequestCtx};
use crate::rpc::RpcError;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SpawnParams {
    shell: Option<String>,
    args: Option<Vec<String>>,
    cwd: Option<String>,
    env: Option<BTreeMap<String, Option<String>>>,
    cols: Value,
    rows: Value,
    name: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WriteParams {
    session_id: String,
    data: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResizeParams {
    session_id: String,
    cols: Value,
    rows: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionParams {
    session_id: String,
}

/// A positive integer that fits a PTY dimension, else node-pty's error.
fn dimension(value: &Value) -> Result<u16, RpcError> {
    value
        .as_u64()
        .or_else(|| {
            value
                .as_f64()
                .filter(|v| v.fract() == 0.0 && *v >= 1.0)
                .and_then(|v| format!("{v:.0}").parse::<u64>().ok())
        })
        .and_then(|v| u16::try_from(v).ok())
        .filter(|v| *v > 0)
        .ok_or_else(|| RpcError::app(BAD_SIZE_MESSAGE))
}

fn app(error: &PtyError) -> RpcError {
    RpcError::app(error.to_string())
}

/// `pty.spawn` → `{sessionId, pid, shell}`.
pub(super) fn spawn(ctx: &RequestCtx<'_>, params: Value) -> Result<Value, RpcError> {
    let params: SpawnParams = parse(params)?;
    let cols = dimension(&params.cols)?;
    let rows = dimension(&params.rows)?;
    let env = params.env.map(|map| {
        // 契约 r2：env 是完整环境，整体替换 harness-host 自身的环境；值为 null 的键视同未设置。
        EnvMap::from_pairs(
            map.iter()
                .filter_map(|(k, v)| v.as_deref().map(|v| (k.as_str(), v))),
        )
    });
    let notifier = Arc::new(Notifier::new(ctx.out.clone()));
    let sink_notifier = Arc::clone(&notifier);
    let sink: PtySink = Arc::new(move |id: &str, event| {
        let message = match event {
            PtyEvent::Data(data) => json!({
                "method": "$/pty/output",
                "params": {"sessionId": id, "data": data},
            }),
            PtyEvent::Exit(code) => json!({
                "method": "$/pty/exit",
                "params": {"sessionId": id, "exitCode": code},
            }),
        };
        sink_notifier.send(message);
    });
    let created = ctx
        .services
        .pty
        .create(
            CreateOptions {
                cols,
                rows,
                cwd: params.cwd,
                shell: params.shell,
                args: params.args.unwrap_or_default(),
                env,
                name: params.name,
                // harness-host 不为缺席的监听者缓存输出（契约 §9.4），缓存归桥接层。
                attached: true,
                flow_control: false,
            },
            sink,
        )
        .map_err(|e| app(&e))?;
    *lock(ctx.after_response) = Some(Box::new(move || notifier.open()));
    Ok(json!({
        "sessionId": created.id,
        "pid": created.pid.unwrap_or(0),
        "shell": created.shell,
    }))
}

/// `pty.write` → `null`.
pub(super) fn write(ctx: &RequestCtx<'_>, params: Value) -> Result<Value, RpcError> {
    let params: WriteParams = parse(params)?;
    ctx.services
        .pty
        .write(&params.session_id, params.data.as_bytes())
        .map_err(|e| app(&e))?;
    Ok(Value::Null)
}

/// `pty.resize` → `null`.
pub(super) fn resize(ctx: &RequestCtx<'_>, params: Value) -> Result<Value, RpcError> {
    let params: ResizeParams = parse(params)?;
    // 与 TS 一致：未知会话先报 Terminal not found，再校验尺寸。
    if !ctx.services.pty.ids().contains(&params.session_id) {
        return Err(app(&PtyError::NotFound(params.session_id)));
    }
    let cols = dimension(&params.cols)?;
    let rows = dimension(&params.rows)?;
    ctx.services
        .pty
        .resize(&params.session_id, cols, rows)
        .map_err(|e| app(&e))?;
    Ok(Value::Null)
}

/// `pty.kill` → `null` (unknown or exited ids too).
pub(super) fn kill(ctx: &RequestCtx<'_>, params: Value) -> Result<Value, RpcError> {
    let params: SessionParams = parse(params)?;
    ctx.services.pty.kill(&params.session_id);
    Ok(Value::Null)
}
