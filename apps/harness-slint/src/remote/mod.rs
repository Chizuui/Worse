//! Phone access: the `harness-remote` server plus the tunnel supervisor (docs/plans/remote-contract.md).
//!
//! The server runs on its own thread with a small multi-thread tokio runtime; the UI never awaits it. Commands go
//! through one channel, results land in a [`Snapshot`] that a Slint timer publishes to the "Remote (phone)"
//! settings page. Starting is user-driven: the page toggle starts the server, nothing runs at launch.

pub mod qr;
mod settings_view;
pub mod tunnel;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use harness_core_api::HarnessCore;
use harness_remote::{DeviceInfo, RemoteConfig, RemoteHandle};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use std::rc::Rc;

use crate::ui::{AppWindow, RemoteDevice, RemoteUi};

/// How often the timer republishes the page (ms).
const TICK_MS: u64 = 400;
/// Ticks between two device-list refreshes (~10 s).
const DEVICE_TICKS: u32 = 25;

/// State of the local HTTP/WS server.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum ServerState {
    #[default]
    Off,
    Starting,
    Up {
        addr: String,
    },
    Error {
        message: String,
    },
}

/// Published state of the page (written by the server/tunnel threads, read by the UI timer).
struct Snapshot {
    server: ServerState,
    port: u16,
    /// Public URL the server currently accepts as `Host` ("" = loopback only).
    public_url: String,
    tunnel: tunnel::State,
    log: Vec<String>,
    offer_url: String,
    offer_code: String,
    offer_expires: u64,
    devices: Vec<DeviceInfo>,
    /// One-line page notice (a tunnel started before the server, for example).
    note: String,
}

static SNAPSHOT: Mutex<Snapshot> = Mutex::new(Snapshot {
    server: ServerState::Off,
    port: 0,
    public_url: String::new(),
    tunnel: tunnel::State::Off,
    log: Vec::new(),
    offer_url: String::new(),
    offer_code: String::new(),
    offer_expires: 0,
    devices: Vec::new(),
    note: String::new(),
});
static TUNNEL: Mutex<Option<tunnel::Tunnel>> = Mutex::new(None);
static COMMANDS: OnceLock<Sender<Cmd>> = OnceLock::new();
/// HARNESS_HOME (paired devices live in `remote.json` there).
static STATE_DIR: OnceLock<PathBuf> = OnceLock::new();

enum Cmd {
    Start {
        cfg: RemoteConfig,
        core: std::sync::Arc<dyn HarnessCore>,
    },
    Stop,
    Pair {
        name: String,
    },
    Devices,
    Revoke(String),
    PublicHost(Option<String>),
}

fn snapshot() -> MutexGuard<'static, Snapshot> {
    match SNAPSHOT.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn send(cmd: Cmd) {
    if let Some(tx) = COMMANDS.get() {
        let _ = tx.send(cmd);
    }
}

/// Installs the page callbacks and the state-publishing timer.
pub fn install(window: &AppWindow, home: &Path) {
    let _ = STATE_DIR.set(home.to_path_buf());
    let (tx, rx) = mpsc::channel();
    if COMMANDS.set(tx).is_ok() {
        std::thread::Builder::new()
            .name("harness-remote".into())
            .spawn(move || serve(rx))
            .ok();
    }
    wire(window);
    let weak = window.as_weak();
    let timer = Box::new(slint::Timer::default());
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(TICK_MS),
        move || {
            if let Some(window) = weak.upgrade() {
                refresh(&window);
            }
        },
    );
    // The timer must outlive this call; it shares the event loop with the window.
    Box::leak(timer);
}

/// Called when the page opens: publish at once and pull the device list.
pub fn open() {
    send(Cmd::Devices);
}

