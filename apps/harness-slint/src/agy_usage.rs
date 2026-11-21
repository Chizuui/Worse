//! Antigravity (agy) plan quota for the usage popover: reads agy's live login from the credential store
//! read-only, asks Cloud Code for the project and per-model quota, and groups models by family.
//! The access token only goes into the `Authorization` header; it is never logged or stored.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use harness_agent_hub::agy_cred::{slot_target, AGY_LIVE_TARGET};
use serde_json::{json, Value};

use crate::plan_usage::{parse_rfc3339, resets_in, Fetched};

/// `agy` itself talks to the `daily-` host, not the prod host: the grouped
/// summary (Gemini + Claude & GPT + Argon) comes from there. The prod host
/// is kept as a fallback so a `daily-` outage still shows the `3p` bars
/// instead of nothing.
const BASE_DAILY: &str = "https://daily-cloudcode-pa.googleapis.com/v1internal";
const BASE: &str = "https://cloudcode-pa.googleapis.com/v1internal";

fn base_hosts() -> [&'static str; 2] {
    [BASE_DAILY, BASE]
}
/// The `agy` CLI's version, as captured from its own traffic: the quota
/// server gates the grouped summary on this client identity. With any other
/// User-Agent the summary comes back with only the Claude & GPT (`3p`)
/// group and no Gemini/Argon groups at all, which is exactly what the
/// first build showed (live-verified by a mitmproxy capture of `agy`).
const CLI_VERSION: &str = "1.0.9";

pub(crate) const AGY_ID: &str = "agy";

/// One saved agy account (from the hub's account list).
#[derive(Clone, Debug)]
pub(crate) struct AgySlot {
    pub id: String,
    pub name: String,
    pub active: bool,
}

/// A bar before display: (label, remaining %, reset unix time).
///
/// Remaining, not used: Antigravity's own panel reports "Weekly Limit
/// Remaining" / "Five Hour Limit Remaining", so Worse shows the same number
/// users compare against. A bar almost full means quota left; 0% remaining
/// means the limit is hit. (Claude Code rows elsewhere in the popover still
/// show utilization, because Anthropic's API returns utilization.)
type RawBar = (String, f32, Option<i64>);

/// Last successful quota per account id: (plan, bars, fetched at). Only numbers, no tokens.
type Snapshot = (String, Vec<RawBar>, i64);

static LAST: Mutex<Option<HashMap<String, Snapshot>>> = Mutex::new(None);

fn user_agent() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    // The Antigravity CLI's own User-Agent: `antigravity/cli/<version>
    // <os>/<arch>`. The quota server only returns the full grouped summary
    // (Gemini, Claude & GPT, Argon) to this client identity.
    format!("antigravity/cli/{CLI_VERSION} {os}/{arch}")
}

fn row(id: &str, label: &str, error: &str) -> Fetched {
    Fetched {
        id: id.into(),
        label: label.into(),
        plan: String::new(),
        error: error.to_owned(),
        five: None,
        week: None,
        opus: None,
        labels: Default::default(),
        summary: String::new(),
        bars: Vec::new(),
    }
}

