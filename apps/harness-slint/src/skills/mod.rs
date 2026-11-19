//! Skill marketplace (spec `docs/specs/harness/skill-marketplace.md`): fetches the google/skills catalog,
//! caches it under `<data-dir>/skills-catalog/`, compares it with what each agent has on disk, and asks the
//! settings worker for a write plan. This module never writes an agent config file: it only projects rows into
//! `SkillsUi` and sends `Req::Plan` / `Req::SkillsInstalled`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use harness_agent_config::skill_catalog::{self, CatalogSkill, TreeEntry};
use harness_agent_config::skill_health::{self, Grade, Report};
use slint::{ComponentHandle, ModelRc, VecModel};
use tokio::sync::mpsc::UnboundedSender;

use crate::settings::{PlanOp, Req};
use crate::ui::{AppWindow, SkillRow, SkillsUi};

const INDEX_URL: &str = "https://raw.githubusercontent.com/google/skills/main/index.json";
const TREE_URL: &str = "https://api.github.com/repos/google/skills/git/trees/main?recursive=1";
const RAW_ROOT: &str = "https://raw.githubusercontent.com/google/skills/main/";
const MAX_ROWS: usize = 24;
const REFRESH_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

#[allow(clippy::struct_excessive_bools)] //独立的开关各自表示一种页面状态
struct State {
    window: slint::Weak<AppWindow>,
    req: UnboundedSender<Req>,
    cache: PathBuf,
    catalog: Vec<CatalogSkill>,
    tree: Vec<TreeEntry>,
    installed: HashMap<String, Vec<String>>,
    meta: HashMap<String, Vec<(String, String)>>,
    last_install: Option<(String, Vec<(String, String)>)>,
    /// Static health per installed skill name (spec `skill-health.md`); empty until checked.
    health: HashMap<String, (String, String)>,
    health_grade: String,
    health_score: u32,
    checking: bool,
    /// Command the user can copy when the live check is on (spec `skill-health.md` §Live check).
    live_command: String,
    query: String,
    busy: String,
    seq: u64,
    health_seq: u64,
    fetching: bool,
    last: Option<Instant>,
    status: String,
    status_error: bool,
    available: bool,
    started: bool,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// Read-only projection for the phone settings page (spec skill-marketplace.md §3.4): `(skill, agents)`.
static SUMMARY: std::sync::RwLock<Vec<(String, String)>> = std::sync::RwLock::new(Vec::new());

#[must_use]
pub fn summary() -> Vec<(String, String)> {
    SUMMARY.read().map(|s| s.clone()).unwrap_or_default()
}

pub fn install(window: &AppWindow, req: UnboundedSender<Req>, data_dir: &std::path::Path) {
    STATE.with(|cell| {
        *cell.borrow_mut() = Some(State {
            window: window.as_weak(),
            req,
            cache: data_dir.join("skills-catalog"),
            catalog: Vec::new(),
            tree: Vec::new(),
            installed: HashMap::new(),
            meta: HashMap::new(),
            last_install: None,
            health: HashMap::new(),
            health_grade: String::new(),
            health_score: 0,
            checking: false,
            live_command: String::new(),
            query: String::new(),
            busy: String::new(),
            seq: 0,
            health_seq: 0,
            fetching: false,
            last: None,
            status: String::new(),
            status_error: false,
            available: true,
            started: false,
        });
    });
    let ui = window.global::<SkillsUi>();
    ui.on_search(|q| search(&q));
    ui.on_refresh(|| refresh(true));
    ui.on_install(|name| install_skill(&name));
    ui.on_update(|name| install_skill(&name));
    ui.on_remove(|name| remove_skill(&name));
    ui.on_check_health(check_health);
}

/// `Check skills`: asks the settings worker for a fresh health report. Never starts skill-reach.
fn check_health() {
    let seq = STATE.with(|cell| {
        let mut cell = cell.borrow_mut();
        let s = cell.as_mut()?;
        s.health_seq += 1;
        s.checking = true;
        Some((s.health_seq, s.req.clone()))
    });
    let Some((seq, req)) = seq else {
        return;
    };
    push();
    let _ = req.send(Req::SkillsHealth { seq });
}

/// Report arrived from the settings worker; `live` is the copyable command when the opt-in is on.
pub fn health_loaded(seq: u64, report: Result<Report, String>, live: Option<String>) {
    let _ = slint::invoke_from_event_loop(move || {
        STATE.with(|cell| {
            let mut cell = cell.borrow_mut();
            let Some(s) = cell.as_mut() else {
                return;
            };
            if s.health_seq != seq {
                return;
            }
            s.checking = false;
            s.live_command = live.unwrap_or_default();
            match report {
                Ok(report) => {
                    s.health = report
                        .skills
                        .iter()
                        .map(|skill| {
                            (
                                skill.name.clone(),
                                (grade_label(skill.grade()).to_string(), detail(skill)),
                            )
                        })
                        .collect();
                    s.health_grade = grade_label(report.grade()).to_string();
                    s.health_score = report.score();
                    // The status line carries errors only; the header badge shows the grade (translated in the UI).
                    s.status = String::new();
                    s.status_error = false;
                }
                Err(error) => {
                    s.health.clear();
                    s.health_grade = String::new();
                    s.status = format!("Skill check failed: {error}");
                    s.status_error = true;
                }
            }
        });
        push();
    });
}

/// Grade as a stable code the UI translates (`good` / `warning` / `poor`).
fn grade_label(grade: Grade) -> &'static str {
    match grade {
        Grade::Good => "good",
        Grade::Warning => "warning",
        Grade::Poor => "poor",
    }
}

