//! MCP server definitions in JSON files (Claude Code, agy, Harness overlay) and Codex `config.toml`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use harness_settings::fsx;
use harness_settings::json::OMap;
use toml_edit::{DocumentMut, Item, Table, Value};

use crate::error::{ConfigError, Result};
use crate::jsonfile::{
    extra_of, get_bool, get_list, get_map, get_str, set_bool, set_extra, set_list, set_map,
    set_str, JsonFile,
};
use crate::plan::{FileChange, Kind};
use crate::types::{McpServer, McpTransport};

/// JSON flavours of `mcpServers` maps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dialect {
    /// `~/.claude.json`, `<ws>/.mcp.json`: `type`, `command`, `args`, `env`, `url`, `headers`.
    Claude,
    /// agy `mcp_config.json`: `command`, `args`, `env`, `serverUrl`|`url`, `headers`, `disabled`.
    Agy,
    /// Harness `mcp.json`: Claude keys plus `enabled`, `agents`.
    Overlay,
}

impl Dialect {
    fn known(self) -> &'static [&'static str] {
        match self {
            Self::Claude => &["type", "command", "args", "env", "url", "headers"],
            Self::Agy => &[
                "command",
                "args",
                "env",
                "serverUrl",
                "url",
                "headers",
                "disabled",
            ],
            Self::Overlay => &[
                "type", "command", "args", "env", "url", "headers", "enabled", "agents",
            ],
        }
    }
}

pub(crate) const MCP_KEY: &str = "mcpServers";

/// Validates a server before writing.
pub(crate) fn validate(server: &McpServer) -> Result<()> {
    if server.name.trim().is_empty() || server.name.contains(['.', '/', '\\', '"']) {
        return Err(ConfigError::invalid(format!(
            "invalid server name {:?}",
            server.name
        )));
    }
    match server.transport {
        McpTransport::Stdio
            if server
                .command
                .as_deref()
                .map_or(true, |c| c.trim().is_empty()) =>
        {
            Err(ConfigError::invalid("a stdio server needs a command"))
        }
        McpTransport::Http | McpTransport::Sse
            if server.url.as_deref().map_or(true, |u| u.trim().is_empty()) =>
        {
            Err(ConfigError::invalid("an http/sse server needs a url"))
        }
        _ => Ok(()),
    }
}

fn transport_str(t: McpTransport) -> &'static str {
    match t {
        McpTransport::Stdio => "stdio",
        McpTransport::Http => "http",
        McpTransport::Sse => "sse",
    }
}

fn read_entry(name: &str, map: &OMap, dialect: Dialect) -> McpServer {
    let url = get_str(map, "url").or_else(|| {
        (dialect == Dialect::Agy)
            .then(|| get_str(map, "serverUrl"))
            .flatten()
    });
    let transport = match get_str(map, "type").as_deref() {
        Some("http" | "streamable-http" | "streamableHttp") => McpTransport::Http,
        Some("sse") => McpTransport::Sse,
        Some(_) => McpTransport::Stdio,
        None if url.is_some() => McpTransport::Http,
        None => McpTransport::Stdio,
    };
    McpServer {
        name: name.to_owned(),
        transport,
        command: get_str(map, "command"),
        args: get_list(map, "args"),
        env: get_map(map, "env"),
        url,
        headers: get_map(map, "headers"),
        enabled: match dialect {
            Dialect::Claude => None,
            Dialect::Agy => Some(!get_bool(map, "disabled").unwrap_or(false)),
            Dialect::Overlay => Some(get_bool(map, "enabled").unwrap_or(true)),
        },
        startup_timeout_sec: None,
        tool_timeout_sec: None,
        agents: if dialect == Dialect::Overlay {
            get_list(map, "agents")
        } else {
            Vec::new()
        },
        extra: extra_of(map, dialect.known()),
    }
}

