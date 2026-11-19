//! Settings › Memory › Graph: the project's Graphify knowledge graph (`harness_agent_hub::graphify`).
//! Finds the tool, builds or refreshes the graph with `graphify update <project>` (code only: no model, no
//! API key), opens the graph and its report, and holds the switch that gives the graph to agents.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use harness_agent_hub::{graphify, recall};
use harness_graph_view::Graph;
use slint::ComponentHandle;

use super::graph_view;
use crate::ui::{AppWindow, GraphUi};

/// Lines of command output kept for the page.
const LOG_LINES: usize = 14;

#[derive(Clone)]
struct Ctx {
    home: PathBuf,
    workspace: PathBuf,
    window: slint::Weak<AppWindow>,
    /// One command at a time.
    busy: Arc<AtomicBool>,
}

/// What the page shows, gathered off the UI thread.
struct State {
    /// The graph for the picture, when there is one.
    graph: Option<Arc<Graph>>,
    cli: Option<PathBuf>,
    mcp: bool,
    plan: String,
    summary: Option<String>,
    has_html: bool,
    has_report: bool,
    give_agents: bool,
}

pub fn install(window: &AppWindow, home: &Path, workspace: &Path) {
    let ctx = Ctx {
        home: home.to_path_buf(),
        workspace: workspace.to_path_buf(),
        window: window.as_weak(),
        busy: Arc::new(AtomicBool::new(false)),
    };
    let ui = window.global::<GraphUi>();
    ui.set_install_command(graphify::INSTALL_COMMAND.into());
    ui.set_project(workspace.to_string_lossy().as_ref().into());
    ui.set_give_agents(recall::Store::new(home).config().graph);
    graph_view::install(window, home, workspace);

    let c = ctx.clone();
    ui.on_refresh(move || refresh(&c, None));

    let c = ctx.clone();
    ui.on_build(move || {
        let workspace = c.workspace.clone();
        run(
            &c,
            vec![Box::new(move || build_command(&workspace))],
            "The graph is up to date.",
            "graphify update failed",
        );
    });

    // "Set up Graphify": install what is missing, then build the graph, in one go.
    let c = ctx.clone();
    ui.on_install(move || {
        let Some(installer) = Installer::find() else {
            return refresh(
                &c,
                Some(format!(
                    "Nothing on this computer can install Graphify. Install uv, then run: {}",
                    graphify::INSTALL_COMMAND
                )),
            );
        };
        let mut steps = installer.steps();
        let workspace = c.workspace.clone();
        steps.push(Box::new(move || build_command(&workspace)));
        run(
            &c,
            steps,
            "Graphify is installed and the graph is built.",
            "Setting up Graphify failed",
        );
    });

    let c = ctx.clone();
    ui.on_open_html(move || open_path(&graphify::paths(&c.workspace).html));
    let c = ctx.clone();
    ui.on_open_report(move || open_path(&graphify::paths(&c.workspace).report));

    let c = ctx;
    ui.on_set_give_agents(move |on| {
        let c = c.clone();
        spawn(move || {
            let store = recall::Store::new(&c.home);
            let mut config = store.config();
            config.graph = on;
            let note = store
                .set_config(config)
                .err()
                .map(|error| format!("Could not save the setting: {error}"));
            refresh(&c, note);
        });
    });
}

fn spawn(work: impl FnOnce() + Send + 'static) {
    let _ = std::thread::Builder::new()
        .name("harness-graph".into())
        .spawn(work);
}

/// One command of a sequence, made when its turn comes: a later step can depend on what an earlier one
/// installed.
type Step = Box<dyn FnOnce() -> Result<Command, String> + Send>;

/// `graphify update <workspace>`: Graphify's code-only pass.
fn build_command(workspace: &Path) -> Result<Command, String> {
    let program = graphify::cli().ok_or(
        "graphify was installed but is not found yet. Restart Harness, then press Build graph.",
    )?;
    let mut command = Command::new(program);
    command
        .arg("update")
        .arg(workspace)
        .current_dir(workspace)
        .env("GRAPHIFY_NO_TIPS", "1");
    Ok(command)
}

/// What can install Graphify on this computer, best first.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Installer {
    /// `uv tool install "graphifyy[mcp]"`.
    Uv,
    /// `pipx install "graphifyy[mcp]"`.
    Pipx(PathBuf),
    /// Windows without uv: `winget install astral-sh.uv`, then as `Uv`. uv brings its own Python, so this
    /// works on a computer without Python.
    WingetThenUv(PathBuf),
    /// `python -m pip install --user "graphifyy[mcp]"`.
    Pip(PathBuf),
}