/// One line per finding, so the row tooltip says what to fix.
fn detail(skill: &skill_health::SkillHealth) -> String {
    if skill.findings.is_empty() {
        return format!(
            "Score {}: the description routes without conflicts",
            skill.score()
        );
    }
    let mut lines: Vec<String> = skill.findings.iter().map(|f| f.message.clone()).collect();
    lines.push(format!("Score {}", skill.score()));
    lines.join("\n")
}

pub fn open() {
    let job = STATE.with(|cell| {
        let mut cell = cell.borrow_mut();
        let s = cell.as_mut()?;
        s.seq += 1;
        let seq = s.seq;
        let due = s.last.map_or(true, |t| t.elapsed() >= REFRESH_AFTER);
        let first = !s.started;
        s.started = true;
        Some((seq, due, s.cache.clone(), first))
    });
    let Some((seq, due, cache, first)) = job else {
        return;
    };
    loaded(seq);
    load_cache();
    push();
    if due || first {
        spawn_revalidate(cache);
    }
}

/// Asks the worker for the installed set; the answer arrives through [`installed_loaded`].
fn loaded(seq: u64) {
    let _ = STATE.with(|cell| {
        cell.borrow()
            .as_ref()
            .map(|s| s.req.send(Req::SkillsInstalled { seq }))
    });
}

fn search(query: &str) {
    STATE.with(|cell| {
        if let Some(s) = cell.borrow_mut().as_mut() {
            s.query = query.to_string();
        }
    });
    push();
}

pub fn refresh(force: bool) {
    let job = STATE.with(|cell| {
        let mut cell = cell.borrow_mut();
        let s = cell.as_mut()?;
        if !force && s.last.is_some_and(|t| t.elapsed() < REFRESH_AFTER) {
            return None;
        }
        s.seq += 1;
        Some((s.seq, s.cache.clone()))
    });
    let Some((seq, cache)) = job else {
        return;
    };
    loaded(seq);
    spawn_revalidate(cache);
}

fn install_skill(name: &str) {
    let job = STATE.with(|cell| {
        let mut cell = cell.borrow_mut();
        let s = cell.as_mut()?;
        let entry = s.catalog.iter().find(|c| c.name == name)?.clone();
        s.busy = name.to_string();
        Some((entry, s.req.clone()))
    });
    let Some((entry, req)) = job else {
        return;
    };
    push();
    spawn_download(req, entry);
}

fn remove_skill(name: &str) {
    let job = STATE.with(|cell| {
        let mut cell = cell.borrow_mut();
        let s = cell.as_mut()?;
        let installed = s.installed.values().any(|n| n.iter().any(|e| e == name));
        if !installed {
            return None;
        }
        s.busy = name.to_string();
        s.last_install = None;
        Some(s.req.clone())
    });
    if let Some(req) = job {
        push();
        let _ = req.send(Req::Plan {
            title: "remove skill",
            op: PlanOp::RemoveCatalog {
                name: name.to_string(),
            },
        });
    }
}