fn write_entry(map: &mut OMap, server: &McpServer, dialect: Dialect, is_new: bool) {
    let stdio = server.transport == McpTransport::Stdio;
    if dialect != Dialect::Agy && (!stdio || is_new || map.get("type").is_some()) {
        set_str(map, "type", Some(transport_str(server.transport)));
    }
    set_str(map, "command", server.command.as_deref().filter(|_| stdio));
    set_list(map, "args", if stdio { &server.args } else { &[] });
    if !stdio {
        map.remove("args");
    }
    set_map(map, "env", &server.env);
    let url_key = if dialect == Dialect::Agy && (map.get("serverUrl").is_some() || is_new) {
        "serverUrl"
    } else {
        "url"
    };
    set_str(map, url_key, server.url.as_deref().filter(|_| !stdio));
    let no_headers = BTreeMap::new();
    set_map(
        map,
        "headers",
        if stdio { &no_headers } else { &server.headers },
    );
    match dialect {
        Dialect::Claude => {}
        Dialect::Agy => {
            let disabled = server.enabled == Some(false);
            if disabled || map.get("disabled").is_some() {
                set_bool(map, "disabled", Some(disabled));
            }
        }
        Dialect::Overlay => {
            set_bool(map, "enabled", Some(server.enabled.unwrap_or(true)));
            set_list(map, "agents", &server.agents);
        }
    }
    set_extra(map, dialect.known(), &server.extra);
}

/// Servers of a JSON file (missing file = none).
pub(crate) async fn list_json(path: &Path, dialect: Dialect) -> Result<Vec<McpServer>> {
    let file = JsonFile::load(path).await?;
    Ok(file
        .object(MCP_KEY)
        .map(|servers| {
            servers
                .iter()
                .filter_map(|(name, v)| v.as_object().map(|m| read_entry(name, m, dialect)))
                .collect()
        })
        .unwrap_or_default())
}

/// Creates or updates `server` (renaming from `previous_name`).
pub(crate) async fn plan_upsert_json(
    path: &Path,
    dialect: Dialect,
    server: &McpServer,
    previous_name: Option<&str>,
) -> Result<FileChange> {
    validate(server)?;
    let mut file = JsonFile::load(path).await?;
    let servers = file.root().object_mut(MCP_KEY);
    if let Some(previous) = previous_name.filter(|p| *p != server.name) {
        if servers.get(&server.name).is_some() {
            return Err(ConfigError::invalid(format!(
                "a server named {} already exists",
                server.name
            )));
        }
        if servers.get(previous).is_none() {
            return Err(ConfigError::NotFound(previous.to_owned()));
        }
        servers.rename(previous, &server.name);
    }
    let is_new = servers.get(&server.name).is_none();
    let entry = servers.object_mut(&server.name);
    write_entry(entry, server, dialect, is_new);
    Ok(file.into_change())
}

pub(crate) async fn plan_add_many_json(
    path: &Path,
    dialect: Dialect,
    servers: &[McpServer],
) -> Result<FileChange> {
    let mut file = JsonFile::load(path).await?;
    let map = file.root().object_mut(MCP_KEY);
    for server in servers {
        validate(server)?;
        let is_new = map.get(&server.name).is_none();
        write_entry(map.object_mut(&server.name), server, dialect, is_new);
    }
    Ok(file.into_change())
}

pub(crate) async fn plan_delete_json(path: &Path, name: &str) -> Result<FileChange> {
    let mut file = JsonFile::load(path).await?;
    let servers = file.root().object_mut(MCP_KEY);
    if servers.remove(name).is_none() {
        return Err(ConfigError::NotFound(name.to_owned()));
    }
    Ok(file.into_change())
}

pub(crate) async fn plan_enabled_json(
    path: &Path,
    dialect: Dialect,
    name: &str,
    enabled: bool,
) -> Result<FileChange> {
    let mut servers = list_json(path, dialect).await?;
    let mut server = servers
        .iter_mut()
        .find(|s| s.name == name)
        .map(std::mem::take)
        .ok_or_else(|| ConfigError::NotFound(name.to_owned()))?;
    server.enabled = Some(enabled);
    plan_upsert_json(path, dialect, &server, None).await
}

// ---------------------------------------------------------------------------------------------------------------
// Claude Code workspace approval (`.claude/settings.local.json`).

