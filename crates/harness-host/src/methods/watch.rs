//! `watch` service handlers (RPC channel `file-watcher`, contract r2 §9.2).

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};

use harness_watch::{WatchOptions, WatchSignal, WatchSink};

use super::{lock, parse, to_value, Notifier, RequestCtx};
use crate::rpc::RpcError;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubscribeParams {
    path: String,
    recursive: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WatchIdParams {
    watch_id: String,
}

/// `watch.subscribe` → `{watchId}`.
pub(super) fn subscribe(ctx: &RequestCtx<'_>, params: Value) -> Result<Value, RpcError> {
    let params: SubscribeParams = parse(params)?;
    let notifier = Arc::new(Notifier::new(ctx.out.clone()));
    let sink_notifier = Arc::clone(&notifier);
    let sink: WatchSink = Arc::new(move |id: &str, signal| {
        let message = match signal {
            WatchSignal::Change { event, .. } => match to_value(&event) {
                Ok(event) => json!({
                    "method": "$/watch/event",
                    "params": {"watchId": id, "event": event},
                }),
                Err(_) => return,
            },
            WatchSignal::Closed { reason } => json!({
                "method": "$/watch/closed",
                "params": {"watchId": id, "reason": reason.wire()},
            }),
        };
        sink_notifier.send(message);
    });
    let options = WatchOptions {
        recursive: params.recursive.unwrap_or(false),
        // 契约 r2：TS 监听器不过滤任何路径（含 .git、node_modules），Rust 同样不传忽略规则。
        ignore: None,
    };
    let id = ctx
        .services
        .watch
        .watch(&params.path, options, sink)
        .map_err(|e| RpcError::app(e.to_string()))?;
    *lock(ctx.after_response) = Some(Box::new(move || notifier.open()));
    Ok(json!({"watchId": id}))
}

/// `watch.unsubscribe` → `null` (unknown ids too).
pub(super) fn unsubscribe(ctx: &RequestCtx<'_>, params: Value) -> Result<Value, RpcError> {
    let params: WatchIdParams = parse(params)?;
    ctx.services.watch.unwatch(&params.watch_id);
    Ok(Value::Null)
}