pub fn installed_loaded(seq: u64, installed: Vec<(String, Vec<String>)>) {
    let _ = slint::invoke_from_event_loop(move || {
        STATE.with(|cell| {
            let mut cell = cell.borrow_mut();
            let Some(s) = cell.as_mut() else {
                return;
            };
            if s.seq != seq {
                return;
            }
            s.installed = installed.into_iter().collect();
        });
        push();
    });
}

pub fn plan_finished(error: Option<&str>) {
    let error = error.map(str::to_string);
    STATE.with(|cell| {
        let mut cell = cell.borrow_mut();
        let Some(s) = cell.as_mut() else {
            return;
        };
        s.busy = String::new();
        if let Some(text) = error {
            s.status = text;
            s.status_error = true;
        } else {
            s.status = String::new();
            s.status_error = false;
            if let Some((name, shas)) = s.last_install.take() {
                s.meta.insert(name, shas);
            }
        }
    });
    save_meta();
    let seq = STATE.with(|cell| {
        cell.borrow_mut().as_ref().map(|s| {
            let seq = s.seq + 1;
            (seq, s.req.clone())
        })
    });
    if let Some((seq, req)) = seq {
        STATE.with(|cell| {
            if let Some(s) = cell.borrow_mut().as_mut() {
                s.seq = seq;
            }
        });
        let _ = req.send(Req::SkillsInstalled { seq });
    }
    push();
}

pub fn cancelled() {
    STATE.with(|cell| {
        if let Some(s) = cell.borrow_mut().as_mut() {
            s.busy = String::new();
            s.last_install = None;
        }
    });
    push();
}

fn publish_summary(s: &State) {
    let mut out: Vec<(String, String)> = Vec::new();
    for (agent, names) in &s.installed {
        for name in names {
            match out.iter_mut().find(|(n, _)| n == name) {
                Some((_, agents)) => {
                    agents.push_str(", ");
                    agents.push_str(agent);
                }
                None => out.push((name.clone(), agent.clone())),
            }
        }
    }
    out.sort();
    if let Ok(mut slot) = SUMMARY.write() {
        *slot = out;
    }
}

fn push() {
    let Some(window) = STATE.with(|cell| cell.borrow().as_ref().and_then(|s| s.window.upgrade()))
    else {
        return;
    };
    STATE.with(|cell| {
        let cell = cell.borrow();
        let Some(s) = cell.as_ref() else {
            return;
        };
        let ui = window.global::<SkillsUi>();
        let query = s.query.to_lowercase();
        let mut rows = Vec::new();
        let mut hidden = 0usize;
        for entry in &s.catalog {
            let hay = format!("{} {}", entry.name, entry.description).to_lowercase();
            if !query.is_empty() && !hay.contains(&query) {
                continue;
            }
            if rows.len() >= MAX_ROWS {
                hidden += 1;
                continue;
            }
            let installed = s
                .installed
                .values()
                .any(|names| names.iter().any(|n| n == &entry.name));
            rows.push(SkillRow {
                name: entry.name.clone().into(),
                description: entry.description.clone().into(),
                group: entry.group.clone().into(),
                installed,
                update: installed && stale(s, entry),
                busy: s.busy == entry.name,
                health: s
                    .health
                    .get(&entry.name)
                    .map(|(grade, _)| grade.clone())
                    .unwrap_or_default()
                    .into(),
                health_detail: s
                    .health
                    .get(&entry.name)
                    .map(|(_, detail)| detail.clone())
                    .unwrap_or_default()
                    .into(),
            });
        }
        // Installed skills that are not in the catalog (their own folder, a Harness overlay) still need a row so
        // their health badge is reachable; they have no group, so Install/Update stay hidden and Remove works.
        let known: Vec<String> = s.catalog.iter().map(|e| e.name.clone()).collect();
        let mut extra: Vec<&String> = s
            .installed
            .values()
            .flatten()
            .filter(|name| !known.iter().any(|k| k == *name))
            .collect();
        extra.sort();
        extra.dedup();
        for name in extra {
            if !query.is_empty() && !name.to_lowercase().contains(&query) {
                continue;
            }
            if rows.len() >= MAX_ROWS {
                hidden += 1;
                continue;
            }
            rows.push(local_row(s, name));
        }
        ui.set_rows(ModelRc::new(VecModel::from(rows)));
        ui.set_hidden(i32::try_from(hidden).unwrap_or(i32::MAX));
        ui.set_loading(!s.busy.is_empty() || (s.fetching && s.catalog.is_empty()));
        ui.set_catalog_available(s.available);
        ui.set_status(s.status.clone().into());
        ui.set_status_error(s.status_error);
        ui.set_health_grade(s.health_grade.clone().into());
        ui.set_health_score(i32::try_from(s.health_score).unwrap_or(100));
        ui.set_checking(s.checking);
        ui.set_live_command(s.live_command.clone().into());
        if ui.get_query() != s.query.as_str() {
            ui.set_query(s.query.clone().into());
        }
        publish_summary(s);
    });
}