fn read_token(target: &str) -> Result<Option<String>, String> {
    let store = crate::agy_cred::shared();
    if !store.0.available() {
        return Ok(None);
    }
    let Some(blob) = store.0.read(target)? else {
        return Err("Not signed in".into());
    };
    let json: Value = serde_json::from_slice(&blob.secret).map_err(|_| "Cannot read login")?;
    drop(blob);
    let token = &json["token"];
    let expired = token["expiry"]
        .as_str()
        .and_then(parse_rfc3339)
        .is_some_and(|at| at <= now_secs());
    if expired {
        return Err("Login expired; run agy once to refresh it".into());
    }
    token["access_token"]
        .as_str()
        .filter(|t| !t.is_empty())
        .map(|t| Some(t.to_owned()))
        .ok_or_else(|| "Not signed in".into())
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

async fn post(
    client: &reqwest::Client,
    token: &str,
    method: &str,
    body: Value,
) -> Result<(Value, &'static str), String> {
    let mut last_error = "Network error".to_owned();
    for base in base_hosts() {
        let response = client
            .post(format!("{base}:{method}"))
            .bearer_auth(token)
            .header("user-agent", user_agent())
            .json(&body)
            .timeout(Duration::from_secs(30))
            .send()
            .await;
        let response = match response {
            Ok(r) => r,
            Err(_) => continue,
        };
        let status = response.status();
        if !status.is_success() {
            last_error = if status.as_u16() == 401 {
                "Login expired; run agy once to refresh it".to_owned()
            } else {
                format!("Usage unavailable (HTTP {})", status.as_u16())
            };
            continue;
        }
        let value = response
            .json::<Value>()
            .await
            .map_err(|_| "Usage unavailable".to_owned())?;
        return Ok((value, base));
    }
    Err(last_error)
}

/// One row per saved agy account (the active one from agy's live login, the others from their Harness copies);
/// without saved accounts one row for the live login. Empty when the credential store does not exist here.
pub(crate) async fn fetch(client: &reqwest::Client, slots: &[AgySlot]) -> Vec<Fetched> {
    if !crate::agy_cred::shared().0.available() {
        return Vec::new();
    }
    if slots.is_empty() {
        return vec![fetch_one(client, AGY_LIVE_TARGET, AGY_ID, "Antigravity").await];
    }
    let mut out = Vec::new();
    for slot in slots {
        let target = if slot.active {
            AGY_LIVE_TARGET.to_owned()
        } else {
            slot_target(&slot.id)
        };
        let id = format!("{AGY_ID}:{}", slot.id);
        let label = format!("Antigravity · {}", slot.name);
        out.push(fetch_one(client, &target, &id, &label).await);
    }
    out
}

async fn fetch_one(client: &reqwest::Client, target: &str, id: &str, label: &str) -> Fetched {
    match quota(client, target).await {
        Ok((plan, bars)) => {
            if let Ok(mut last) = LAST.lock() {
                last.get_or_insert_with(HashMap::new)
                    .insert(id.to_owned(), (plan.clone(), bars.clone(), now_secs()));
            }
            shown(id, label, plan, &bars, None)
        }
        // 修复：未使用的账号令牌约 1 小时后过期；显示该账号最后一次读到的额度，而不是只显示错误。
        Err(error) => {
            let cached = LAST
                .lock()
                .ok()
                .and_then(|last| last.as_ref()?.get(id).cloned());
            if let Some((plan, bars, at)) = cached {
                shown(id, label, plan, &bars, Some(at))
            } else {
                let mut out = row(id, label, &error);
                if error.starts_with("Login expired") && target != AGY_LIVE_TARGET {
                    out.error = "Login expired; switch to this account once to refresh it".into();
                }
                out
            }
        }
    }
}

/// Display row; `as_of` marks numbers remembered from an earlier check. A passed reset reads as full again.
fn shown(id: &str, label: &str, plan: String, bars: &[RawBar], as_of: Option<i64>) -> Fetched {
    let now = now_secs();
    let mut out = row(id, label, "");
    out.plan = plan;
    out.bars = bars
        .iter()
        .map(|(name, remaining, reset)| match reset {
            Some(at) if *at <= now => (name.clone(), 100.0, String::new()),
            Some(at) => (name.clone(), *remaining, resets_in(*at)),
            None => (name.clone(), *remaining, String::new()),
        })
        .collect();
    // Lowest remaining first: the most constrained bucket is the summary.
    let mut top: Vec<&(String, f32, String)> = out.bars.iter().collect();
    top.sort_by(|a, b| a.1.total_cmp(&b.1));
    out.summary = top
        .iter()
        .take(2)
        .map(|(name, remaining, _)| format!("{name} {remaining:.0}% left"))
        .collect::<Vec<_>>()
        .join(" · ");
    if let Some(at) = as_of {
        let minutes = (now - at).max(0) / 60;
        let ago = if minutes < 60 {
            format!("{minutes}m")
        } else {
            format!("{}h", minutes / 60)
        };
        out.summary = format!("{} (as of {ago} ago)", out.summary);
    }
    if out.bars.is_empty() {
        out.error = "Usage unavailable".into();
    }
    out
}

/// Plan name and bars of the login in `target`.
async fn quota(client: &reqwest::Client, target: &str) -> Result<(String, Vec<RawBar>), String> {
    let token = read_token(target)?.ok_or_else(|| "Not signed in".to_owned())?;
    let (load, _) = post(
        client,
        &token,
        "loadCodeAssist",
        json!({"metadata": {"ideType": "ANTIGRAVITY", "platform": "PLATFORM_UNSPECIFIED", "pluginType": "GEMINI"}}),
    )
    .await?;
    let plan = ["paidTier", "currentTier"]
        .iter()
        .find_map(|k| load[*k]["name"].as_str().filter(|n| !n.is_empty()))
        .unwrap_or_default()
        .to_owned();
    let project = match &load["cloudaicompanionProject"] {
        Value::String(s) => s.clone(),
        v => v["id"].as_str().unwrap_or_default().to_owned(),
    };
    let (summary, summary_host) = post(
        client,
        &token,
        "retrieveUserQuotaSummary",
        json!({"project": project}),
    )
    .await
    .map(|(v, h)| (Some(v), h))
    .unwrap_or((None, BASE));
    let (mut bars, skipped) = summary.as_ref().map(summary_bars).unwrap_or_default();
    // Fallback: `fetchAvailableModels` only carries one remaining fraction per
    // model, with no window attached, so it can never invent a weekly or a
    // 5-hour bar. Fill in only the families the summary did not report, one
    // honestly labelled "quota" bar each (worst model of the family).
    let families = ["Gemini", "Claude", "GPT"];
    // "Gemini · " (with separator) so a "Gemini 4 Argon" bar does not count
    // as the Gemini Models family being present.
    let prefixes = ["Gemini · ", "Claude", "GPT"];
    // The summary's "Claude & GPT" group already covers the GPT-OSS models,
    // so a GPT fallback bar next to it would double-count the same quota.
    let covered = |bars: &[(String, f32, Option<i64>)], slot: usize| match slot {
        2 => bars
            .iter()
            .any(|(label, _, _)| label.starts_with("GPT") || label.starts_with("Claude & GPT")),
        _ => bars
            .iter()
            .any(|(label, _, _)| label.starts_with(prefixes[slot])),
    };
    let missing = (0..3).filter(|slot| !covered(&bars, *slot)).count();
    let mut fallback_used = false;
    let mut models_seen: Option<Value> = None;
    if missing > 0 {
        if let Ok((models, _)) = post(
            client,
            &token,
            "fetchAvailableModels",
            json!({"project": project}),
        )
        .await
        {
            fallback_used = true;
            for (slot, value) in group(&models["models"]).into_iter().enumerate() {
                let Some((remaining, reset)) = value else {
                    continue;
                };
                if covered(&bars, slot) {
                    continue;
                }
                bars.push((
                    format!("{} · quota remaining", families[slot]),
                    remaining,
                    reset,
                ));
            }
            models_seen = Some(models);
        }
    }
    write_diagnostic(
        &plan,
        summary.as_ref(),
        &skipped,
        fallback_used,
        &bars,
        models_seen.as_ref(),
        summary_host,
    );
    Ok((plan, bars))
}

/// Top-level shape of a quota-summary response: key -> value kind, so a
/// second group array hiding under another key (e.g. `geminiGroups`) shows
/// up instead of silently never being parsed. No values, only shapes.
fn summary_shape(summary: Option<&Value>) -> Value {
    match summary {
        Some(Value::Object(map)) => {
            let mut shape = serde_json::Map::new();
            for (k, v) in map {
                let desc = match v {
                    Value::Array(a) => format!("array[{}]", a.len()),
                    Value::Object(o) => format!("object[{} keys]", o.len()),
                    Value::String(_) => "string".to_owned(),
                    Value::Number(_) => "number".to_owned(),
                    Value::Bool(_) => "bool".to_owned(),
                    Value::Null => "null".to_owned(),
                };
                shape.insert(k.clone(), Value::String(desc));
            }
            Value::Object(shape)
        }
        Some(_) => Value::String("non-object".to_owned()),
        None => Value::Null,
    }
}

/// Per-model quota from `fetchAvailableModels`, redacted to id + fraction +
/// reset only: shows whether the Gemini models really report 100%, or the
/// metered ones are skipped (e.g. ids that do not start with `gemini`).
fn models_quota(models: Option<&Value>) -> Vec<Value> {
    let Some(models) = models else {
        return Vec::new();
    };
    let entries: Vec<(String, &Value)> = match &models["models"] {
        Value::Object(map) => map.iter().map(|(k, v)| (k.clone(), v)).collect(),
        Value::Array(items) => items
            .iter()
            .map(|v| {
                (
                    v["id"]
                        .as_str()
                        .or_else(|| v["modelId"].as_str())
                        .or_else(|| v["name"].as_str())
                        .unwrap_or_default()
                        .to_owned(),
                    v,
                )
            })
            .collect(),
        _ => match &models["response"]["models"] {
            Value::Array(items) => items
                .iter()
                .map(|v| {
                    (
                        v["id"]
                            .as_str()
                            .or_else(|| v["modelId"].as_str())
                            .or_else(|| v["name"].as_str())
                            .unwrap_or_default()
                            .to_owned(),
                        v,
                    )
                })
                .collect(),
            _ => Vec::new(),
        },
    };
    entries
        .into_iter()
        .take(300)
        .map(|(id, model)| {
            json!({
                "id": id,
                "remaining": fraction(&model["quotaInfo"]["remainingFraction"]),
                "reset": model["quotaInfo"]["resetTime"].as_str().unwrap_or_default(),
            })
        })
        .collect()
}

/// Where the redacted quota diagnostic goes: `<harness home>/logs/agy-quota-debug.json`.
/// No token, no project id, no account name: only group/bucket shapes and numbers.
fn write_diagnostic(
    plan: &str,
    summary: Option<&Value>,
    skipped: &[String],
    fallback_used: bool,
    bars: &[RawBar],
    models: Option<&Value>,
    host: &str,
) {
    let home = std::env::var_os("HARNESS_HOME")
        .filter(|v| !v.is_empty())
        .map_or_else(
            || {
                std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                    .map_or_else(|| std::path::PathBuf::from("."), std::path::PathBuf::from)
                    .join(".harness")
            },
            std::path::PathBuf::from,
        );
    let groups: Vec<Value> = summary
        .and_then(|s| groups_of(s))
        .map(|groups| {
            groups
                .iter()
                .map(|group| {
                    let buckets: Vec<Value> = group["buckets"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|bucket| {
                            json!({
                                "bucketId": bucket["bucketId"].as_str().unwrap_or_default(),
                                "displayName": bucket["displayName"].as_str().unwrap_or_default(),
                                "window": bucket["window"].as_str().unwrap_or_default(),
                                "disabled": bucket["disabled"].as_bool().unwrap_or(false),
                                "hasRemaining": bucket["remainingFraction"].is_number()
                                    || bucket["remainingFraction"].is_string(),
                                "remaining": fraction(&bucket["remainingFraction"]),
                                "resetTime": bucket["resetTime"].as_str().unwrap_or_default(),
                            })
                        })
                        .collect();
                    json!({
                        "displayName": group["displayName"].as_str().unwrap_or_default(),
                        "buckets": buckets,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let doc = json!({
        "atUnix": now_secs(),
        "plan": plan,
        "userAgent": user_agent(),
        "host": host,
        "summaryGroups": groups,
        "summaryTopLevel": summary_shape(summary),
        "modelsQuota": models_quota(models),
        "skippedBuckets": skipped,
        "fallbackUsed": fallback_used,
        "barsShown": bars.iter().map(|(label, remaining, reset)| json!({
            "label": label, "remaining": remaining, "resetUnix": reset,
        })).collect::<Vec<_>>(),
    });
    let dir = home.join("logs");
    if std::fs::create_dir_all(&dir).is_ok() {
        let text = serde_json::to_string_pretty(&doc).unwrap_or_default();
        let _ = std::fs::write(dir.join("agy-quota-debug.json"), text);
    }
    eprintln!(
        "agy usage: plan={plan} groups={} bars={} skipped={:?} fallback={fallback_used}",
        groups.len(),
        bars.len(),
        skipped,
    );
}

/// The `groups` array of a quota summary, tolerating the Connect-RPC envelope
/// `{"response": {"groups": [...]}}` as well as a bare `{"groups": [...]}`.
fn groups_of(summary: &Value) -> Option<&Vec<Value>> {
    summary["groups"]
        .as_array()
        .or_else(|| summary["response"]["groups"].as_array())
}

/// A fraction that may arrive as a JSON number or as a numeric string.
fn fraction(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.parse::<f64>().ok()))
}
fn short_group(name: &str) -> String {
    let name = name
        .trim()
        .trim_end_matches(" models")
        .trim_end_matches(" Models");
    name.replace(" and ", " & ")
}

/// Every bucket of every quota group, as (label, remaining %, reset time), in
/// server order, plus the buckets that had to be skipped and why (never silent:
/// a skipped Gemini weekly / Argon bucket is exactly the bug this file had).
fn summary_bars(summary: &Value) -> (Vec<RawBar>, Vec<String>) {
    let mut out = Vec::new();
    let mut skipped = Vec::new();
    for group in groups_of(summary).into_iter().flatten() {
        let name = short_group(group["displayName"].as_str().unwrap_or("Models"));
        for bucket in group["buckets"].as_array().into_iter().flatten() {
            let bucket_name = bucket["displayName"]
                .as_str()
                .or_else(|| bucket["bucketId"].as_str())
                .unwrap_or("limit");
            if bucket["disabled"].as_bool() == Some(true) {
                skipped.push(format!("{name} · {bucket_name}: disabled by server"));
                continue;
            }
            let Some(remaining) = fraction(&bucket["remainingFraction"]) else {
                skipped.push(format!("{name} · {bucket_name}: no remainingFraction"));
                continue;
            };
            let window = match bucket["window"]
                .as_str()
                .unwrap_or_default()
                .to_ascii_lowercase()
                .as_str()
            {
                "weekly" => "weekly".to_owned(),
                "5h" | "5-hour" | "five_hour" | "five-hour" => "5-hour".to_owned(),
                "" => bucket_name.to_owned(),
                other => other.to_owned(),
            };
            #[allow(clippy::cast_possible_truncation)]
            let remaining = (remaining.clamp(0.0, 1.0) * 100.0) as f32;
            let reset = bucket["resetTime"].as_str().and_then(parse_rfc3339);
            out.push((format!("{name} · {window} remaining"), remaining, reset));
        }
    }
    (out, skipped)
}

/// Worst (lowest remaining) quota per family: [Gemini, Claude, GPT-OSS], as
/// (remaining %, reset time). Accepts the models as an object map or an array.
fn group(models: &Value) -> [Option<(f32, Option<i64>)>; 3] {
    let mut worst: [Option<(f64, Option<i64>)>; 3] = [None, None, None];
    let entries: Vec<(String, &Value)> = match models {
        Value::Object(map) => map.iter().map(|(k, v)| (k.clone(), v)).collect(),
        Value::Array(items) => items
            .iter()
            .map(|v| {
                (
                    v["id"]
                        .as_str()
                        .or_else(|| v["modelId"].as_str())
                        .or_else(|| v["name"].as_str())
                        .unwrap_or_default()
                        .to_owned(),
                    v,
                )
            })
            .collect(),
        _ => return [None, None, None],
    };
    for (id, model) in entries {
        if model["displayName"].as_str().is_none() || id.contains("image") {
            continue;
        }
        let slot = if id.starts_with("gemini") {
            0
        } else if id.starts_with("claude") {
            1
        } else if id.starts_with("gpt") {
            2
        } else {
            continue;
        };
        let quota = &model["quotaInfo"];
        let Some(remaining) = fraction(&quota["remainingFraction"]) else {
            continue;
        };
        let reset = quota["resetTime"].as_str().and_then(parse_rfc3339);
        if worst[slot].map_or(true, |(r, _)| remaining < r) {
            worst[slot] = Some((remaining, reset));
        }
    }
    worst.map(|w| {
        w.map(|(remaining, reset)| {
            #[allow(clippy::cast_possible_truncation)]
            let remaining = (remaining.clamp(0.0, 1.0) * 100.0) as f32;
            (remaining, reset)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_take_the_lowest_remaining_per_family() {
        let models = json!({
            "gemini-3-flash": {"displayName": "G3", "quotaInfo": {"remainingFraction": 0.8}},
            "gemini-4-argon-high": {"displayName": "G4", "quotaInfo": {"remainingFraction": 0.25}},
            "gemini-3.1-flash-image": {"displayName": "Img", "quotaInfo": {"remainingFraction": 0.0}},
            "claude-opus-5-5-high": {"displayName": "Opus", "quotaInfo": {"remainingFraction": 1.0}},
            "chat_1": {"quotaInfo": {"remainingFraction": 0.0}}
        });
        let g = group(&models);
        assert_eq!(g[0].as_ref().map(|x| x.0.round()), Some(25.0));
        assert_eq!(g[1].as_ref().map(|x| x.0), Some(100.0));
        assert!(g[2].is_none());
    }

    #[test]
    fn group_accepts_models_as_an_array() {
        let models = json!([
            {"id": "gemini-3-flash", "displayName": "G3", "quotaInfo": {"remainingFraction": 0.9}},
            {"id": "gpt-oss-120b", "displayName": "GPT", "quotaInfo": {"remainingFraction": "0.5"}}
        ]);
        let g = group(&models);
        assert_eq!(g[0].as_ref().map(|x| x.0.round()), Some(90.0));
        assert!(g[1].is_none());
        assert_eq!(g[2].as_ref().map(|x| x.0.round()), Some(50.0));
    }

    #[test]
    fn summary_groups_become_bars() {
        let v = json!({"groups": [{"displayName": "Claude and GPT models", "buckets": [
            {"window": "weekly", "remainingFraction": 0.53},
            {"window": "5h", "remainingFraction": 1},
            {"window": "5h", "disabled": true, "remainingFraction": 0}
        ]}, {"displayName": "Gemini 4 Argon", "buckets": [
            {"displayName": "Weekly Limit", "remainingFraction": 0.8227}
        ]}]});
        let (bars, skipped) = summary_bars(&v);
        assert_eq!(bars.len(), 3);
        assert_eq!(bars[0].0, "Claude & GPT · weekly remaining");
        assert_eq!(bars[0].1.round(), 53.0);
        assert_eq!(bars[2].0, "Gemini 4 Argon · Weekly Limit remaining");
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].contains("disabled"));
    }

    #[test]
    fn summary_reads_the_response_envelope_and_reports_missing_fractions() {
        let v = json!({"response": {"groups": [
            {"displayName": "Gemini Models", "buckets": [
                {"window": "weekly"},
                {"window": "5h", "remainingFraction": 0.9378}
            ]}
        ]}});
        let (bars, skipped) = summary_bars(&v);
        assert_eq!(bars.len(), 1);
        assert_eq!(bars[0].0, "Gemini · 5-hour remaining");
        assert_eq!(bars[0].1.round(), 94.0);
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].contains("no remainingFraction"));
    }

    #[test]
    fn user_screenshot_numbers_show_as_remaining() {
        // Antigravity's own panel: Gemini weekly 98.20% left, 5-hour 93.78%
        // left, Claude & GPT 100% left, Argon weekly 100% left. Worse used to
        // show those as 0% used and drop the weekly/Argon bars entirely.
        let v = json!({"groups": [
            {"displayName": "Gemini Models", "buckets": [
                {"window": "weekly", "remainingFraction": 0.982},
                {"window": "5h", "remainingFraction": 0.9378}
            ]},
            {"displayName": "Claude and GPT models", "buckets": [
                {"window": "weekly", "remainingFraction": 1},
                {"window": "5h", "remainingFraction": 1}
            ]},
            {"displayName": "Gemini 4 Argon", "buckets": [
                {"displayName": "Weekly Limit", "remainingFraction": 1}
            ]}
        ]});
        let (bars, skipped) = summary_bars(&v);
        assert!(skipped.is_empty());
        assert_eq!(bars.len(), 5);
        assert_eq!(bars[0].0, "Gemini · weekly remaining");
        assert_eq!(bars[0].1.round(), 98.0);
        assert_eq!(bars[1].1.round(), 94.0);
        assert_eq!(bars[4].0, "Gemini 4 Argon · Weekly Limit remaining");
        assert_eq!(bars[4].1.round(), 100.0);
    }

    #[test]
    fn remembered_numbers_show_as_of_and_passed_resets_read_full() {
        let now = now_secs();
        let bars = vec![
            (
                "Gemini · weekly remaining".to_owned(),
                60.0,
                Some(now + 3600),
            ),
            ("Claude · 5-hour remaining".to_owned(), 10.0, Some(now - 60)),
        ];
        let row = shown(
            "agy:sek2",
            "Antigravity · Sek2",
            "Pro".into(),
            &bars,
            Some(now - 1500),
        );
        assert_eq!(row.error, "");
        assert_eq!(row.bars[1].1, 100.0);
        assert!(row.bars[0].2.starts_with("Resets in"));
        assert!(row.summary.ends_with("(as of 25m ago)"));
        assert!(row.summary.contains("60% left"));
    }

    #[test]
    fn user_agent_matches_the_agy_cli() {
        // The quota server gates the grouped summary on the CLI's identity:
        // any other UA returns only the Claude & GPT group (seen live).
        let ua = user_agent();
        assert!(ua.starts_with("antigravity/cli/"), "{ua}");
        let arch = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            other => other,
        };
        assert!(ua.ends_with(arch), "{ua}");
    }

    #[test]
    fn daily_host_is_tried_before_prod() {
        // `agy` itself hits daily-cloudcode-pa; the prod host answered the
        // summary with only the Claude & GPT group even with the CLI UA.
        let hosts = base_hosts();
        assert_eq!(hosts[0], BASE_DAILY);
        assert!(hosts[0].starts_with("https://daily-"));
        assert_eq!(hosts[1], BASE);
    }

    #[test]
    fn models_quota_records_ids_and_fractions() {
        // The user's live run showed a Gemini fallback bar of 100% while
        // Antigravity itself reported 98.20% / 93.78%: the diagnostic must
        // show each model's id as seen by the grouping code, so a skipped
        // metered model (e.g. an id not starting with `gemini`) is visible.
        let models = json!({"models": {
            "gemini-2.5-flash": {"displayName": "Flash", "quotaInfo": {"remainingFraction": 0.982, "resetTime": "2026-11-21T06:59:40Z"}},
            "models/gemini-4-neon": {"displayName": "Neon", "quotaInfo": {"remainingFraction": 1.0}},
            "claude-opus-5-5": {"displayName": "Opus"}
        }});
        let q = models_quota(Some(&models));
        assert_eq!(q.len(), 3);
        let by_id = |id: &str| q.iter().find(|e| e["id"] == id).unwrap().clone();
        assert_eq!(by_id("gemini-2.5-flash")["remaining"], 0.982);
        assert_eq!(by_id("models/gemini-4-neon")["remaining"], 1.0);
        assert!(by_id("claude-opus-5-5")["remaining"].is_null());
    }

    #[test]
    fn summary_shape_lists_top_level_keys() {
        let v = json!({"groups": [{"displayName": "Claude and GPT models", "buckets": []}],
            "geminiGroups": [{"displayName": "Gemini Models"}],
            "project": "p"});
        let shape = summary_shape(Some(&v));
        assert_eq!(shape["groups"], "array[1]");
        assert_eq!(shape["geminiGroups"], "array[1]");
        assert_eq!(shape["project"], "string");
    }
}