const ENABLED_KEY: &str = "enabledMcpjsonServers";
const DISABLED_KEY: &str = "disabledMcpjsonServers";
const ENABLE_ALL_KEY: &str = "enableAllProjectMcpServers";

/// Whether Claude Code uses project server `name` per the local settings file.
pub(crate) async fn claude_project_enabled(settings_local: &Path) -> Result<impl Fn(&str) -> bool> {
    let file = JsonFile::load(settings_local).await?;
    let root = file.tree.as_object().cloned().unwrap_or_default();
    let enabled = get_list(&root, ENABLED_KEY);
    let disabled = get_list(&root, DISABLED_KEY);
    let all = get_bool(&root, ENABLE_ALL_KEY).unwrap_or(false);
    Ok(move |name: &str| {
        !disabled.iter().any(|n| n == name) && (all || enabled.iter().any(|n| n == name))
    })
}

pub(crate) async fn plan_claude_project_enabled(
    settings_local: &Path,
    name: &str,
    enabled: bool,
) -> Result<FileChange> {
    let mut file = JsonFile::load(settings_local).await?;
    let root = file.root();
    let (add, remove) = if enabled {
        (ENABLED_KEY, DISABLED_KEY)
    } else {
        (DISABLED_KEY, ENABLED_KEY)
    };
    let mut removed = get_list(root, remove);
    if removed.iter().any(|n| n == name) {
        removed.retain(|n| n != name);
        set_list(root, remove, &removed);
    }
    let all = get_bool(root, ENABLE_ALL_KEY).unwrap_or(false);
    if !(enabled && all) {
        let mut added = get_list(root, add);
        if !added.iter().any(|n| n == name) {
            added.push(name.to_owned());
            set_list(root, add, &added);
        }
    }
    Ok(file.into_change())
}

// ---------------------------------------------------------------------------------------------------------------
// Codex `config.toml` (`[mcp_servers.<name>]`), edited with toml_edit so comments and unknown keys survive.

const CODEX_KNOWN: &[&str] = &[
    "command",
    "args",
    "env",
    "url",
    "http_headers",
    "enabled",
    "startup_timeout_sec",
    "tool_timeout_sec",
];

pub(crate) struct TomlFile {
    path: PathBuf,
    before: Option<String>,
    pub doc: DocumentMut,
}

impl TomlFile {
    pub(crate) async fn load(path: &Path) -> Result<Self> {
        let before = fsx::read_to_string_opt(path).await?;
        let doc = before
            .as_deref()
            .unwrap_or("")
            .parse::<DocumentMut>()
            .map_err(|e| ConfigError::invalid(format!("{}: {e}", path.display())))?;
        Ok(Self {
            path: path.to_path_buf(),
            before,
            doc,
        })
    }

    fn servers(&self) -> Option<&Table> {
        self.doc.get("mcp_servers").and_then(Item::as_table)
    }

    fn servers_mut(&mut self) -> &mut Table {
        let item = self.doc.entry("mcp_servers").or_insert_with(|| {
            let mut t = Table::new();
            t.set_implicit(true);
            Item::Table(t)
        });
        if !item.is_table() {
            let mut t = Table::new();
            t.set_implicit(true);
            *item = Item::Table(t);
        }
        match item {
            Item::Table(t) => t,
            _ => unreachable!("item was just set to a table"),
        }
    }

    pub(crate) fn into_change(self) -> FileChange {
        let text = self.doc.to_string();
        FileChange::write(self.path, self.before, text, Kind::Toml)
    }
}

pub(crate) fn toml_to_json(value: &Value) -> serde_json::Value {
    match value {
        Value::String(s) => serde_json::Value::String(s.value().clone()),
        Value::Integer(i) => serde_json::Value::from(*i.value()),
        Value::Float(f) => serde_json::Number::from_f64(*f.value())
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        Value::Boolean(b) => serde_json::Value::Bool(*b.value()),
        Value::Datetime(d) => serde_json::Value::String(d.value().to_string()),
        Value::Array(a) => serde_json::Value::Array(a.iter().map(toml_to_json).collect()),
        Value::InlineTable(t) => serde_json::Value::Object(
            t.iter()
                .map(|(k, v)| (k.to_owned(), toml_to_json(v)))
                .collect(),
        ),
    }
}

