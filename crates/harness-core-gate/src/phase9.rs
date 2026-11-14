//! Phase 9.1 core gate (spec phase-9-parity.md §9.1.7): settings store, agent-config round trips, MCP overlay on
//! `session/new`, modes/models/options, data dir relocation. Run through `scripts/e2e/phase9-settings.mjs`, which
//! provides the isolated home (`HOME`, `CODEX_HOME`, `CLAUDE_CONFIG_DIR`) and scans every output and data file for
//! the fixture secrets below.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use harness_agent_config::{
    AgentConfigService, AgentKind, AuthState, Command, Concept, ConfigError, ConfigPaths,
    ConfigTarget, Entry, EntryBody, FileExistenceProbe, Hook, McpServer, McpTransport, Scope,
    Skill, Subagent,
};
use harness_agent_hub::{Hub, HubConfig};
use harness_core_api::{
    AddAccount, AgentDefaults, AgentId, ApiError, AuthPresence, EventKind, HarnessCore,
    ListSessions, SessionId,
};
use harness_settings::relocation::{self, FailPoint};
use harness_settings::{Language, SaveOutcome, SettingsStore, ThemePref};

use super::{check, step, Env, Gate, A, B, R};

/// Fixture secrets (the script has the same list and scans for them; none may appear outside the fixtures).
pub const SECRETS: [&str; 6] = [
    "sk-ant-FIXTURE-cred-0001",
    "sk-codex-FIXTURE-auth-0002",
    "ya29.FIXTURE-oauth-0003",
    "sk-ant-FIXTURE-primary-0004",
    "FIXTURE-bearer-0005",
    "FIXTURE-envsecret-0006",
];