impl Installer {
    fn find() -> Option<Self> {
        let on_path = harness_agent_hub::agents::resolve_program;
        if graphify::uv().is_some() {
            return Some(Self::Uv);
        }
        if let Some(pipx) = on_path("pipx") {
            return Some(Self::Pipx(pipx));
        }
        if cfg!(windows) {
            if let Some(winget) = on_path("winget") {
                return Some(Self::WingetThenUv(winget));
            }
        }
        ["python3", "python", "py"]
            .into_iter()
            .find_map(on_path)
            .map(Self::Pip)
    }

    /// What it will run, in words, for the page.
    fn plan(&self) -> String {
        match self {
            Self::Uv => graphify::INSTALL_COMMAND.to_owned(),
            Self::Pipx(_) => "pipx install \"graphifyy[mcp]\"".to_owned(),
            Self::WingetThenUv(_) => {
                format!(
                    "winget install astral-sh.uv, then {}",
                    graphify::INSTALL_COMMAND
                )
            }
            Self::Pip(_) => "python -m pip install --user \"graphifyy[mcp]\"".to_owned(),
        }
    }

    fn steps(self) -> Vec<Step> {
        fn with_uv() -> Result<Command, String> {
            let uv = graphify::uv().ok_or(
                "uv was installed but is not found yet. Restart Harness, then press Set up again.",
            )?;
            let mut command = Command::new(uv);
            command.args(["tool", "install", "graphifyy[mcp]"]);
            Ok(command)
        }
        match self {
            Self::Uv => vec![Box::new(with_uv)],
            Self::Pipx(pipx) => vec![Box::new(move || {
                let mut command = Command::new(pipx);
                command.args(["install", "graphifyy[mcp]", "--force"]);
                Ok(command)
            })],
            Self::WingetThenUv(winget) => vec![
                Box::new(move || {
                    let mut command = Command::new(winget);
                    command.args([
                        "install",
                        "--id",
                        "astral-sh.uv",
                        "-e",
                        "--accept-source-agreements",
                        "--accept-package-agreements",
                        "--disable-interactivity",
                    ]);
                    Ok(command)
                }),
                Box::new(with_uv),
            ],
            Self::Pip(python) => vec![Box::new(move || {
                let mut command = Command::new(python);
                command.args(["-m", "pip", "install", "--user", "graphifyy[mcp]"]);
                Ok(command)
            })],
        }
    }
}

/// "1,204" for 1204.
pub(super) fn grouped(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// "1,204 nodes · 3,310 links · 18 communities · updated 2h ago".
fn summary(stats: graphify::Stats, updated: Option<&str>) -> String {
    let mut text = format!(
        "{} nodes · {} links",
        grouped(stats.nodes),
        grouped(stats.links)
    );
    if stats.communities > 0 {
        text.push_str(&format!(" · {} communities", grouped(stats.communities)));
    }
    if let Some(age) = updated {
        text.push_str(&format!(" · updated {age}"));
    }
    text
}

fn gather(ctx: &Ctx) -> State {
    let files = graphify::paths(&ctx.workspace);
    let graph_text = std::fs::read_to_string(&files.graph).ok();
    let graph = graph_text.as_deref().and_then(Graph::parse).map(Arc::new);
    let graph_summary = graph_text.map(|text| {
        let updated = std::fs::metadata(&files.graph)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| recall::age(recall::now_ms(), i64::try_from(d.as_millis()).unwrap_or(0)));
        graphify::stats(&text).map_or_else(
            || "graph.json is there, but it is not a graph this page can read.".to_owned(),
            |stats| summary(stats, updated.as_deref()),
        )
    });
    State {
        graph,
        cli: graphify::cli(),
        mcp: graphify::mcp_cli().is_some(),
        plan: Installer::find().map(|i| i.plan()).unwrap_or_default(),
        summary: graph_summary,
        has_html: files.html.is_file(),
        has_report: files.report.is_file(),
        give_agents: recall::Store::new(&ctx.home).config().graph,
    }
}

/// Looks again and shows it; `note` replaces the status line.
fn refresh(ctx: &Ctx, note: Option<String>) {
    let ctx = ctx.clone();
    spawn(move || {
        let state = gather(&ctx);
        let weak = ctx.window.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(window) = weak.upgrade() else {
                return;
            };
            let ui = window.global::<GraphUi>();
            ui.set_installed(state.cli.is_some());
            ui.set_cli_path(
                state
                    .cli
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default()
                    .into(),
            );
            ui.set_mcp(state.mcp);
            ui.set_install_plan(state.plan.into());
            ui.set_has_graph(state.summary.is_some());
            ui.set_summary(state.summary.unwrap_or_default().into());
            ui.set_has_html(state.has_html);
            ui.set_has_report(state.has_report);
            ui.set_give_agents(state.give_agents);
            if let Some(note) = note {
                ui.set_note(note.into());
            }
            graph_view::show(&window, state.graph);
        });
    });
}