/// Stops the tunnel and the server; called on application exit.
pub fn shutdown() {
    stop_tunnel();
    send(Cmd::Stop);
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if snapshot().server == ServerState::Off {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Server thread: one runtime, one handle, commands applied in order.
fn serve(rx: mpsc::Receiver<Cmd>) {
    let Ok(runtime) = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("harness-remote-io")
        .enable_all()
        .build()
    else {
        set_server(ServerState::Error {
            message: "cannot start the remote server runtime".into(),
        });
        return;
    };
    let mut handle: Option<RemoteHandle> = None;
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Start { cfg, core } => {
                if handle.is_some() {
                    continue;
                }
                let port = cfg.port;
                snapshot().port = port;
                set_server(ServerState::Starting);
                match runtime.block_on(RemoteHandle::start(cfg, core)) {
                    Ok(started) => {
                        let addr = started.local_addr().to_string();
                        snapshot().devices = started.devices();
                        started.set_public_host(host_of(&snapshot().public_url));
                        started.set_settings_view(std::sync::Arc::new(
                            settings_view::DesktopSettings,
                        ));
                        handle = Some(started);
                        set_server(ServerState::Up { addr });
                    }
                    Err(error) => set_server(ServerState::Error {
                        message: format!("cannot serve on 127.0.0.1:{port}: {error}"),
                    }),
                }
            }
            Cmd::Stop => {
                if let Some(running) = handle.take() {
                    runtime.block_on(running.shutdown());
                }
                set_server(ServerState::Off);
            }
            Cmd::Pair { name } => {
                if let Some(running) = &handle {
                    let offer = running.new_pair_offer(&name);
                    let mut state = snapshot();
                    state.offer_url = pair_url(&state.public_url, &offer.token);
                    state.offer_code = offer.code;
                    state.offer_expires = offer.expires_ms;
                    state.note = String::new();
                }
            }
            Cmd::Devices => {
                if let Some(running) = &handle {
                    snapshot().devices = running.devices();
                }
            }
            Cmd::Revoke(id) => {
                if let Some(running) = &handle {
                    running.revoke(&id);
                    snapshot().devices = running.devices();
                }
            }
            Cmd::PublicHost(host) => {
                if let Some(running) = &handle {
                    running.set_public_host(host);
                }
            }
        }
    }
    if let Some(running) = handle.take() {
        runtime.block_on(running.shutdown());
    }
    set_server(ServerState::Off);
}

fn set_server(state: ServerState) {
    snapshot().server = state;
}

fn set_tunnel(state: tunnel::State) {
    snapshot().tunnel = state;
}

/// `https://<host>/p#t=<token>` (docs/plans/remote-contract.md "Pair link").
fn pair_url(public_url: &str, token: &str) -> String {
    let base = public_url.trim().trim_end_matches('/');
    if base.is_empty() {
        String::new()
    } else {
        format!("{base}/p#t={token}")
    }
}

/// Host of `https://host/…`, the form `RemoteHandle::set_public_host` expects.
fn host_of(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let host = rest
        .split(['/', '#', '?'])
        .next()
        .unwrap_or_default()
        .trim();
    (!host.is_empty()).then(|| host.to_owned())
}

/// Copies the tunnel's live state into the snapshot (UI timer, before painting).
fn poll_tunnel() {
    let Some(tunnel) = tunnel_handle() else {
        return;
    };
    let state = tunnel.state();
    let log = tunnel.log();
    let mut state_snapshot = snapshot();
    state_snapshot.tunnel = state;
    state_snapshot.log = log;
}

/// Keeps the server's accepted `Host` in step with the tunnel state.
fn sync_public_host() {
    let (tunnel_state, current) = {
        let state = snapshot();
        (state.tunnel.clone(), state.public_url.clone())
    };
    let next = match &tunnel_state {
        tunnel::State::Up { url } => url.clone(),
        _ => String::new(),
    };
    if next == current {
        return;
    }
    send(Cmd::PublicHost(host_of(&next)));
    let mut state = snapshot();
    state.public_url = next;
    // A QR made for the old host would pair against a dead URL.
    state.offer_url = String::new();
    state.offer_code = String::new();
    state.offer_expires = 0;
}

#[derive(Default)]
struct Cache {
    offer_url: String,
    rendered: bool,
    log: Vec<String>,
    devices: Vec<(String, String, u64)>,
    ticks: u32,
}

thread_local! {
    static CACHE: RefCell<Cache> = const { RefCell::new(Cache::new()) };
}

impl Cache {
    const fn new() -> Self {
        Self {
            offer_url: String::new(),
            rendered: false,
            log: Vec::new(),
            devices: Vec::new(),
            ticks: 0,
        }
    }
}