pub async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => {
            println!("GATE_OK");
            std::process::ExitCode::SUCCESS
        }
        Err(reason) => {
            println!("GATE_FAIL {reason}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn io<T>(what: &str, r: std::io::Result<T>) -> R<T> {
    r.map_err(|e| format!("{what}: {e}"))
}

fn cfg<T>(what: &str, r: Result<T, ConfigError>) -> R<T> {
    r.map_err(|e| format!("{what}: {e}"))
}

fn write(path: &Path, text: &str) -> R<()> {
    if let Some(dir) = path.parent() {
        io("mkdir", std::fs::create_dir_all(dir))?;
    }
    io(
        &format!("write {}", path.display()),
        std::fs::write(path, text),
    )
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

struct Layout {
    /// Isolated user home (parent of the data dir).
    home: PathBuf,
    claude_dir: PathBuf,
    codex_home: PathBuf,
    workspace: PathBuf,
}

/// Seeds the isolated home: CLI configs with unknown keys, TOML comments and dummy secrets.
fn seed(env: &Env) -> R<Layout> {
    let home = env
        .home
        .parent()
        .ok_or("HARNESS_HOME has no parent")?
        .to_path_buf();
    let var = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let claude_dir = var("CLAUDE_CONFIG_DIR").ok_or("CLAUDE_CONFIG_DIR must be set (isolated)")?;
    let codex_home = var("CODEX_HOME").ok_or("CODEX_HOME must be set (isolated)")?;
    for dir in [&claude_dir, &codex_home] {
        check(
            dir.starts_with(&home),
            format!("{} is not inside the isolated home", dir.display()),
        )?;
    }
    let [s1, s2, s3, s4, s5, s6] = SECRETS;
    write(
        &claude_dir.join(".credentials.json"),
        &format!(r#"{{"claudeAiOauth":{{"accessToken":"{s1}"}}}}"#),
    )?;
    write(
        &claude_dir.join(".claude.json"),
        &format!(
            "{{\n  \"primaryApiKey\": \"{s4}\",\n  \"unknownTop\": 1,\n  \"mcpServers\": {{\n    \"existing\": {{\"command\": \"node\", \"args\": [\"x.js\"], \"customKey\": \"keep\"}}\n  }}\n}}\n"
        ),
    )?;
    write(
        &claude_dir.join("settings.json"),
        "{\n  \"theme\": \"dark\",\n  \"unknownSetting\": true\n}\n",
    )?;
    write(
        &codex_home.join("auth.json"),
        &format!(r#"{{"OPENAI_API_KEY":"{s2}"}}"#),
    )?;
    write(
        &codex_home.join("config.toml"),
        &format!(
            "# keep this comment\nmodel = \"gpt-5\" # inline comment\n\n[mcp_servers.local]\ncommand = \"node\"\nargs = [\"local.js\"]\nenv = {{ API_KEY = \"{s6}\" }}\n\n[mcp_servers.remote]\nurl = \"https://example.invalid/mcp\"\nhttp_headers = {{ Authorization = \"Bearer {s5}\" }}\n"
        ),
    )?;
    write(
        &home.join(".gemini").join("oauth_creds.json"),
        &format!(r#"{{"access_token":"{s3}"}}"#),
    )?;
    let workspace = env.workspace.clone();
    write(
        &workspace.join(".claude").join("settings.json"),
        "{\"wsUnknown\": [1, 2]}\n",
    )?;
    write(&workspace.join("CLAUDE.md"), "# Workspace rules\n")?;
    Ok(Layout {
        home,
        claude_dir,
        codex_home,
        workspace,
    })
}

async fn run() -> R<()> {
    let env = phase9_env()?;
    let layout = seed(&env)?;
    settings_roundtrip(&env).await?;
    let svc = AgentConfigService::new(
        cfg(
            "paths",
            ConfigPaths::from_env(&env.home, Some(layout.workspace.clone())),
        )?,
        Arc::new(FileExistenceProbe),
    );
    concept_roundtrip(&svc, &layout).await?;
    conflict(&svc, &env).await?;
    diff_masking(&svc).await?;
    auth(&svc).await?;
    let session = overlay_session_new(&env, &svc).await?;
    modes_models(&env, &session).await?;
    relocation_step(&env, &layout, &session).await?;
    accounts_step(&env, &layout).await?;
    super::phase9_providers::run(&env, &layout.home).await?;
    super::phase9_delegate::run(&env, &layout.home).await?;
    let data_dir = relocation::resolve_data_dir(&env.home).await.data_dir;
    super::phase9_admin::run(&env, &data_dir).await
}

/// Data dir = `HARNESS_HOME`; workspace, fake-agent state and log live next to it (outside), so relocation moves only
/// Harness data.
fn phase9_env() -> R<Env> {
    let mut env = Env::from_env()?;
    let parent = env
        .home
        .parent()
        .ok_or("HARNESS_HOME has no parent")?
        .to_path_buf();
    env.workspace = parent.join("ws9");
    env.state = parent.join("fake-state9");
    env.log = parent.join("fake-agent9.jsonl");
    for dir in [&env.workspace, &env.state] {
        io("mkdir", std::fs::create_dir_all(dir))?;
    }
    Ok(env)
}

pub(super) fn hub_config(env: &Env, home: &Path, defaults: bool) -> HubConfig {
    let mut config = env.config();
    config.home = home.to_path_buf();
    config.turn_snapshots = false;
    if defaults {
        config.agent_defaults.insert(
            AgentId::new(B),
            AgentDefaults {
                model: Some("fake-large".into()),
                ..AgentDefaults::default()
            },
        );
    }
    config
}

pub(super) async fn start(config: HubConfig) -> R<Gate> {
    let hub = Hub::start(config).await.map_err(|e| e.to_string())?;
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut stream = hub.subscribe();
    let sink = Arc::clone(&events);
    tokio::spawn(async move {
        while let Ok(event) = stream.recv().await {
            if let EventKind::AgentRuntime { pid: Some(pid), .. } = &event.kind {
                println!("AGENT_PID {pid}");
            }
            if let Ok(mut all) = sink.lock() {
                all.push(event);
            }
        }
    });
    Ok(Gate { hub, events })
}

// ---- accounts (§9.6) -------------------------------------------------------------------------------------

/// Hub config whose first fake agent plays `claude-code`, so accounts can be based on it.
fn accounts_config(env: &Env, home: &Path) -> HubConfig {
    let mut config = hub_config(env, home, false);
    if let Some(first) = config.agents.first_mut() {
        first.id = AgentId::new("claude-code");
    }
    config.default_fallback_order = config.agents.iter().map(|a| a.id.clone()).collect();
    config
}

async fn accounts_step(env: &Env, layout: &Layout) -> R<()> {
    let home = layout.home.join("accounts-data");
    io("mkdir", std::fs::create_dir_all(&home))?;
    let gate = start(accounts_config(env, &home)).await?;
    let work = gate
        .hub
        .add_account(AddAccount {
            base: AgentId::new("claude-code"),
            label: "Work".into(),
        })
        .await
        .map_err(|e| format!("add_account: {e}"))?;
    check(
        work.id.as_str() == "claude-code-work" && !work.loaded,
        format!("new account {work:?}"),
    )?;
    check(
        work.auth == AuthPresence::Absent,
        "fresh profile must not look signed in",
    )?;
    let profile = PathBuf::from(&work.profile_dir);
    check(
        read(&profile.join("settings.json")).contains("unknownSetting"),
        "settings.json not shared into the profile",
    )?;
    check(
        !profile.join(".credentials.json").exists() && !profile.join(".claude.json").exists(),
        "credentials or .claude.json leaked into the profile",
    )?;
    let twin = gate
        .hub
        .add_account(AddAccount {
            base: AgentId::new("claude-code"),
            label: "Work".into(),
        })
        .await
        .map_err(|e| format!("add twin: {e}"))?;
    check(
        twin.id.as_str() == "claude-code-work-2",
        "duplicate label id",
    )?;
    gate.hub
        .remove_account(twin.id.clone())
        .await
        .map_err(|e| format!("remove twin: {e}"))?;
    check(
        gate.hub
            .add_account(AddAccount {
                base: AgentId::new(B),
                label: "x".into(),
            })
            .await
            .is_err(),
        "accounts must only be based on claude-code",
    )?;
    // A saved order from before the account existed must still get the account (fix in Hub::start).
    gate.hub
        .set_fallback_order(vec![AgentId::new(B), AgentId::new("claude-code")])
        .await
        .map_err(|e| format!("set order: {e}"))?;
    gate.hub.shutdown().await.map_err(|e| e.to_string())?;

    let gate = start(accounts_config(env, &home)).await?;
    let agents = gate.hub.list_agents().await.map_err(|e| e.to_string())?;
    check(
        agents.iter().any(|a| a.id.as_str() == "claude-code-work"),
        "account agent missing after restart",
    )?;
    let order = gate
        .hub
        .get_fallback_order()
        .await
        .map_err(|e| e.to_string())?;
    let ids: Vec<&str> = order.iter().map(AgentId::as_str).collect();
    // The saved order stays (B before claude-code); missing agents go to their default position, so the account
    // sits right after claude-code.
    let pos = |id: &str| ids.iter().position(|a| *a == id);
    check(
        ids.first() == Some(&B)
            && pos("claude-code").is_some()
            && pos("claude-code-work") == pos("claude-code").map(|p| p + 1),
        format!("fallback order after restart: {ids:?}"),
    )?;
    let listed = gate.hub.list_accounts().await.map_err(|e| e.to_string())?;
    check(
        listed.len() == 1 && listed.iter().all(|a| a.loaded),
        format!("accounts after restart: {listed:?}"),
    )?;
    gate.hub
        .remove_account(AgentId::new("claude-code-work"))
        .await
        .map_err(|e| format!("remove: {e}"))?;
    gate.hub.shutdown().await.map_err(|e| e.to_string())?;
    step("accounts");
    Ok(())
}

// ---- 1. settings ---------------------------------------------------------------------------------------

async fn settings_roundtrip(env: &Env) -> R<()> {
    let dir = &env.home;
    write(
        &dir.join(harness_settings::LEGACY_FILE),
        r#"{"theme":"dark","language":"zh-CN"}"#,
    )?;
    let store = SettingsStore::open(dir)
        .await
        .map_err(|e| format!("open: {e}"))?;
    check(
        store.snapshot().appearance.theme == ThemePref::Dark
            && store.snapshot().general.language == Language::ZhCn,
        "ui-settings.json not migrated",
    )?;
    // An unknown key written by someone else must survive our writes.
    let path = dir.join(harness_settings::SETTINGS_FILE);
    let mut value: serde_json::Value =
        serde_json::from_str(&read(&path)).map_err(|e| format!("settings.json: {e}"))?;
    value["zzFuture"] = serde_json::json!({"keep": 1});
    write(
        &path,
        &serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?,
    )?;
    store.reload().await.map_err(|e| format!("reload: {e}"))?;
    let mut rx = store.subscribe();
    rx.borrow_and_update();
    let outcome = store
        .patch(|s| s.appearance.theme = ThemePref::Light)
        .await
        .map_err(|e| format!("patch: {e}"))?;
    check(outcome == SaveOutcome::Changed, "patch reported no change")?;
    check(
        rx.has_changed().unwrap_or(false),
        "no change event after patch",
    )?;
    let text = read(&path);
    check(
        text.contains("zzFuture") && text.contains("\"light\""),
        "settings.json lost the unknown key or the value",
    )?;
    let mtime = io("stat", std::fs::metadata(&path).and_then(|m| m.modified()))?;
    let same = store
        .patch(|s| s.appearance.theme = ThemePref::Light)
        .await
        .map_err(|e| format!("patch: {e}"))?;
    check(same == SaveOutcome::Unchanged, "same-value patch wrote")?;
    let after = io("stat", std::fs::metadata(&path).and_then(|m| m.modified()))?;
    check(mtime == after, "same-value patch changed the mtime")?;
    drop(store);
    let reopened = SettingsStore::open(dir)
        .await
        .map_err(|e| format!("reopen: {e}"))?;
    check(
        reopened.snapshot().appearance.theme == ThemePref::Light,
        "value lost after reopen",
    )?;
    step("settings_roundtrip");
    Ok(())
}

// ---- 2. every concept cell -----------------------------------------------------------------------------

fn body_for(concept: Concept, revision: u32) -> Option<EntryBody> {
    let description = Some(format!("Gate {revision}"));
    Some(match concept {
        Concept::Mcp => EntryBody::Mcp(McpServer {
            name: "gate-mcp".into(),
            transport: McpTransport::Stdio,
            command: Some("node".into()),
            args: vec![format!("gate-{revision}.js")],
            env: BTreeMap::from([("GATE_VAR".to_owned(), "plain".to_owned())]),
            ..McpServer::default()
        }),
        Concept::Skill => EntryBody::Skill(Skill {
            name: "gate-skill".into(),
            description,
            body: format!("Do the gate thing {revision}.\n"),
            ..Skill::default()
        }),
        Concept::Command => EntryBody::Command(Command {
            name: "gate-cmd".into(),
            description,
            body: format!("Run gate {revision}.\n"),
            ..Command::default()
        }),
        Concept::Hook => EntryBody::Hook(Hook {
            event: "PreToolUse".into(),
            matcher: Some("Bash".into()),
            hook_type: "command".into(),
            command: format!("echo gate-{revision}"),
            ..Hook::default()
        }),
        Concept::Subagent => EntryBody::Subagent(Subagent {
            name: "gate-agent".into(),
            description,
            body: format!("You are gate agent {revision}.\n"),
            ..Subagent::default()
        }),
        Concept::Instructions => return None,
    })
}

/// The listed entry that corresponds to `body` (hooks: by command), if equal in the fields we wrote.
fn find<'a>(entries: &'a [Entry], body: &EntryBody) -> Option<&'a Entry> {
    entries.iter().find(|e| match (&e.body, body) {
        (EntryBody::Mcp(a), EntryBody::Mcp(b)) => {
            a.name == b.name && a.command == b.command && a.args == b.args && a.env == b.env
        }
        (EntryBody::Skill(a), EntryBody::Skill(b)) => {
            a.name == b.name && a.description == b.description && a.body.trim() == b.body.trim()
        }
        (EntryBody::Command(a), EntryBody::Command(b)) => {
            a.name == b.name && a.description == b.description && a.body.trim() == b.body.trim()
        }
        (EntryBody::Hook(a), EntryBody::Hook(b)) => {
            a.event == b.event && a.matcher == b.matcher && a.command == b.command
        }
        (EntryBody::Subagent(a), EntryBody::Subagent(b)) => {
            a.name == b.name && a.description == b.description && a.body.trim() == b.body.trim()
        }
        _ => false,
    })
}

fn name_of(entry: &Entry) -> String {
    entry.name.clone()
}

async fn concept_roundtrip(svc: &AgentConfigService, layout: &Layout) -> R<()> {
    let agents = [
        AgentKind::ClaudeCode,
        AgentKind::Codex,
        AgentKind::Agy,
        AgentKind::Harness,
    ];
    let mut cells = 0;
    for agent in agents {
        let support: Vec<_> = svc.capabilities(agent);
        for concept in Concept::ALL {
            let row = support
                .iter()
                .find(|s| s.concept == concept)
                .ok_or("missing capability row")?;
            for (scope, supported) in [(Scope::User, row.user), (Scope::Workspace, row.workspace)] {
                let target = ConfigTarget::new(agent, scope);
                let label = format!("{agent:?}/{concept:?}/{scope:?}");
                if !supported {
                    match svc.list(concept, target).await {
                        Err(ConfigError::Unsupported { .. }) => {}
                        other => {
                            return Err(format!(
                                "{label}: expected Unsupported, got {:?}",
                                other.map(|v| v.len())
                            ))
                        }
                    }
                    continue;
                }
                cells += 1;
                if concept == Concept::Instructions {
                    instructions_cell(svc, target, &label).await?;
                } else {
                    entry_cell(svc, concept, target, &label).await?;
                }
            }
        }
    }
    // Unknown keys and comments of the seeded files survived all writes.
    let preserved = [
        (layout.claude_dir.join("settings.json"), "unknownSetting"),
        (layout.claude_dir.join(".claude.json"), "unknownTop"),
        (layout.claude_dir.join(".claude.json"), "customKey"),
        (layout.codex_home.join("config.toml"), "# keep this comment"),
        (layout.codex_home.join("config.toml"), "# inline comment"),
        (
            layout.workspace.join(".claude").join("settings.json"),
            "wsUnknown",
        ),
    ];
    for (file, needle) in preserved {
        check(
            read(&file).contains(needle),
            format!("{} lost {needle}", file.display()),
        )?;
    }
    println!("NOTE concept cells round-tripped: {cells}");
    step("concept_roundtrip");
    Ok(())
}

async fn entry_cell(
    svc: &AgentConfigService,
    concept: Concept,
    target: ConfigTarget,
    label: &str,
) -> R<()> {
    let first = body_for(concept, 1).ok_or("no body")?;
    let plan = cfg(label, svc.plan_upsert(target, &first, None).await)?;
    check(!plan.is_no_op(), format!("{label}: create is a no-op"))?;
    cfg(label, svc.apply(&plan).await)?;
    let listed = cfg(label, svc.list(concept, target).await)?;
    let entry = find(&listed, &first).ok_or(format!("{label}: created entry not listed"))?;
    let name = name_of(entry);
    let again = cfg(label, svc.plan_upsert(target, &first, Some(&name)).await)?;
    check(
        again.is_no_op(),
        format!("{label}: same content is not a no-op"),
    )?;
    let second = body_for(concept, 2).ok_or("no body")?;
    let plan = cfg(label, svc.plan_upsert(target, &second, Some(&name)).await)?;
    cfg(label, svc.apply(&plan).await)?;
    let listed = cfg(label, svc.list(concept, target).await)?;
    let entry = find(&listed, &second).ok_or(format!("{label}: update not listed"))?;
    check(
        find(&listed, &first).is_none(),
        format!("{label}: old version still listed"),
    )?;
    let name = name_of(entry);
    let plan = cfg(label, svc.plan_delete(concept, target, &name).await)?;
    cfg(label, svc.apply(&plan).await)?;
    let listed = cfg(label, svc.list(concept, target).await)?;
    check(
        find(&listed, &second).is_none(),
        format!("{label}: entry still listed after delete"),
    )
}

async fn instructions_cell(svc: &AgentConfigService, target: ConfigTarget, label: &str) -> R<()> {
    let listed = cfg(label, svc.list(Concept::Instructions, target).await)?;
    let file = listed
        .iter()
        .find_map(|e| match &e.body {
            EntryBody::Instructions { file, .. } => Some(file.clone()),
            _ => None,
        })
        .ok_or(format!("{label}: no instruction candidate"))?;
    let before = cfg(label, svc.read_instructions(target, &file.path).await)?;
    let text = format!("{}\n<!-- gate -->\n", before.clone().unwrap_or_default());
    let plan = cfg(
        label,
        svc.plan_instructions(target, &file.path, &text).await,
    )?;
    cfg(label, svc.apply(&plan).await)?;
    let read_back = cfg(label, svc.read_instructions(target, &file.path).await)?;
    check(
        read_back.as_deref() == Some(text.as_str()),
        format!("{label}: read back differs"),
    )?;
    let same = cfg(
        label,
        svc.plan_instructions(target, &file.path, &text).await,
    )?;
    check(
        same.is_no_op(),
        format!("{label}: same text is not a no-op"),
    )?;
    let restore = before.unwrap_or_default();
    let plan = cfg(
        label,
        svc.plan_instructions(target, &file.path, &restore).await,
    )?;
    cfg(label, svc.apply(&plan).await)?;
    Ok(())
}

// ---- 3./4. conflict + masking --------------------------------------------------------------------------

async fn conflict(svc: &AgentConfigService, env: &Env) -> R<()> {
    let target = ConfigTarget::new(AgentKind::Harness, Scope::User);
    let body = body_for(Concept::Mcp, 7).ok_or("no body")?;
    let plan = cfg("plan", svc.plan_upsert(target, &body, None).await)?;
    let file = env.home.join("mcp.json");
    let external = "{\n  \"mcpServers\": {\"someone-else\": {\"command\": \"x\"}}\n}\n";
    write(&file, external)?;
    match svc.apply(&plan).await {
        Err(ConfigError::Conflict { .. }) => {}
        other => return Err(format!("expected Conflict, got {other:?}")),
    }
    check(
        read(&file) == external,
        "conflicting apply touched the file",
    )?;
    io("cleanup", std::fs::remove_file(&file))?;
    step("conflict");
    Ok(())
}

async fn diff_masking(svc: &AgentConfigService) -> R<()> {
    let codex = ConfigTarget::new(AgentKind::Codex, Scope::User);
    let listed = cfg("codex mcp", svc.list(Concept::Mcp, codex).await)?;
    let mut changes = Vec::new();
    for name in ["local", "remote"] {
        let entry = listed
            .iter()
            .find(|e| e.name == name)
            .ok_or(format!("codex server {name} missing"))?;
        let EntryBody::Mcp(mut server) = entry.body.clone() else {
            return Err("not an MCP entry".into());
        };
        server.args.push("--changed".into());
        server.url = server.url.map(|u| format!("{u}/v2"));
        changes.push(cfg(
            "plan",
            svc.plan_upsert(codex, &EntryBody::Mcp(server), Some(name))
                .await,
        )?);
    }
    let claude = ConfigTarget::new(AgentKind::ClaudeCode, Scope::User);
    let body = body_for(Concept::Mcp, 3).ok_or("no body")?;
    changes.push(cfg("plan", svc.plan_upsert(claude, &body, None).await)?);
    for plan in &changes {
        let mut shown = plan.diff();
        for change in &plan.changes {
            shown.push_str(change.before_preview.as_deref().unwrap_or_default());
            shown.push_str(change.after_preview.as_deref().unwrap_or_default());
            shown.push_str(&format!("{change:?}")[..]);
        }
        check(!shown.trim().is_empty(), "empty preview")?;
        for secret in SECRETS {
            check(
                !shown.contains(secret),
                "a secret appears in a diff or preview",
            )?;
        }
    }
    step("diff_masking");
    Ok(())
}

async fn auth(svc: &AgentConfigService) -> R<()> {
    for agent in AgentKind::CLIS {
        let state = svc.auth_state(agent).await;
        check(
            state == AuthState::Present,
            format!("{agent:?} auth state {state:?}, expected Present (fixture file exists)"),
        )?;
    }
    step("auth_presence");
    Ok(())
}

// ---- 5. overlay ----------------------------------------------------------------------------------------

fn overlay_server(name: &str, transport: McpTransport) -> EntryBody {
    EntryBody::Mcp(McpServer {
        name: name.into(),
        transport,
        command: (transport == McpTransport::Stdio).then(|| "node".to_owned()),
        args: if transport == McpTransport::Stdio {
            vec![format!("{name}.js")]
        } else {
            Vec::new()
        },
        url: (transport != McpTransport::Stdio).then(|| "https://example.invalid/mcp".to_owned()),
        ..McpServer::default()
    })
}

/// Names of the `mcpServers` of every logged `new` of `agent`, in order.
fn logged_news(env: &Env, agent: &str) -> Vec<(String, Vec<String>)> {
    env.log_entries()
        .into_iter()
        .filter(|e| e["event"] == "new" && e["agent"] == agent)
        .map(|e| {
            let names = e["mcpServers"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s["name"].as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            (
                e["sessionId"].as_str().unwrap_or_default().to_owned(),
                names,
            )
        })
        .collect()
}

async fn overlay_session_new(env: &Env, svc: &AgentConfigService) -> R<SessionId> {
    let target = ConfigTarget::new(AgentKind::Harness, Scope::User);
    for body in [
        overlay_server("gate-stdio", McpTransport::Stdio),
        overlay_server("gate-http", McpTransport::Http),
    ] {
        let plan = cfg("overlay", svc.plan_upsert(target, &body, None).await)?;
        cfg("overlay", svc.apply(&plan).await)?;
    }
    let gate = start(hub_config(env, &env.home, false)).await?;
    let s1 = gate.session(env, A, "phase9").await?;
    let t = gate.send(&s1, "hello overlay", None).await?;
    gate.completed(&s1, &t).await?;
    let news = logged_news(env, A);
    let (_, names) = news.last().ok_or("no session/new logged")?;
    check(
        names == &["harness-memory", "harness-delegate", "gate-stdio"],
        format!("session/new mcpServers {names:?}, expected harness-memory + harness-delegate + gate-stdio"),
    )?;
    // Edit while the native session exists: the running native session keeps what it got.
    let plan = cfg(
        "overlay",
        svc.plan_upsert(
            target,
            &overlay_server("gate-late", McpTransport::Stdio),
            None,
        )
        .await,
    )?;
    cfg("overlay", svc.apply(&plan).await)?;
    let t = gate.send(&s1, "second turn", None).await?;
    gate.completed(&s1, &t).await?;
    check(
        logged_news(env, A).len() == news.len(),
        "an overlay edit re-created the running native session",
    )?;
    let s2 = gate.session(env, A, "phase9-b").await?;
    let t = gate.send(&s2, "hello again", None).await?;
    gate.completed(&s2, &t).await?;
    let (_, names) = logged_news(env, A)
        .last()
        .cloned()
        .ok_or("no session/new logged")?;
    check(
        names.iter().any(|n| n == "gate-late"),
        format!("next session/new did not see the overlay edit: {names:?}"),
    )?;
    gate.hub.shutdown().await.map_err(|e| e.to_string())?;
    step("overlay_session_new");
    Ok(s1)
}

// ---- 6. modes / models / options -----------------------------------------------------------------------

fn sets_logged(env: &Env, key: &str, value: &str) -> usize {
    env.log_entries()
        .iter()
        .filter(|e| e["event"] == "set" && e["key"] == key && e["value"] == value)
        .count()
}

async fn options_text(gate: &Gate, s: &SessionId, prefix: &str) -> R<String> {
    let t = gate.send(s, &format!("{prefix}[[options]]"), None).await?;
    Ok(gate.completed(s, &t).await?.assistant_text)
}

/// One continuous scenario: splitting it would hide the ordering the steps depend on.
#[allow(clippy::too_many_lines)]
async fn modes_models(env: &Env, s1: &SessionId) -> R<()> {
    let gate = start(hub_config(env, &env.home, true)).await?;
    let hub = &gate.hub;
    let e = |e: ApiError| e.to_string();
    let modes = hub.list_modes(s1.clone()).await.map_err(e)?;
    check(
        modes.known && modes.available.len() == 3 && modes.current.as_deref() == Some("default"),
        format!("modes after first turn: {modes:?}"),
    )?;
    let model_list = hub.list_models(s1.clone()).await.map_err(e)?;
    check(
        model_list.known && model_list.available.len() == 2,
        format!("models: {model_list:?}"),
    )?;
    let options = hub.list_config_options(s1.clone()).await.map_err(e)?;
    check(
        options.known && options.options.iter().any(|o| o.id == "effort"),
        format!("config options: {options:?}"),
    )?;
    hub.set_mode(s1.clone(), "plan".into()).await.map_err(e)?;
    hub.set_model(s1.clone(), "fake-large".into())
        .await
        .map_err(e)?;
    hub.set_config_option(s1.clone(), "effort".into(), "high".into())
        .await
        .map_err(e)?;
    for bad in [
        hub.set_mode(s1.clone(), "bogus".into()).await,
        hub.set_model(s1.clone(), "bogus".into()).await,
        hub.set_config_option(s1.clone(), "effort".into(), "max".into())
            .await,
    ] {
        check(
            matches!(bad, Err(ApiError::InvalidArgument(_))),
            format!("invalid id accepted: {bad:?}"),
        )?;
    }
    check(
        gate.event_index(|k| matches!(k, EventKind::SessionOptionsChanged { .. }))
            .is_some(),
        "no SessionOptionsChanged event",
    )?;
    check(
        hub.list_modes(s1.clone())
            .await
            .map_err(e)?
            .current
            .as_deref()
            == Some("plan"),
        "list_modes does not show the choice",
    )?;
    let text = options_text(&gate, s1, "").await?;
    check(
        text.contains("options=mode:plan,model:fake-large,effort:high"),
        format!("choices not applied before the prompt: {text}"),
    )?;
    check(
        sets_logged(env, "mode", "plan") >= 1 && sets_logged(env, "effort", "high") >= 1,
        "set calls not logged by the agent",
    )?;
    // A choice made while a turn runs applies only to the next turn.
    let t = gate.send(s1, "[[slow 20]] [[options]]", None).await?;
    gate.wait_turn(
        s1,
        &t,
        |v| v.assistant_text.contains("chunk-1"),
        "slow turn streaming",
    )
    .await?;
    hub.set_mode(s1.clone(), "acceptEdits".into())
        .await
        .map_err(e)?;
    let running = gate.completed(s1, &t).await?.assistant_text;
    check(
        running.contains("options=mode:plan,"),
        format!("running turn was changed: {running}"),
    )?;
    let text = options_text(&gate, s1, "").await?;
    check(
        text.contains("options=mode:acceptEdits,"),
        format!("choice not applied to the next turn: {text}"),
    )?;
    // Agent defaults (AGT-007) on a fresh native session.
    let sb = gate.session(env, B, "defaults").await?;
    let text = options_text(&gate, &sb, "").await?;
    check(
        text.contains("model:fake-large"),
        format!("agent default not applied: {text}"),
    )?;
    let agent_options = hub.agent_options(AgentId::new(B)).await.map_err(e)?;
    check(
        agent_options.models.current.as_deref() == Some("fake-large") && agent_options.modes.known,
        format!("agent_options: {agent_options:?}"),
    )?;
    hub.shutdown().await.map_err(e)?;
    // Persisted across a hub restart (new agent process → session/load → choices re-applied).
    let gate = start(hub_config(env, &env.home, false)).await?;
    check(
        gate.hub
            .list_modes(s1.clone())
            .await
            .map_err(e)?
            .current
            .as_deref()
            == Some("acceptEdits"),
        "mode choice lost across restart",
    )?;
    let text = options_text(&gate, s1, "").await?;
    check(
        text.contains("options=mode:acceptEdits,model:fake-large,effort:high"),
        format!("choices not re-applied after restart: {text}"),
    )?;
    gate.hub.shutdown().await.map_err(e)?;
    step("modes_models");
    Ok(())
}

// ---- 7. relocation -------------------------------------------------------------------------------------

async fn relocation_step(env: &Env, layout: &Layout, s1: &SessionId) -> R<()> {
    let base = env.home.clone();
    let target = layout.home.join("harness-moved");
    // Interrupted after the copy: no pointer, the old dir stays in use, the retry succeeds.
    let interrupted =
        relocation::relocate_with_fail_point(&base, &base, &target, Some(FailPoint::AfterCopy))
            .await;
    check(interrupted.is_err(), "fail point did not interrupt")?;
    let info = relocation::resolve_data_dir(&base).await;
    check(
        info.data_dir == base && !info.relocated,
        "interrupted relocation switched the data dir",
    )?;
    relocation::schedule(&base, &target)
        .await
        .map_err(|e| format!("schedule: {e}"))?;
    let report = relocation::apply_pending(&base)
        .await
        .map_err(|e| format!("relocate: {e}"))?
        .ok_or("no pending relocation applied")?;
    check(report.files_copied > 0, "nothing copied")?;
    let info = relocation::resolve_data_dir(&base).await;
    check(
        info.relocated && info.data_dir == relocation_abs(&target),
        format!("data dir is {}", info.data_dir.display()),
    )?;
    println!("NOTE relocated to {}", info.data_dir.display());
    let gate = start(hub_config(env, &info.data_dir, false)).await?;
    let sessions = gate
        .hub
        .list_sessions(ListSessions::default())
        .await
        .map_err(|e| e.to_string())?;
    check(
        sessions.iter().any(|s| &s.id == s1),
        "session missing after relocation",
    )?;
    let detail = gate
        .hub
        .open_session(s1.clone())
        .await
        .map_err(|e| e.to_string())?;
    check(detail.turns.len() >= 4, "turns missing after relocation")?;
    let text = options_text(&gate, s1, "").await?;
    check(
        text.contains("options=mode:acceptEdits"),
        "choices lost after relocation",
    )?;
    gate.hub.shutdown().await.map_err(|e| e.to_string())?;
    let store = SettingsStore::open(&info.data_dir)
        .await
        .map_err(|e| format!("settings after relocation: {e}"))?;
    check(
        store.snapshot().appearance.theme == ThemePref::Light,
        "settings lost after relocation",
    )?;
    step("relocation");
    Ok(())
}

fn relocation_abs(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}