/// Row for an installed skill that the catalog does not list (no group, no update check).
fn local_row(s: &State, name: &str) -> SkillRow {
    let clients = s
        .installed
        .iter()
        .filter(|(_, names)| names.iter().any(|n| n == name))
        .map(|(agent, _)| agent.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let (health, detail) = s
        .health
        .get(name)
        .map_or((String::new(), String::new()), |(g, d)| {
            (g.clone(), d.clone())
        });
    SkillRow {
        name: name.into(),
        description: clients.into(),
        group: "".into(),
        installed: true,
        update: false,
        busy: s.busy == name,
        health: health.into(),
        health_detail: detail.into(),
    }
}

fn stale(s: &State, entry: &CatalogSkill) -> bool {
    let Some(recorded) = s.meta.get(&entry.name) else {
        return false;
    };
    let mut current = skill_catalog::skill_files(&s.tree, &entry.group, &entry.name);
    let mut recorded = recorded.clone();
    current.sort();
    recorded.sort();
    current != recorded
}

// ---- cache ---------------------------------------------------------------------------------------------------

fn cache_file(cache: &std::path::Path, name: &str) -> PathBuf {
    cache.join(name)
}

fn load_cache() {
    STATE.with(|cell| {
        let mut cell = cell.borrow_mut();
        let Some(s) = cell.as_mut() else {
            return;
        };
        let cache = s.cache.clone();
        if let Ok(bytes) = std::fs::read(cache_file(&cache, "index.json")) {
            if let Ok(catalog) = skill_catalog::parse_catalog(&bytes) {
                s.catalog = catalog.skills;
                s.available = true;
            }
        }
        if let Ok(bytes) = std::fs::read(cache_file(&cache, "tree.json")) {
            if let Ok(tree) = skill_catalog::parse_tree(&bytes) {
                s.tree = tree;
            }
        }
        if let Ok(text) = std::fs::read_to_string(cache_file(&cache, "installed.json")) {
            if let Ok(map) = serde_json::from_str::<HashMap<String, Vec<(String, String)>>>(&text) {
                s.meta = map;
            }
        }
    });
}

fn save_meta() {
    let (cache, meta) = STATE.with(|cell| {
        let cell = cell.borrow();
        let Some(s) = cell.as_ref() else {
            return (PathBuf::new(), HashMap::new());
        };
        (s.cache.clone(), s.meta.clone())
    });
    if cache.as_os_str().is_empty() {
        return;
    }
    let _ = std::fs::create_dir_all(&cache);
    if let Ok(text) = serde_json::to_string(&meta) {
        let _ = std::fs::write(cache_file(&cache, "installed.json"), text);
    }
}

fn client() -> Option<reqwest::Client> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("harness/", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()
}

fn spawn_revalidate(cache: PathBuf) {
    STATE.with(|cell| {
        if let Some(s) = cell.borrow_mut().as_mut() {
            s.fetching = true;
        }
    });
    push();
    std::thread::spawn(move || {
        let result = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt.block_on(fetch_catalog(cache)),
            Err(error) => Err(format!("runtime: {error}")),
        };
        let _ = slint::invoke_from_event_loop(move || {
            let failed = result.is_err();
            load_cache();
            STATE.with(|cell| {
                let mut cell = cell.borrow_mut();
                let Some(s) = cell.as_mut() else {
                    return;
                };
                s.fetching = false;
                s.last = Some(Instant::now());
                if failed {
                    s.available = !s.catalog.is_empty();
                    s.status = if s.available {
                        String::new()
                    } else {
                        "The skill catalog cannot be loaded".to_string()
                    };
                    s.status_error = !s.available;
                }
            });
            push();
        });
    });
}