/// Paints the page from the snapshot (UI thread, timer-driven).
fn refresh(window: &AppWindow) {
    poll_tunnel();
    sync_public_host();
    let state = {
        let state = snapshot();
        (
            state.server.clone(),
            state.public_url.clone(),
            state.tunnel.clone(),
            state.log.clone(),
            state.offer_url.clone(),
            state.offer_code.clone(),
            state.offer_expires,
            state.devices.clone(),
            state.note.clone(),
        )
    };
    let (
        server,
        public_url,
        tunnel_state,
        log,
        offer_url,
        offer_code,
        offer_expires,
        devices,
        note,
    ) = state;
    let settings = crate::settings::current();
    settings_view::mirror(settings.clone());
    let ui = window.global::<RemoteUi>();
    ui.set_server_state(match &server {
        ServerState::Off => 0,
        ServerState::Starting => 1,
        ServerState::Up { .. } => 2,
        ServerState::Error { .. } => 3,
    });
    ui.set_server_text(sv(&match &server {
        ServerState::Off => "Off".to_owned(),
        ServerState::Starting => "Starting…".to_owned(),
        ServerState::Up { addr } => format!("Serving on 127.0.0.1:{}", port_of(addr)),
        ServerState::Error { message } => message.clone(),
    }));
    ui.set_has_url(!public_url.is_empty());
    ui.set_public_url(sv(&public_url));
    ui.set_local_url(sv(&match &server {
        ServerState::Up { addr } => format!("http://{addr}"),
        _ => String::new(),
    }));
    ui.set_tunnel_state(match &tunnel_state {
        tunnel::State::Off => 0,
        tunnel::State::Starting => 1,
        tunnel::State::Up { .. } => 2,
        tunnel::State::Error { .. } => 3,
    });
    ui.set_tunnel_text(sv(&match &tunnel_state {
        tunnel::State::Off => "Off".to_owned(),
        tunnel::State::Starting => "Waiting for the public URL…".to_owned(),
        tunnel::State::Up { url } => url.clone(),
        tunnel::State::Error { message } => message.clone(),
    }));
    ui.set_pair_url(sv(&offer_url));
    ui.set_pair_code(sv(&offer_code));
    ui.set_pair_expiry(sv(&expiry_text(offer_expires)));
    ui.set_note(sv(&note));
    if let Some(settings) = &settings {
        ui.set_tunnel_hint(sv(
            &match (&settings.remote.subdomain, &settings.remote.tunnel_domain) {
                (sub, _) if !sub.is_empty() => {
                    "Named tunnel: the host stays the same between starts.".to_owned()
                }
                (_, domain) if !domain.is_empty() => {
                    "Quick tunnel: a random host; the zone field is the fallback URL.".to_owned()
                }
                _ => "Quick tunnel: a new random host on every start, so re-pair by QR afterwards."
                    .to_owned(),
            },
        ));
    }
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.log != log {
            let rows: Vec<SharedString> = log.iter().map(|line| sv(line)).collect();
            ui.set_tunnel_log(model(rows));
            cache.log = log;
        }
        if cache.offer_url != offer_url || !cache.rendered {
            let rendered = (!offer_url.is_empty())
                .then(|| qr::image(&offer_url))
                .flatten();
            let has_qr = rendered.is_some();
            ui.set_has_qr(has_qr);
            if let Some(image) = rendered {
                ui.set_qr(image);
            }
            cache.rendered = has_qr;
            cache.offer_url = offer_url;
        }
        let rows: Vec<(String, String, u64)> = devices
            .iter()
            .map(|d| (d.id.clone(), d.name.clone(), d.last_seen_ms))
            .collect();
        if rows != cache.devices {
            let list: Vec<RemoteDevice> = devices
                .iter()
                .map(|d| RemoteDevice {
                    id: sv(&d.id),
                    name: sv(if d.name.is_empty() {
                        "Phone"
                    } else {
                        d.name.as_str()
                    }),
                    created: sv(&crate::settings::date_of(
                        i64::try_from(d.created_ms).unwrap_or(0),
                    )),
                    last_seen: sv(&if d.last_seen_ms == 0 {
                        "never".to_owned()
                    } else {
                        crate::settings::date_of(i64::try_from(d.last_seen_ms).unwrap_or(0))
                    }),
                })
                .collect();
            ui.set_devices(model(list));
            cache.devices = rows;
        }
        cache.ticks = cache.ticks.wrapping_add(1);
        if cache.ticks % DEVICE_TICKS == 0 {
            send(Cmd::Devices);
        }
    });
}

/// Port of a `127.0.0.1:<port>` socket address.
fn port_of(addr: &str) -> String {
    addr.rsplit(':').next().unwrap_or(addr).to_owned()
}