fn show_running(ctx: &Ctx, busy: bool, log: String) {
    let weak = ctx.window.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            let ui = window.global::<GraphUi>();
            ui.set_busy(busy);
            ui.set_log(log.into());
        }
    });
}

/// Runs `steps` one after the other on a worker thread, showing the last lines of their output; stops at the
/// first one that fails.
fn run(ctx: &Ctx, steps: Vec<Step>, done: &'static str, failed: &'static str) {
    if ctx.busy.swap(true, Ordering::SeqCst) {
        return;
    }
    let ctx = ctx.clone();
    spawn(move || {
        show_running(&ctx, true, String::new());
        let lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let mut note = done.to_owned();
        for step in steps {
            let outcome = step().and_then(|command| run_one(&ctx, command, &lines));
            if let Err(reason) = outcome {
                note = format!("{failed}: {reason}");
                break;
            }
        }
        let tail = lines
            .lock()
            .map(|all| all[all.len().saturating_sub(LOG_LINES)..].join("\n"))
            .unwrap_or_default();
        ctx.busy.store(false, Ordering::SeqCst);
        show_running(&ctx, false, tail);
        refresh(&ctx, Some(note));
    });
}

/// Runs one command to its end, feeding its output lines to the page. `Err`: why it did not succeed.
fn run_one(ctx: &Ctx, mut command: Command, lines: &Arc<Mutex<Vec<String>>>) -> Result<(), String> {
    let shown = format!(
        "> {} {}",
        command.get_program().to_string_lossy(),
        command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    );
    if let Ok(mut all) = lines.lock() {
        all.push(shown);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // No console window for the child.
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let mut readers = Vec::new();
    let streams: [Option<Box<dyn std::io::Read + Send>>; 2] = [
        child.stdout.take().map(|s| Box::new(s) as _),
        child.stderr.take().map(|s| Box::new(s) as _),
    ];
    for stream in streams.into_iter().flatten() {
        let lines = Arc::clone(lines);
        let ctx = ctx.clone();
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                let line = line.trim_end().to_owned();
                if line.is_empty() {
                    continue;
                }
                let Ok(mut all) = lines.lock() else {
                    return;
                };
                all.push(line);
                let from = all.len().saturating_sub(LOG_LINES);
                let tail = all[from..].join("\n");
                drop(all);
                show_running(&ctx, true, tail);
            }
        }));
    }
    let status = child.wait();
    for reader in readers {
        let _ = reader.join();
    }
    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(status.code().map_or_else(
            || "the command was stopped. Its output is below.".to_owned(),
            |code| format!("exit code {code}. The output is below."),
        )),
        Err(error) => Err(error.to_string()),
    }
}

/// Opens a file with the system's default program (the graph page in the browser, the report in an editor).
pub(super) fn open_path(path: &Path) {
    if !path.is_file() {
        return;
    }
    let program = if cfg!(windows) {
        "explorer.exe"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = Command::new(program).arg(path).spawn();
}

#[cfg(test)]
mod tests {
    use super::{grouped, summary, Installer};
    use harness_agent_hub::graphify::Stats;

    #[test]
    fn summary_groups_digits_and_skips_what_is_missing() {
        assert_eq!(grouped(7), "7");
        assert_eq!(grouped(1204), "1,204");
        assert_eq!(grouped(1_234_567), "1,234,567");
        let stats = Stats {
            nodes: 1204,
            links: 3310,
            communities: 18,
        };
        assert_eq!(
            summary(stats, Some("2h ago")),
            "1,204 nodes · 3,310 links · 18 communities · updated 2h ago"
        );
        let bare = Stats {
            nodes: 3,
            links: 0,
            communities: 0,
        };
        assert_eq!(summary(bare, None), "3 nodes · 0 links");
    }

    #[test]
    fn each_installer_says_what_it_runs_and_ends_with_graphify() {
        let winget = Installer::WingetThenUv("winget".into());
        assert_eq!(
            winget.plan(),
            "winget install astral-sh.uv, then uv tool install \"graphifyy[mcp]\""
        );
        assert_eq!(winget.steps().len(), 2);
        assert_eq!(Installer::Uv.plan(), "uv tool install \"graphifyy[mcp]\"");
        for installer in [
            Installer::Pipx("pipx".into()),
            Installer::Pip("python".into()),
        ] {
            assert!(installer.plan().contains("graphifyy[mcp]"));
            let mut steps = installer.steps();
            let command = steps.remove(0)().expect("command");
            assert!(command.get_args().any(|a| a == "graphifyy[mcp]"));
        }
    }
}
