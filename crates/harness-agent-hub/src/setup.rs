//! First-run setup: find and install missing ACP adapters (phase-9-parity.md §9.10).
//!
//! Owner: this module owns the table of known adapters, the process-wide adapter search directories (read by
//! [`crate::agents::resolve_program`]) and the single-flight set of running installs. Harness installs only the
//! npm adapter packages, never a CLI. Probing never runs a program; installing runs `npm install -g <package>`
//! only when the user asks for it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use harness_core_api::{
    AdapterInstallResult, AdapterInstallStatus, AgentId, ApiError, ApiResult, Availability,
    EventKind, GhSetup, SetupAgent, SetupState, SetupStatus,
};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::agents::{resolve_program, AgentSpec};
use crate::hub::Hub;

/// An agent CLI Harness knows, with the npm package of its ACP adapter (`None`: the CLI speaks ACP itself).
#[derive(Debug, Clone, Copy)]
pub struct KnownCli {
    /// Launch program of the agent (the key; Claude accounts share the Claude row).
    pub program: &'static str,
    /// CLI command name.
    pub cli: &'static str,
    /// npm package of the ACP adapter.
    pub package: Option<&'static str>,
}

/// Known CLIs (§9.10 table).
pub const KNOWN_CLIS: &[KnownCli] = &[
    KnownCli {
        program: "claude-agent-acp",
        cli: "claude",
        package: Some("@agentclientprotocol/claude-agent-acp"),
    },
    KnownCli {
        program: "codex-acp",
        cli: "codex",
        package: Some("@agentclientprotocol/codex-acp"),
    },
    KnownCli {
        program: "agy",
        cli: "agy",
        package: None,
    },
];

/// Env override of the npm command (gates use a fake npm).
pub const ENV_NPM: &str = "HARNESS_NPM";
/// Shown when npm (Node.js) is missing.
pub const NPM_STEPS: &str =
    "npm was not found. Install Node.js LTS from https://nodejs.org (it includes npm), \
then restart Harness and install the adapters again.";

const INSTALL_TIMEOUT: Duration = Duration::from_secs(600);
const PREFIX_TIMEOUT: Duration = Duration::from_secs(30);
const LINE_MAX: usize = 300;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The known CLI whose launch program is `program` (bare name or path; compared by file stem).
#[must_use]
pub fn known_cli(program: &str) -> Option<&'static KnownCli> {
    let stem = Path::new(program)
        .file_stem()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())?;
    KNOWN_CLIS.iter().find(|k| k.program == stem)
}

fn search_dirs_cell() -> &'static Mutex<Vec<PathBuf>> {
    static DIRS: OnceLock<Mutex<Vec<PathBuf>>> = OnceLock::new();
    DIRS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Directories searched after PATH (npm global bin directories found after an install).
#[must_use]
pub fn extra_search_dirs() -> Vec<PathBuf> {
    search_dirs_cell()
        .lock()
        .map(|d| d.clone())
        .unwrap_or_default()
}

fn add_search_dir(dir: PathBuf) {
    if let Ok(mut dirs) = search_dirs_cell().lock() {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
}

/// Probe of an agent without a resolvable launch program: `AdapterMissing` when the CLI of a known adapter is
/// installed, else `NotInstalled` (never runs a program).
pub(crate) fn probe_missing(spec: &AgentSpec) -> Availability {
    let Some(known) = known_cli(&spec.program) else {
        return Availability::NotInstalled {
            hint: spec.install_hint.clone(),
        };
    };
    let Some(package) = known.package else {
        return Availability::NotInstalled {
            hint: spec.install_hint.clone(),
        };
    };
    match resolve_program(known.cli) {
        Some(cli_path) => Availability::AdapterMissing {
            cli_path: cli_path.to_string_lossy().into_owned(),
            package: package.to_owned(),
            hint: format!(
                "the `{}` CLI is installed but its ACP adapter is not: install {package} (Settings › Agents › Install adapter)",
                known.cli
            ),
        },
        None => Availability::NotInstalled {
            hint: format!(
                "install the `{}` CLI first; Harness then installs its ACP adapter ({package})",
                known.cli
            ),
        },
    }
}

/// The npm command: `HARNESS_NPM` (missing file = no npm), else `npm` on PATH (`npm.cmd` on Windows).
#[must_use]
pub fn npm_command() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os(ENV_NPM).filter(|v| !v.is_empty()) {
        let path = PathBuf::from(value);
        return path.is_file().then_some(path);
    }
    resolve_program("npm")
}