fn item_to_json(item: &Item) -> serde_json::Value {
    match item {
        Item::Value(v) => toml_to_json(v),
        Item::Table(t) => serde_json::Value::Object(
            t.iter()
                .map(|(k, v)| (k.to_owned(), item_to_json(v)))
                .collect(),
        ),
        Item::ArrayOfTables(a) => serde_json::Value::Array(
            a.iter()
                .map(|t| {
                    serde_json::Value::Object(
                        t.iter()
                            .map(|(k, v)| (k.to_owned(), item_to_json(v)))
                            .collect(),
                    )
                })
                .collect(),
        ),
        Item::None => serde_json::Value::Null,
    }
}

fn json_to_toml(value: &serde_json::Value) -> Option<Value> {
    Some(match value {
        serde_json::Value::Null => return None,
        serde_json::Value::Bool(b) => Value::from(*b),
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) => Value::from(i),
            None => Value::from(n.as_f64()?),
        },
        serde_json::Value::String(s) => Value::from(s.as_str()),
        serde_json::Value::Array(items) => {
            Value::Array(items.iter().filter_map(json_to_toml).collect())
        }
        serde_json::Value::Object(map) => {
            let mut t = toml_edit::InlineTable::new();
            for (k, v) in map {
                if let Some(v) = json_to_toml(v) {
                    t.insert(k, v);
                }
            }
            Value::InlineTable(t)
        }
    })
}

/// Sets `key` to `desired` (JSON form) unless the current item already means the same (keeps its formatting).
fn set_toml(table: &mut Table, key: &str, desired: Option<serde_json::Value>) {
    let current = table.get(key).map(item_to_json);
    match desired {
        None | Some(serde_json::Value::Null) => {
            table.remove(key);
        }
        Some(d) if current.as_ref() == Some(&d) => {}
        Some(d) => {
            if let (Some(Item::Table(sub)), serde_json::Value::Object(map)) =
                (table.get_mut(key), &d)
            {
                // A `[x.env]` sub-table stays a sub-table: rewrite its keys in place.
                sub.clear();
                for (k, v) in map {
                    if let Some(v) = json_to_toml(v) {
                        sub.insert(k, Item::Value(v));
                    }
                }
                return;
            }
            if let Some(v) = json_to_toml(&d) {
                table.insert(key, Item::Value(v));
            }
        }
    }
}

fn str_map_json(map: &BTreeMap<String, String>) -> Option<serde_json::Value> {
    (!map.is_empty()).then(|| {
        serde_json::Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), v.clone().into()))
                .collect(),
        )
    })
}

fn f64_json(v: Option<f64>) -> Option<serde_json::Value> {
    v.and_then(crate::jsonfile::number)
        .map(serde_json::Value::Number)
}

