//! Subagent coordination (phase-9-parity.md §9.8.1): background `delegate`, `subagent_wait`,
//! `subagent_message`, the child role's `notify_parent` and advisory `claim_paths`.

use harness_core_api::SessionId;
use serde_json::{json, Value};

use super::phase9_delegate::{delegate_env, is_error, text, Client, Mcp};
use super::{check, step, Env, Gate, A, B, R};

#[allow(clippy::too_many_lines)] // 一个连续场景，拆开反而难读。
pub(super) async fn run(gate: &Gate, env: &Env, parent_mcp: &Client, parent: &SessionId) -> R<()> {
    // 1. 后台委派：立即返回，答案由 subagent_wait 取回且只汇报一次。
    let started = parent_mcp
        .call(
            "delegate",
            json!({"agent": B, "title": "Bg child", "task": "[[say BG-ANSWER]]", "background": true}),
        )
        .await?;
    let body = text(&started);
    check(
        !is_error(&started) && body.contains("started in the background"),
        format!("background delegate: {started}"),
    )?;
    let child = SessionId::new(
        body.split_whitespace()
            .nth(1)
            .ok_or("no session id in the background result")?,
    );
    let first = parent_mcp
        .call(
            "subagent_wait",
            json!({"session_ids": [child.as_str()], "timeout_s": 60}),
        )
        .await?;
    check(
        !is_error(&first) && text(&first).contains("BG-ANSWER"),
        format!("subagent_wait answer: {first}"),
    )?;
    let again = parent_mcp
        .call("subagent_wait", json!({"timeout_s": 0}))
        .await?;
    check(
        text(&again).contains("No new answers") && !text(&again).contains("BG-ANSWER"),
        format!("answer reported twice: {again}"),
    )?;

    // 2. subagent_message 排队下一轮，subagent_wait 取回回复。
    let sent = parent_mcp
        .call(
            "subagent_message",
            json!({"session_id": child.as_str(), "text": "[[say MSG-TWO]]"}),
        )
        .await?;
    check(!is_error(&sent), format!("subagent_message: {sent}"))?;
    let reply = parent_mcp
        .call("subagent_wait", json!({"timeout_s": 60}))
        .await?;
    check(
        text(&reply).contains("MSG-TWO"),
        format!("message reply: {reply}"),
    )?;
    let foreign = parent_mcp
        .call(
            "subagent_message",
            json!({"session_id": parent.as_str(), "text": "x"}),
        )
        .await?;
    check(
        is_error(&foreign),
        "subagent_message to a non-subagent was accepted",
    )?;

    // 3. child 角色：工具列表、不能委派、notify_parent 唤醒正在等待的 subagent_wait。
    let child_mcp = child_client(env, parent, &child).await?;
    let tools = child_mcp
        .blocking(|m| m.request("tools/list", &json!({})))
        .await?;
    let names: Vec<String> = tools["tools"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|t| t["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    check(
        names == ["notify_parent", "claim_paths", "release_paths"],
        format!("child tools {names:?}"),
    )?;
    let nested = child_mcp
        .call("delegate", json!({"agent": A, "task": "x"}))
        .await?;
    check(is_error(&nested), "a subagent was allowed to delegate")?;
    let waiter = {
        let parent_mcp = parent_mcp.clone();
        tokio::spawn(async move {
            parent_mcp
                .call("subagent_wait", json!({"timeout_s": 60}))
                .await
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let note = child_mcp
        .call("notify_parent", json!({"text": "NOTE-ONE blocked on X"}))
        .await?;
    check(!is_error(&note), format!("notify_parent: {note}"))?;
    let woke = tokio::time::timeout(std::time::Duration::from_secs(20), waiter)
        .await
        .map_err(|_| "subagent_wait was not woken by notify_parent")?
        .map_err(|e| e.to_string())??;
    check(
        text(&woke).contains("NOTE-ONE"),
        format!("woken wait: {woke}"),
    )?;

    // 4. 路径占用：冲突报告持有者，释放后可获取；父会话一轮结束自动释放。
    let mine = parent_mcp
        .call(
            "claim_paths",
            json!({"paths": ["src/a.rs"], "note": "gate"}),
        )
        .await?;
    check(
        !is_error(&mine) && text(&mine).contains("Granted: src/a.rs"),
        format!("parent claim: {mine}"),
    )?;
    let clash = child_mcp
        .call("claim_paths", json!({"paths": ["./src/"]}))
        .await?;
    check(
        is_error(&clash)
            && text(&clash).contains("CONFLICT")
            && text(&clash).contains(parent.as_str()),
        format!("conflicting claim: {clash}"),
    )?;
    let bad = child_mcp
        .call("claim_paths", json!({"paths": ["../outside"]}))
        .await?;
    check(is_error(&bad), "a `..` path was accepted")?;
    parent_mcp.call("release_paths", json!({})).await?;
    let free = child_mcp
        .call("claim_paths", json!({"paths": ["src"]}))
        .await?;
    check(!is_error(&free), format!("claim after release: {free}"))?;
    child_mcp.call("release_paths", json!({})).await?;
    parent_mcp
        .call("claim_paths", json!({"paths": ["docs"]}))
        .await?;
    let turn = gate.send(parent, "turn ends", None).await?;
    gate.completed(parent, &turn).await?;
    // 自动释放由 track 任务异步处理：轮询有限次数等待其生效。
    let mut after = Value::Null;
    for _ in 0..50 {
        after = child_mcp
            .call("claim_paths", json!({"paths": ["docs/readme.md"]}))
            .await?;
        if !is_error(&after) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    check(
        !is_error(&after),
        format!("claim not released at turn end: {after}"),
    )?;
    child_mcp.call("release_paths", json!({})).await?;
    let _ = child_mcp
        .blocking(|m| {
            m.close();
            Ok(Value::Null)
        })
        .await;
    step("delegate-coordination");
    Ok(())
}

/// A `harness-delegate` client in the child role for `child` (same endpoint and token as the parent's).
async fn child_client(env: &Env, parent: &SessionId, child: &SessionId) -> R<Client> {
    let mut vars = delegate_env(env, parent)?;
    vars.retain(|(k, _)| k != "HARNESS_DELEGATE_SESSION" && k != "HARNESS_DELEGATE_ROLE");
    vars.push(("HARNESS_DELEGATE_SESSION".into(), child.as_str().to_owned()));
    vars.push(("HARNESS_DELEGATE_ROLE".into(), "child".into()));
    let exe = env.exe("harness-memory-mcp");
    let mcp = tokio::task::spawn_blocking(move || Mcp::start(&exe, &vars))
        .await
        .map_err(|e| e.to_string())??;
    Ok(Client(std::sync::Arc::new(std::sync::Mutex::new(mcp))))
}
