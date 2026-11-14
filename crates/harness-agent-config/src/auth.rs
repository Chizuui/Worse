//! Login presence (AGT-003). Credentials files are never opened: only their existence is checked, and gates
//! inject a [`StaticProbe`].

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use harness_settings::fsx;

use crate::paths::ConfigPaths;
use crate::types::{AgentKind, AuthState};

/// Boxed future returned by [`AuthProbe::probe`].
pub type ProbeFuture<'a> = Pin<Box<dyn Future<Output = AuthState> + Send + 'a>>;

/// Answers "is this CLI logged in?" without reading secrets.
pub trait AuthProbe: Send + Sync {
    fn probe<'a>(&'a self, agent: AgentKind, paths: &'a ConfigPaths) -> ProbeFuture<'a>;
}

/// Default probe: credential file exists (metadata only) or the API key env var is set (presence only).
#[derive(Debug, Default, Clone, Copy)]
pub struct FileExistenceProbe;

impl AuthProbe for FileExistenceProbe {
    fn probe<'a>(&'a self, agent: AgentKind, paths: &'a ConfigPaths) -> ProbeFuture<'a> {
        Box::pin(async move {
            let (files, env): (Vec<std::path::PathBuf>, &[&str]) = match agent {
                AgentKind::ClaudeCode => (
                    vec![paths.claude_dir.join(".credentials.json")],
                    &["ANTHROPIC_API_KEY"],
                ),
                AgentKind::Codex => (
                    vec![paths.codex_home.join("auth.json")],
                    &["OPENAI_API_KEY", "CODEX_API_KEY"],
                ),
                AgentKind::Agy => (
                    vec![paths.gemini_dir.join("oauth_creds.json")],
                    &["GEMINI_API_KEY"],
                ),
                AgentKind::Harness => return AuthState::Unknown,
            };
            for file in &files {
                if fsx::exists(file).await {
                    return AuthState::Present;
                }
            }
            if env
                .iter()
                .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()))
            {
                return AuthState::Present;
            }
            // agy 1.5+ 在 Windows 上把登录保存在凭据管理器（target `gemini:antigravity`），不再写 oauth_creds.json。
            if agent == AgentKind::Agy && windows_credential_exists("gemini:antigravity") {
                return AuthState::Present;
            }
            // macOS keeps Claude Code's login in the keychain, which Harness does not query.
            if agent == AgentKind::ClaudeCode && cfg!(target_os = "macos") {
                return AuthState::Unknown;
            }
            AuthState::Absent
        })
    }
}

/// Whether Windows Credential Manager has an entry for `target`. Only `cmdkey /list:<target>` is
/// run: it prints the target name, never the secret.
#[cfg(windows)]
fn windows_credential_exists(target: &str) -> bool {
    use std::os::windows::process::CommandExt;
    /// `CREATE_NO_WINDOW`: no console flash from the GUI process.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new("cmdkey")
        .arg(format!("/list:{target}"))
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .is_ok_and(|out| {
            out.status.success()
                && String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .any(|line| line.trim().ends_with(target))
        })
}

#[cfg(not(windows))]
fn windows_credential_exists(_target: &str) -> bool {
    false
}

/// Fixed answers (tests and gates).
#[derive(Debug, Default, Clone)]
pub struct StaticProbe(pub BTreeMap<AgentKind, AuthState>);

impl AuthProbe for StaticProbe {
    fn probe<'a>(&'a self, agent: AgentKind, _paths: &'a ConfigPaths) -> ProbeFuture<'a> {
        let state = self.0.get(&agent).copied().unwrap_or(AuthState::Unknown);
        Box::pin(async move { state })
    }
}
