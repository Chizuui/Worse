//! Settings › Memory › Graph: the project's Graphify knowledge graph (`harness_agent_hub::graphify`).
//! Finds the tool, builds or refreshes the graph with `graphify update <project>` (code only: no model, no
//! API key), opens the graph and its report, and holds the switch that gives the graph to agents.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use harness_agent_hub::{graphify, recall};
use slint::ComponentHandle;

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
    cli: Option<PathBuf>,
    mcp: bool,
    installer: &'static str,
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

    let c = ctx.clone();
    ui.on_refresh(move || refresh(&c, None));

    let c = ctx.clone();
    ui.on_build(move || {
        let Some(program) = graphify::cli() else {
            return refresh(&c, Some("graphify was not found.".to_owned()));
        };
        let mut command = Command::new(program);
        command
            .arg("update")
            .arg(&c.workspace)
            .current_dir(&c.workspace)
            .env("GRAPHIFY_NO_TIPS", "1");
        run(
            &c,
            command,
            "The graph is up to date.",
            "graphify update failed",
        );
    });

    let c = ctx.clone();
    ui.on_install(move || {
        let Some((program, args)) = installer_command() else {
            return refresh(
                &c,
                Some(format!(
                    "Neither uv nor pipx was found. Install one of them, then run: {}",
                    graphify::INSTALL_COMMAND
                )),
            );
        };
        let mut command = Command::new(program);
        command.args(args);
        run(&c, command, "Graphify is installed.", "The install failed");
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

/// `uv tool install "graphifyy[mcp]"`, else the same with pipx; `None` when neither is installed.
fn installer_command() -> Option<(PathBuf, [&'static str; 3])> {
    if let Some(uv) = harness_agent_hub::agents::resolve_program("uv") {
        return Some((uv, ["tool", "install", "graphifyy[mcp]"]));
    }
    harness_agent_hub::agents::resolve_program("pipx")
        .map(|pipx| (pipx, ["install", "graphifyy[mcp]", "--force"]))
}

fn installer_name() -> &'static str {
    if harness_agent_hub::agents::resolve_program("uv").is_some() {
        "uv"
    } else if harness_agent_hub::agents::resolve_program("pipx").is_some() {
        "pipx"
    } else {
        ""
    }
}

/// "1,204" for 1204.
fn grouped(n: usize) -> String {
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
    let graph_summary = std::fs::read_to_string(&files.graph).ok().map(|text| {
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
        cli: graphify::cli(),
        mcp: graphify::mcp_cli().is_some(),
        installer: installer_name(),
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
            ui.set_installer(state.installer.into());
            ui.set_has_graph(state.summary.is_some());
            ui.set_summary(state.summary.unwrap_or_default().into());
            ui.set_has_html(state.has_html);
            ui.set_has_report(state.has_report);
            ui.set_give_agents(state.give_agents);
            if let Some(note) = note {
                ui.set_note(note.into());
            }
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

/// Runs `command` on a worker thread, showing the last lines of its output while it runs.
fn run(ctx: &Ctx, mut command: Command, done: &'static str, failed: &'static str) {
    if ctx.busy.swap(true, Ordering::SeqCst) {
        return;
    }
    let ctx = ctx.clone();
    spawn(move || {
        show_running(&ctx, true, String::new());
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
        let lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let note = match command.spawn() {
            Err(error) => format!("{failed}: {error}"),
            Ok(mut child) => {
                let mut readers = Vec::new();
                let streams: [Option<Box<dyn std::io::Read + Send>>; 2] = [
                    child.stdout.take().map(|s| Box::new(s) as _),
                    child.stderr.take().map(|s| Box::new(s) as _),
                ];
                for stream in streams.into_iter().flatten() {
                    let lines = Arc::clone(&lines);
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
                    Ok(status) if status.success() => done.to_owned(),
                    Ok(status) => format!(
                        "{failed} ({}). The output is below.",
                        status
                            .code()
                            .map_or_else(|| "stopped".to_owned(), |c| format!("exit code {c}"))
                    ),
                    Err(error) => format!("{failed}: {error}"),
                }
            }
        };
        let tail = lines
            .lock()
            .map(|all| all[all.len().saturating_sub(LOG_LINES)..].join("\n"))
            .unwrap_or_default();
        ctx.busy.store(false, Ordering::SeqCst);
        show_running(&ctx, false, tail);
        refresh(&ctx, Some(note));
    });
}

/// Opens a file with the system's default program (the graph page in the browser, the report in an editor).
fn open_path(path: &Path) {
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
    use super::{grouped, summary};
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
}