fn installing() -> &'static Mutex<HashSet<String>> {
    static SET: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SET.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Releases the single-flight slot of a package when the install ends (also on early return).
struct InstallSlot(String);

impl InstallSlot {
    fn take(package: &str) -> Option<Self> {
        let mut set = installing().lock().ok()?;
        set.insert(package.to_owned())
            .then(|| Self(package.to_owned()))
    }
}

impl Drop for InstallSlot {
    fn drop(&mut self) {
        if let Ok(mut set) = installing().lock() {
            set.remove(&self.0);
        }
    }
}

fn npm(program: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

fn clip(line: &str) -> String {
    let line = line.trim_end();
    match line.char_indices().nth(LINE_MAX) {
        Some((end, _)) => format!("{}…", &line[..end]),
        None => line.to_owned(),
    }
}

/// `npm prefix -g` → the global bin directory (`<prefix>` on Windows, `<prefix>/bin` elsewhere).
async fn global_bin_dir(program: &Path) -> Option<PathBuf> {
    let output = tokio::time::timeout(PREFIX_TIMEOUT, npm(program, &["prefix", "-g"]).output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let prefix = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())?
        .to_owned();
    let prefix = PathBuf::from(prefix);
    Some(if cfg!(windows) {
        prefix
    } else {
        prefix.join("bin")
    })
}

impl Hub {
    pub(crate) fn do_setup_status(&self) -> SetupStatus {
        let mut agents: Vec<SetupAgent> = Vec::new();
        let mut seen = HashSet::new();
        for spec in &self.ctx.config.agents {
            let Some(known) = known_cli(&spec.program) else {
                continue;
            };
            if !seen.insert(known.program) {
                continue;
            }
            let cli_path = resolve_program(known.cli).map(|p| p.to_string_lossy().into_owned());
            let state = match spec.probe() {
                Availability::Available { .. } => SetupState::Ready,
                Availability::AdapterMissing { .. } => SetupState::AdapterMissing,
                Availability::NotInstalled { .. } => SetupState::CliMissing,
            };
            agents.push(SetupAgent {
                agent: spec.id.clone(),
                display_name: spec.display_name.clone(),
                cli: known.cli.to_owned(),
                cli_path,
                package: known.package.map(str::to_owned),
                state,
            });
        }
        SetupStatus {
            npm_path: npm_command().map(|p| p.to_string_lossy().into_owned()),
            npm_steps: NPM_STEPS.to_owned(),
            agents,
            gh: crate::gh::resolve_gh().map_or_else(GhSetup::default, |(path, bundled)| GhSetup {
                path: Some(path.to_string_lossy().into_owned()),
                bundled,
            }),
        }
    }

    pub(crate) async fn do_install_gh(&self) -> ApiResult<()> {
        let agent = AgentId::new("gh");
        let package = "cli/cli";
        if crate::gh::resolve_gh().is_some() {
            self.install_event(
                &agent,
                package,
                AdapterInstallStatus::Succeeded,
                None,
                Some("GitHub CLI is already installed".to_owned()),
            );
            return Ok(());
        }
        let Some(_guard) = crate::gh::InstallGuard::take() else {
            return Err(ApiError::Busy(
                "GitHub CLI is already being installed".to_owned(),
            ));
        };
        tracing::info!("gh install started");
        let result = crate::gh::install(|line| {
            self.install_event(
                &agent,
                package,
                AdapterInstallStatus::Running,
                Some(line),
                None,
            );
        })
        .await;
        match result {
            Ok(path) => {
                tracing::info!(path = %path.display(), "gh install finished");
                self.install_event(
                    &agent,
                    package,
                    AdapterInstallStatus::Succeeded,
                    None,
                    Some(format!("GitHub CLI installed at {}", path.display())),
                );
            }
            Err(message) => {
                tracing::warn!(%message, "gh install failed");
                self.install_event(
                    &agent,
                    package,
                    AdapterInstallStatus::Failed,
                    None,
                    Some(message),
                );
            }
        }
        if let Ok(mut slot) = self.inbox.gh.lock() {
            *slot = None;
        }
        self.ctx.events.emit(None, EventKind::InboxChanged);
        Ok(())
    }

    fn install_event(
        &self,
        agent: &AgentId,
        package: &str,
        status: AdapterInstallStatus,
        line: Option<String>,
        message: Option<String>,
    ) {
        self.ctx.events.emit(
            None,
            EventKind::AdapterInstall {
                agent: agent.clone(),
                package: package.to_owned(),
                status,
                line,
                message,
            },
        );
    }

    fn install_done(
        &self,
        agent: &AgentId,
        package: &str,
        ok: bool,
        message: String,
    ) -> AdapterInstallResult {
        let status = if ok {
            AdapterInstallStatus::Succeeded
        } else {
            AdapterInstallStatus::Failed
        };
        self.install_event(agent, package, status, None, Some(message.clone()));
        AdapterInstallResult {
            agent: agent.clone(),
            ok,
            message,
        }
    }

    pub(crate) async fn do_install_adapter(
        &self,
        agent: AgentId,
    ) -> ApiResult<AdapterInstallResult> {
        let spec = self
            .ctx
            .specs
            .get(&agent)
            .cloned()
            .ok_or_else(|| ApiError::NotFound(format!("agent {agent}")))?;
        let known = known_cli(&spec.program)
            .filter(|k| k.package.is_some())
            .ok_or_else(|| {
                ApiError::InvalidArgument(format!(
                    "{} has no ACP adapter to install",
                    spec.display_name
                ))
            })?;
        let package = known.package.unwrap_or_default();
        match spec.probe() {
            Availability::Available { .. } => {
                return Ok(AdapterInstallResult {
                    agent,
                    ok: true,
                    message: format!("{package} is already installed"),
                })
            }
            Availability::NotInstalled { .. } => {
                return Err(ApiError::InvalidArgument(format!(
                    "install the `{}` CLI first",
                    known.cli
                )))
            }
            Availability::AdapterMissing { .. } => {}
        }
        let Some(npm_path) = npm_command() else {
            return Ok(self.install_done(&agent, package, false, NPM_STEPS.to_owned()));
        };
        let Some(_slot) = InstallSlot::take(package) else {
            return Err(ApiError::Busy(format!(
                "{package} is already being installed"
            )));
        };
        tracing::info!(%package, "adapter install started");
        let code = self.run_npm_install(&agent, package, &npm_path).await;
        tracing::info!(%package, ?code, "adapter install finished");
        let code = match code {
            Ok(code) => code,
            Err(message) => return Ok(self.install_done(&agent, package, false, message)),
        };
        if code != Some(0) {
            let message = format!(
                "npm install -g {package} failed (exit code {})",
                code.map_or_else(|| "none".to_owned(), |c| c.to_string())
            );
            return Ok(self.install_done(&agent, package, false, message));
        }
        let bin = global_bin_dir(&npm_path).await;
        if let Some(dir) = &bin {
            add_search_dir(dir.clone());
        }
        if spec.probe().is_available() {
            return Ok(self.install_done(&agent, package, true, format!("{package} installed")));
        }
        let place = bin.map_or_else(
            || "the npm global directory".to_owned(),
            |d| d.display().to_string(),
        );
        Ok(self.install_done(
            &agent,
            package,
            false,
            format!(
                "npm installed {package} into {place}, but `{}` is still not found; restart Harness or add that directory to PATH",
                known.program
            ),
        ))
    }

    /// Runs `npm install -g <package>`, streaming every output line as an event. `Ok(exit code)` or a message.
    async fn run_npm_install(
        &self,
        agent: &AgentId,
        package: &str,
        npm_path: &Path,
    ) -> Result<Option<i32>, String> {
        let mut child = npm(npm_path, &["install", "-g", package])
            .spawn()
            .map_err(|e| format!("cannot start npm: {e}"))?;
        let (line_tx, mut line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        if let Some(stdout) = child.stdout.take() {
            let tx = line_tx.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let _ = tx.send(line);
                }
            });
        }
        if let Some(stderr) = child.stderr.take() {
            let tx = line_tx.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let _ = tx.send(line);
                }
            });
        }
        drop(line_tx);
        let run = async {
            while let Some(line) = line_rx.recv().await {
                if line.trim().is_empty() {
                    continue;
                }
                let line = clip(&line);
                tracing::debug!(%package, %line, "npm");
                self.install_event(
                    agent,
                    package,
                    AdapterInstallStatus::Running,
                    Some(line),
                    None,
                );
            }
            child.wait().await
        };
        match tokio::time::timeout(INSTALL_TIMEOUT, run).await {
            Ok(Ok(status)) => Ok(status.code()),
            Ok(Err(error)) => Err(format!("npm failed: {error}")),
            Err(_) => Err(format!(
                "npm install -g {package} did not finish within {} minutes and was stopped",
                INSTALL_TIMEOUT.as_secs() / 60
            )),
        }
    }
}