async fn fetch_catalog(cache: PathBuf) -> Result<(), String> {
    let client = client().ok_or_else(|| "cannot build the HTTP client".to_string())?;
    let etag_path = cache_file(&cache, "index.etag");
    let etag = std::fs::read_to_string(&etag_path).ok();
    let mut request = client.get(INDEX_URL);
    if let Some(tag) = etag.as_deref() {
        request = request.header(reqwest::header::IF_NONE_MATCH, tag);
    }
    let response = request.send().await.map_err(|e| e.to_string())?;
    if response.status() != reqwest::StatusCode::NOT_MODIFIED {
        if !response.status().is_success() {
            return Err(format!("catalog index: HTTP {}", response.status()));
        }
        let tag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let bytes = response.bytes().await.map_err(|e| e.to_string())?;
        skill_catalog::parse_catalog(&bytes).map_err(|e| e.to_string())?;
        let _ = std::fs::create_dir_all(&cache);
        let _ = std::fs::write(cache_file(&cache, "index.json"), &bytes);
        if let Some(tag) = tag {
            let _ = std::fs::write(&etag_path, tag);
        }
    }
    let tree = client
        .get(TREE_URL)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !tree.status().is_success() {
        return Err(format!("file tree: HTTP {}", tree.status()));
    }
    let bytes = tree.bytes().await.map_err(|e| e.to_string())?;
    skill_catalog::parse_tree(&bytes).map_err(|e| e.to_string())?;
    let _ = std::fs::create_dir_all(&cache);
    let _ = std::fs::write(cache_file(&cache, "tree.json"), &bytes);
    Ok(())
}

fn spawn_download(req: UnboundedSender<Req>, entry: CatalogSkill) {
    let (tree, group) = STATE.with(|cell| {
        let cell = cell.borrow();
        let Some(s) = cell.as_ref() else {
            return (Vec::new(), entry.group.clone());
        };
        (s.tree.clone(), entry.group.clone())
    });
    std::thread::spawn(move || {
        let name = entry.name.clone();
        let result = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt.block_on(download(&entry, &tree, &group)),
            Err(error) => Err(format!("runtime: {error}")),
        };
        match result {
            Ok((skill, files, shas)) => {
                let _ = slint::invoke_from_event_loop(move || {
                    STATE.with(|cell| {
                        if let Some(s) = cell.borrow_mut().as_mut() {
                            s.last_install = Some((name, shas));
                        }
                    });
                });
                let _ = req.send(Req::Plan {
                    title: "install skill",
                    op: PlanOp::InstallCatalog {
                        name: entry.name,
                        skill: Box::new(skill),
                        extra: files,
                    },
                });
            }
            Err(error) => {
                let _ = slint::invoke_from_event_loop(move || {
                    STATE.with(|cell| {
                        let mut cell = cell.borrow_mut();
                        if let Some(s) = cell.as_mut() {
                            s.busy = String::new();
                            s.status = error;
                            s.status_error = true;
                        }
                    });
                    push();
                });
            }
        }
    });
}

async fn download(
    entry: &CatalogSkill,
    tree: &[TreeEntry],
    group: &str,
) -> Result<
    (
        harness_agent_config::Skill,
        Vec<(String, String)>,
        Vec<(String, String)>,
    ),
    String,
> {
    let client = client().ok_or_else(|| "cannot build the HTTP client".to_string())?;
    let text = get_text(&client, &entry.entrypoint).await?;
    let skill = skill_catalog::parse_skill_md(&entry.name, &text);
    let listed = skill_catalog::skill_files(tree, group, &entry.name);
    let mut files = Vec::new();
    for (rel, _sha) in &listed {
        let encoded = rel
            .split('/')
            .map(percent_encode)
            .collect::<Vec<_>>()
            .join("/");
        let url = format!("{RAW_ROOT}skills/{group}/{}/{encoded}", entry.name);
        if let Ok(text) = get_text(&client, &url).await {
            files.push((rel.clone(), text));
        }
    }
    Ok((skill, files, listed))
}

async fn get_text(client: &reqwest::Client, url: &str) -> Result<String, String> {
    let response = client.get(url).send().await.map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {} for {url}", response.status()));
    }
    response.text().await.map_err(|e| e.to_string())
}

fn percent_encode(segment: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::new();
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(HEX[usize::from(byte >> 4)] as char);
            out.push(HEX[usize::from(byte & 0x0f)] as char);
        }
    }
    out
}
