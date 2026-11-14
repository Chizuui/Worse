//! Claude Code hooks in `settings.json`: `hooks.<Event>[] { matcher, hooks[] { type, command, timeout,
//! statusMessage, async } }`, flattened to one [`Hook`] per command.

use std::path::Path;

use harness_settings::json::{OMap, OValue};

use crate::error::{ConfigError, Result};
use crate::jsonfile::{
    extra_of, get_bool, get_f64, get_str, set_bool, set_extra, set_number, set_str, JsonFile,
};
use crate::plan::FileChange;
use crate::types::Hook;

const HOOKS_KEY: &str = "hooks";
const KNOWN: &[&str] = &["type", "command", "timeout", "statusMessage", "async"];

fn parse_id(id: &str) -> Result<(String, usize, usize)> {
    let mut parts = id.rsplitn(3, '#');
    let hook = parts.next().and_then(|p| p.parse().ok());
    let group = parts.next().and_then(|p| p.parse().ok());
    let event = parts.next();
    match (event, group, hook) {
        (Some(e), Some(g), Some(h)) => Ok((e.to_owned(), g, h)),
        _ => Err(ConfigError::invalid(format!("invalid hook id {id:?}"))),
    }
}

fn matcher_of(group: &OValue) -> Option<String> {
    group
        .as_object()
        .and_then(|g| get_str(g, "matcher"))
        .filter(|m| !m.is_empty())
}

pub(crate) async fn list(path: &Path) -> Result<Vec<Hook>> {
    let file = JsonFile::load(path).await?;
    let mut out = Vec::new();
    let Some(events) = file.object(HOOKS_KEY) else {
        return Ok(out);
    };
    for (event, groups) in events.iter() {
        let OValue::Array(groups) = groups else {
            continue;
        };
        for (gi, group) in groups.iter().enumerate() {
            let matcher = matcher_of(group);
            let Some(OValue::Array(hooks)) = group.as_object().and_then(|g| g.get("hooks")) else {
                continue;
            };
            for (hi, hook) in hooks.iter().enumerate() {
                let Some(h) = hook.as_object() else {
                    continue;
                };
                out.push(Hook {
                    id: format!("{event}#{gi}#{hi}"),
                    event: event.to_owned(),
                    matcher: matcher.clone(),
                    hook_type: get_str(h, "type").unwrap_or_else(|| "command".into()),
                    command: get_str(h, "command").unwrap_or_default(),
                    timeout: get_f64(h, "timeout"),
                    status_message: get_str(h, "statusMessage"),
                    run_async: get_bool(h, "async"),
                    extra: extra_of(h, KNOWN),
                });
            }
        }
    }
    Ok(out)
}

fn write_hook(map: &mut OMap, hook: &Hook) {
    let hook_type = if hook.hook_type.is_empty() {
        "command"
    } else {
        &hook.hook_type
    };
    set_str(map, "type", Some(hook_type));
    set_str(map, "command", Some(&hook.command));
    set_number(map, "timeout", hook.timeout);
    set_str(map, "statusMessage", hook.status_message.as_deref());
    set_bool(map, "async", hook.run_async);
    set_extra(map, KNOWN, &hook.extra);
}

fn groups_mut<'a>(events: &'a mut OMap, event: &str) -> &'a mut Vec<OValue> {
    if !matches!(events.get(event), Some(OValue::Array(_))) {
        events.insert(event, OValue::Array(Vec::new()));
    }
    match events.get_mut(event) {
        Some(OValue::Array(groups)) => groups,
        _ => unreachable!("event was just set to an array"),
    }
}

fn hooks_of(group: &mut OValue) -> Option<&mut Vec<OValue>> {
    match group.as_object_mut()?.get_mut("hooks")? {
        OValue::Array(hooks) => Some(hooks),
        _ => None,
    }
}