fn read_codex(name: &str, table: &Table) -> McpServer {
    let json = item_to_json(&Item::Table(table.clone()));
    let get = |k: &str| json.get(k);
    let str_of = |k: &str| {
        get(k)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    let map_of = |k: &str| -> BTreeMap<String, String> {
        get(k)
            .and_then(serde_json::Value::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                    .collect()
            })
            .unwrap_or_default()
    };
    let url = str_of("url");
    McpServer {
        name: name.to_owned(),
        transport: if url.is_some() {
            McpTransport::Http
        } else {
            McpTransport::Stdio
        },
        command: str_of("command"),
        args: get("args")
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        env: map_of("env"),
        url,
        headers: map_of("http_headers"),
        enabled: Some(
            get("enabled")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true),
        ),
        startup_timeout_sec: get("startup_timeout_sec").and_then(serde_json::Value::as_f64),
        tool_timeout_sec: get("tool_timeout_sec").and_then(serde_json::Value::as_f64),
        agents: Vec::new(),
        extra: json
            .as_object()
            .map(|m| {
                m.iter()
                    .filter(|(k, _)| !CODEX_KNOWN.contains(&k.as_str()))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

pub(crate) async fn list_toml(path: &Path) -> Result<Vec<McpServer>> {
    let file = TomlFile::load(path).await?;
    Ok(file
        .servers()
        .map(|servers| {
            servers
                .iter()
                .filter_map(|(name, item)| item.as_table().map(|t| read_codex(name, t)))
                .collect()
        })
        .unwrap_or_default())
}

pub(crate) async fn plan_upsert_toml(
    path: &Path,
    server: &McpServer,
    previous_name: Option<&str>,
) -> Result<FileChange> {
    validate(server)?;
    if server.transport == McpTransport::Sse {
        return Err(ConfigError::invalid(
            "Codex supports stdio and streamable http servers, not sse",
        ));
    }
    let mut file = TomlFile::load(path).await?;
    let servers = file.servers_mut();
    if let Some(previous) = previous_name.filter(|p| *p != server.name) {
        if servers.contains_key(&server.name) {
            return Err(ConfigError::invalid(format!(
                "a server named {} already exists",
                server.name
            )));
        }
        let item = servers
            .remove(previous)
            .ok_or_else(|| ConfigError::NotFound(previous.to_owned()))?;
        servers.insert(&server.name, item);
    }
    if !servers.get(&server.name).is_some_and(Item::is_table) {
        servers.insert(&server.name, Item::Table(Table::new()));
    }
    let Some(Item::Table(table)) = servers.get_mut(&server.name) else {
        unreachable!("server table was just inserted");
    };
    let stdio = server.transport == McpTransport::Stdio;
    set_toml(
        table,
        "command",
        server.command.clone().filter(|_| stdio).map(Into::into),
    );
    set_toml(
        table,
        "args",
        (stdio && (!server.args.is_empty() || table.contains_key("args"))).then(|| {
            serde_json::Value::Array(server.args.iter().cloned().map(Into::into).collect())
        }),
    );
    set_toml(table, "env", str_map_json(&server.env));
    set_toml(
        table,
        "url",
        server.url.clone().filter(|_| !stdio).map(Into::into),
    );
    set_toml(
        table,
        "http_headers",
        if stdio {
            None
        } else {
            str_map_json(&server.headers)
        },
    );
    let enabled = server.enabled.unwrap_or(true);
    set_toml(
        table,
        "enabled",
        (!enabled || table.contains_key("enabled")).then_some(serde_json::Value::Bool(enabled)),
    );
    set_toml(
        table,
        "startup_timeout_sec",
        f64_json(server.startup_timeout_sec),
    );
    set_toml(table, "tool_timeout_sec", f64_json(server.tool_timeout_sec));
    let stale: Vec<String> = table
        .iter()
        .map(|(k, _)| k.to_owned())
        .filter(|k| !CODEX_KNOWN.contains(&k.as_str()) && !server.extra.contains_key(k))
        .collect();
    for k in stale {
        table.remove(&k);
    }
    for (k, v) in &server.extra {
        if !CODEX_KNOWN.contains(&k.as_str()) {
            set_toml(table, k, Some(v.clone()));
        }
    }
    Ok(file.into_change())
}

pub(crate) async fn plan_delete_toml(path: &Path, name: &str) -> Result<FileChange> {
    let mut file = TomlFile::load(path).await?;
    if file.servers_mut().remove(name).is_none() {
        return Err(ConfigError::NotFound(name.to_owned()));
    }
    Ok(file.into_change())
}

pub(crate) async fn plan_enabled_toml(
    path: &Path,
    name: &str,
    enabled: bool,
) -> Result<FileChange> {
    let mut server = list_toml(path)
        .await?
        .into_iter()
        .find(|s| s.name == name)
        .ok_or_else(|| ConfigError::NotFound(name.to_owned()))?;
    server.enabled = Some(enabled);
    plan_upsert_toml(path, &server, None).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stdio(name: &str) -> McpServer {
        McpServer {
            name: name.into(),
            command: Some("npx".into()),
            args: vec!["-y".into(), "pkg".into()],
            ..McpServer::default()
        }
    }

    #[tokio::test]
    async fn codex_toml_keeps_comments_and_unknown_keys() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("config.toml");
        let original = "# my config\nmodel = \"o3\" # inline\n\n[mcp_servers.gh]\n# gh server\ncommand = \"gh\"\nargs = [\"mcp\"]\ncwd = \"/tmp\"\n\n[mcp_servers.gh.env]\nGH_TOKEN = \"SECRET\"\n\n[profiles.x]\nmodel = \"y\"\n";
        std::fs::write(&path, original)?;
        let servers = list_toml(&path).await?;
        assert_eq!(servers.len(), 1);
        let gh = &servers[0];
        assert_eq!(gh.extra.get("cwd").and_then(|v| v.as_str()), Some("/tmp"));
        assert_eq!(gh.env.get("GH_TOKEN").map(String::as_str), Some("SECRET"));
        // Same content → no-op.
        assert!(plan_upsert_toml(&path, gh, None).await?.no_op);
        // Disable + add.
        let change = plan_enabled_toml(&path, "gh", false).await?;
        assert!(!change.diff.contains("SECRET"), "{}", change.diff);
        crate::plan::apply(&crate::plan::WritePlan::single(change)).await?;
        let change = plan_upsert_toml(&path, &stdio("new"), None).await?;
        crate::plan::apply(&crate::plan::WritePlan::single(change)).await?;
        let text = std::fs::read_to_string(&path)?;
        for keep in [
            "# my config",
            "model = \"o3\" # inline",
            "# gh server",
            "cwd = \"/tmp\"",
            "[profiles.x]",
            "enabled = false",
            "[mcp_servers.new]",
        ] {
            assert!(text.contains(keep), "{keep} missing in\n{text}");
        }
        let names: Vec<String> = list_toml(&path)
            .await?
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, vec!["gh".to_owned(), "new".to_owned()]);
        Ok(())
    }

    #[tokio::test]
    async fn json_round_trip_keeps_order_and_extra() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join(".claude.json");
        let original = "{\n  \"numStartups\": 3,\n  \"primaryApiKey\": \"SECRET\",\n  \"mcpServers\": {\n    \"b\": {\"type\": \"stdio\", \"command\": \"x\", \"args\": [], \"env\": {}, \"custom\": 1},\n    \"a\": {\"type\": \"http\", \"url\": \"https://x\"}\n  },\n  \"tail\": true\n}\n";
        std::fs::write(&path, original)?;
        let servers = list_json(&path, Dialect::Claude).await?;
        assert_eq!(
            servers.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["b", "a"]
        );
        assert_eq!(servers[1].transport, McpTransport::Http);
        for s in &servers {
            assert!(
                plan_upsert_json(&path, Dialect::Claude, s, None)
                    .await?
                    .no_op,
                "{}",
                s.name
            );
        }
        let mut b = servers[0].clone();
        b.name = "b2".into();
        let change = plan_upsert_json(&path, Dialect::Claude, &b, Some("b")).await?;
        assert!(!change.diff.contains("SECRET"));
        crate::plan::apply(&crate::plan::WritePlan::single(change)).await?;
        let text = std::fs::read_to_string(&path)?;
        assert!(text.find("numStartups") < text.find("mcpServers"));
        assert!(text.find("\"b2\"") < text.find("\"a\""));
        assert!(text.contains("\"custom\": 1"));
        assert!(text.contains("\"primaryApiKey\": \"SECRET\""));
        assert!(text.trim_end().ends_with("\"tail\": true\n}"));
        Ok(())
    }

    #[tokio::test]
    async fn claude_project_approval() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("settings.local.json");
        let change = plan_claude_project_enabled(&path, "s", true).await?;
        crate::plan::apply(&crate::plan::WritePlan::single(change)).await?;
        assert!(claude_project_enabled(&path).await?("s"));
        let change = plan_claude_project_enabled(&path, "s", false).await?;
        crate::plan::apply(&crate::plan::WritePlan::single(change)).await?;
        assert!(!claude_project_enabled(&path).await?("s"));
        Ok(())
    }
}