fn expiry_text(expires_ms: u64) -> String {
    if expires_ms == 0 {
        return String::new();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default();
    if expires_ms <= now {
        return "expired".to_owned();
    }
    let minutes = (expires_ms - now).div_ceil(60_000);
    format!("valid for {minutes} min")
}

fn sv(text: &str) -> SharedString {
    SharedString::from(text)
}

fn model<T: Clone + 'static>(items: Vec<T>) -> ModelRc<T> {
    ModelRc::from(Rc::new(VecModel::from(items)))
}

fn wire(window: &AppWindow) {
    let ui = window.global::<RemoteUi>();
    ui.on_set_enabled(|on| {
        crate::settings::patch_setting("remote.enabled", serde_json::Value::from(on));
        if on {
            start_server();
        } else {
            stop_tunnel();
            send(Cmd::Stop);
        }
    });
    ui.on_start_tunnel(start_tunnel);
    ui.on_stop_tunnel(stop_tunnel);
    ui.on_pair_new(|| {
        send(Cmd::Pair {
            name: "Phone".into(),
        })
    });
    ui.on_revoke(|id| send(Cmd::Revoke(id.to_string())));
    ui.on_refresh(|| send(Cmd::Devices));
    let weak = window.as_weak();
    ui.on_copy(move |text| {
        if let Some(window) = weak.upgrade() {
            window.invoke_copy_text(text);
        }
    });
    ui.on_open_url(|url| open_url(&url));
}

/// Opens an `https://` link in the default browser.
fn open_url(url: &str) {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return;
    }
    let program = if cfg!(windows) {
        "explorer.exe"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(program).arg(url).spawn();
}

/// Starts the local server (page toggle switched on, or "Start server").
pub fn start_server() {
    let Some(settings) = crate::settings::current() else {
        set_server(ServerState::Error {
            message: "settings are not loaded yet".into(),
        });
        return;
    };
    let Some(core) = crate::core::hub::core_handle() else {
        set_server(ServerState::Error {
            message: "the agent hub is still starting; enable the server again in a moment".into(),
        });
        return;
    };
    let state_dir = STATE_DIR.get().cloned().unwrap_or_default();
    snapshot().port = settings.remote.port;
    send(Cmd::Start {
        cfg: RemoteConfig {
            port: settings.remote.port,
            state_dir,
        },
        core,
    });
}

/// Starts the configured tunnel for the running server.
pub fn start_tunnel() {
    let Some(settings) = crate::settings::current() else {
        return;
    };
    let port = match snapshot().server.clone() {
        ServerState::Up { .. } => settings.remote.port,
        _ => {
            snapshot().note = "Start the server before the tunnel.".into();
            return;
        }
    };
    let cfg = tunnel::TunnelConfig::new(
        &settings.remote.tunnel_bin,
        port,
        &settings.remote.subdomain,
        &settings.remote.tunnel_domain,
    );
    let tunnel = tunnel_or_new();
    let state = match tunnel.start(&cfg) {
        Ok(()) => tunnel.state(),
        Err(message) => tunnel::State::Error { message },
    };
    snapshot().note = String::new();
    set_tunnel(state);
}

/// Stops the tunnel (page button, or the server being switched off).
pub fn stop_tunnel() {
    if let Some(tunnel) = tunnel_handle() {
        tunnel.stop();
    }
    send(Cmd::PublicHost(None));
    let mut state = snapshot();
    state.tunnel = tunnel::State::Off;
    state.public_url = String::new();
    state.offer_url = String::new();
    state.offer_code = String::new();
    state.offer_expires = 0;
}

/// The one tunnel supervisor of the process (lazily created).
fn tunnel_handle() -> Option<tunnel::Tunnel> {
    let slot = match TUNNEL.lock() {
        Ok(slot) => slot,
        Err(poisoned) => poisoned.into_inner(),
    };
    slot.as_ref().cloned()
}

/// The tunnel supervisor, created on first use.
fn tunnel_or_new() -> tunnel::Tunnel {
    let mut slot = match TUNNEL.lock() {
        Ok(slot) => slot,
        Err(poisoned) => poisoned.into_inner(),
    };
    slot.get_or_insert_with(tunnel::Tunnel::new).clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_link_and_host() {
        assert_eq!(
            pair_url("https://harness.example.com/", "ab12"),
            "https://harness.example.com/p#t=ab12"
        );
        assert_eq!(pair_url("", "ab12"), "");
        assert_eq!(
            host_of("https://harness.example.com/p#t=x").as_deref(),
            Some("harness.example.com")
        );
        assert_eq!(host_of(""), None);
        assert_eq!(port_of("127.0.0.1:7878"), "7878");
    }
}