/// Removes the hook at `(event, group, index)`; drops emptied groups and events. Returns the removed object.
fn remove_at(events: &mut OMap, event: &str, gi: usize, hi: usize) -> Result<OValue> {
    let not_found = || ConfigError::NotFound(format!("{event}#{gi}#{hi}"));
    let Some(OValue::Array(groups)) = events.get_mut(event) else {
        return Err(not_found());
    };
    let group = groups.get_mut(gi).ok_or_else(not_found)?;
    let hooks = hooks_of(group).ok_or_else(not_found)?;
    if hi >= hooks.len() {
        return Err(not_found());
    }
    let removed = hooks.remove(hi);
    if hooks.is_empty() {
        groups.remove(gi);
    }
    if groups.is_empty() {
        events.remove(event);
    }
    Ok(removed)
}

pub(crate) async fn plan_upsert(
    path: &Path,
    hook: &Hook,
    previous_id: Option<&str>,
) -> Result<FileChange> {
    if hook.event.trim().is_empty() || hook.event.contains('#') {
        return Err(ConfigError::invalid("a hook needs an event name"));
    }
    if hook.command.trim().is_empty() && hook.hook_type == "command" {
        return Err(ConfigError::invalid("a command hook needs a command"));
    }
    let mut file = JsonFile::load(path).await?;
    let events = file.root().object_mut(HOOKS_KEY);
    if let Some(id) = previous_id {
        let (event, gi, hi) = parse_id(id)?;
        let same_place = event == hook.event
            && match events.get(&event) {
                Some(OValue::Array(groups)) => {
                    groups.get(gi).map(matcher_of)
                        == Some(hook.matcher.clone().filter(|m| !m.is_empty()))
                }
                _ => false,
            };
        if same_place {
            let target = match events.get_mut(&event) {
                Some(OValue::Array(groups)) => groups
                    .get_mut(gi)
                    .and_then(hooks_of)
                    .and_then(|h| h.get_mut(hi))
                    .and_then(OValue::as_object_mut),
                _ => None,
            };
            let target = target.ok_or_else(|| ConfigError::NotFound(id.to_owned()))?;
            write_hook(target, hook);
            return Ok(file.into_change());
        }
        remove_at(events, &event, gi, hi)?;
    }
    let mut object = OMap::new();
    write_hook(&mut object, hook);
    let matcher = hook.matcher.clone().filter(|m| !m.is_empty());
    let groups = groups_mut(events, &hook.event);
    if let Some(group) = groups.iter_mut().find(|g| matcher_of(g) == matcher) {
        if let Some(hooks) = hooks_of(group) {
            hooks.push(OValue::Object(object));
            return Ok(file.into_change());
        }
    }
    let mut group = OMap::new();
    if let Some(m) = matcher {
        group.insert("matcher", OValue::String(m));
    }
    group.insert("hooks", OValue::Array(vec![OValue::Object(object)]));
    groups.push(OValue::Object(group));
    Ok(file.into_change())
}

pub(crate) async fn plan_delete(path: &Path, id: &str) -> Result<FileChange> {
    let (event, gi, hi) = parse_id(id)?;
    let mut file = JsonFile::load(path).await?;
    let events = file.root().object_mut(HOOKS_KEY);
    remove_at(events, &event, gi, hi)?;
    if events.is_empty() {
        file.root().remove(HOOKS_KEY);
    }
    Ok(file.into_change())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hook_round_trip() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"model":"opus","hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"echo hi","timeout":5,"custom":true}]}]}}"#,
        )?;
        let hooks = list(&path).await?;
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].id, "PreToolUse#0#0");
        assert!(hooks[0].extra.contains_key("custom"));
        assert!(
            plan_upsert(&path, &hooks[0], Some(&hooks[0].id))
                .await?
                .no_op
        );
        let mut moved = hooks[0].clone();
        moved.event = "PostToolUse".into();
        moved.matcher = None;
        let change = plan_upsert(&path, &moved, Some(&hooks[0].id)).await?;
        crate::plan::apply(&crate::plan::WritePlan::single(change)).await?;
        let hooks = list(&path).await?;
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].id, "PostToolUse#0#0");
        let change = plan_delete(&path, &hooks[0].id).await?;
        crate::plan::apply(&crate::plan::WritePlan::single(change)).await?;
        let text = std::fs::read_to_string(&path)?;
        assert!(!text.contains("hooks") && text.contains("opus"), "{text}");
        Ok(())
    }
}
