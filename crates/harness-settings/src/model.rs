//! Typed settings (spec §9.1.1). Every field has a serde default, so a missing or partial file yields defaults.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Schema version this build reads and writes.
pub const SCHEMA_VERSION: u64 = 1;

/// Root of `settings.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub schema_version: u64,
    pub general: GeneralSettings,
    pub appearance: AppearanceSettings,
    pub notifications: NotificationSettings,
    pub updates: UpdateSettings,
    /// Keyboard shortcut overrides: command id → bindings (verbatim strings). Defaults live in the UI keymap.
    pub shortcuts: BTreeMap<String, Vec<String>>,
    /// Per agent id preferences.
    pub agents: BTreeMap<String, AgentPrefs>,
    pub memory: MemorySettings,
    /// First-run setup (phase-9-parity.md §9.10).
    pub setup: SetupSettings,
    /// Phone access (phase-12-remote-phone.md): local HTTP server + tunnel.
    pub remote: RemoteSettings,
    /// Orchestra page: delegation defaults (settings redesign S3).
    #[serde(default)]
    pub orchestra: OrchestraSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            general: GeneralSettings::default(),
            appearance: AppearanceSettings::default(),
            notifications: NotificationSettings::default(),
            updates: UpdateSettings::default(),
            shortcuts: BTreeMap::new(),
            agents: BTreeMap::new(),
            memory: MemorySettings::default(),
            setup: SetupSettings::default(),
            remote: RemoteSettings::default(),
            orchestra: OrchestraSettings::default(),
        }
    }
}

impl Settings {
    /// Copy with every bounded field clamped to its range (applied on load and on every patch).
    #[must_use]
    pub fn normalized(mut self) -> Self {
        let a = &mut self.appearance;
        a.ui_font_size = a.ui_font_size.clamp(12, 20);
        a.code_font_size = a.code_font_size.clamp(10, 24);
        a.zoom_percent = a.zoom_percent.clamp(80, 150);
        let t = &mut self.general.terminal;
        t.font_size = t.font_size.clamp(8, 32);
        self.general.auto_archive_days = self.general.auto_archive_days.clamp(1, 3650);
        self.orchestra.max_concurrent = self.orchestra.max_concurrent.clamp(1, 8);
        self.orchestra.canvas_zoom = self.orchestra.canvas_zoom.clamp(40, 200);
        // A privileged or zero port cannot be bound by an unprivileged process: fall back to the default.
        if self.remote.port < 1024 {
            self.remote.port = RemoteSettings::default().port;
        }
        self.schema_version = SCHEMA_VERSION;
        self
    }

    /// Preferences of `agent_id` (defaults when not set).
    #[must_use]
    pub fn agent(&self, agent_id: &str) -> AgentPrefs {
        self.agents.get(agent_id).cloned().unwrap_or_default()
    }
}

/// UI language. `System` follows the OS locale.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    #[default]
    #[serde(rename = "system")]
    System,
    #[serde(rename = "en-US")]
    EnUs,
    #[serde(rename = "zh-CN")]
    ZhCn,
}

impl Language {
    /// Parses a stored tag (`system`, `en-US`, `zh-CN`, case-insensitive; `en`/`zh` prefixes accepted).
    #[must_use]
    pub fn from_tag(tag: &str) -> Option<Self> {
        let lower = tag.trim().to_ascii_lowercase();
        match lower.as_str() {
            "system" | "" => Some(Self::System),
            t if t.starts_with("zh") => Some(Self::ZhCn),
            t if t.starts_with("en") => Some(Self::EnUs),
            _ => None,
        }
    }
}

/// Colour theme.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ThemePref {
    #[default]
    System,
    Light,
    Dark,
}

/// Follow-up message while a turn runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FollowUp {
    /// Queue it for after the turn.
    #[default]
    Queue,
    /// Steer the running turn (called guide in the old app).
    #[serde(alias = "guide")]
    Steer,
}

/// Composer send key.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ComposerSend {
    #[default]
    Enter,
    ModEnter,
}

/// General page.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct GeneralSettings {
    pub language: Language,
    pub terminal: TerminalSettings,
    pub proxy: ProxySettings,
    /// Legacy switch that was never read (it defaulted to on while the app drew on the CPU); kept so old
    /// settings files load. `gpu_rendering` is the real one.
    pub hardware_acceleration: bool,
    /// Draw with the GPU (Skia on OpenGL / Direct3D / Metal) instead of the CPU. Smoother on very large
    /// screens and with animated wallpapers, at about 170 MB more memory. Takes effect after restart.
    pub gpu_rendering: bool,
    pub follow_up: FollowUp,
    pub composer_send: ComposerSend,
    pub show_reasoning: bool,
    pub show_todos: bool,
    pub group_exploration: bool,
    pub group_terminal: bool,
    pub group_file_changes: bool,
    pub auto_archive: bool,
    pub auto_archive_days: u32,
    pub auto_compact: bool,
    pub last_workspace: Option<String>,
    /// Open Harness when the user signs in (settings S1 "Launch at startup").
    pub launch_at_startup: bool,
}

impl Default for GeneralSettings {
    fn default() -> Self {
        Self {
            language: Language::System,
            terminal: TerminalSettings::default(),
            proxy: ProxySettings::default(),
            hardware_acceleration: true,
            gpu_rendering: false,
            follow_up: FollowUp::Queue,
            composer_send: ComposerSend::Enter,
            show_reasoning: true,
            show_todos: true,
            group_exploration: true,
            group_terminal: true,
            group_file_changes: true,
            auto_archive: false,
            auto_archive_days: 30,
            auto_compact: true,
            last_workspace: None,
            launch_at_startup: false,
        }
    }
}

/// Integrated terminal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TerminalSettings {
    pub inherit_system_profile: bool,
    pub font_family: Option<String>,
    pub font_size: u16,
    pub shell: Option<String>,
}

impl Default for TerminalSettings {
    fn default() -> Self {
        Self {
            inherit_system_profile: true,
            font_family: None,
            font_size: 13,
            shell: None,
        }
    }
}

/// Network proxy passed to agent CLIs (env) in 9.5.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ProxySettings {
    pub http_proxy: Option<String>,
    pub no_proxy: Option<String>,
    pub ca_cert_path: Option<String>,
}

/// Appearance page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AppearanceSettings {
    pub theme: ThemePref,
    /// "jakarta" | "inter" | "system".
    pub ui_font: String,
    pub ui_font_size: u16,
    pub zoom_percent: u16,
    pub code_theme_light: String,
    pub code_theme_dark: String,
    pub line_numbers: bool,
    pub wrap_lines: bool,
    pub code_font_size: u16,
    /// Use the OS window frame (Windows title bar + native resize / snap) instead of the custom
    /// borderless chrome. Applied at startup: winit cannot switch decorations on a live window.
    pub native_frame: bool,
    /// Accent colour "#RRGGBB" (settings S2 swatches).
    pub accent_color: String,
    /// "compact" | "comfortable".
    pub density: String,
    pub reduce_motion: bool,
    /// Context-use ring next to each session in the sidebar.
    pub show_token_usage: bool,
    /// Unicode animation beside "Thinking…": "scan" | "bars" | "pulse" | "quads" | "charge" | "moon" |
    /// "cipher" | "chevrons" | "bunny" | "shuffle" (a different one every turn).
    pub thinking_style: String,
    /// Words beside it: "plain" ("Thinking…") | "playful" (a rotating set).
    pub status_words: String,
    /// Unicode field at the window edges: "dither" | "rain" | "stars" | "circuit" | "none".
    pub backdrop_field: String,
    /// When the field moves: "home" (empty chat only) | "always" | "off".
    pub backdrop_motion: String,
    /// Wallpaper behind the window: path of an image or an animated GIF / WebP ("" = none).
    pub wallpaper_path: String,
    /// Blur of the wallpaper, 0..=40 (pixels of a 1440 px wide window).
    pub wallpaper_blur: u16,
    /// How much of the canvas colour lies over the wallpaper, percent 0..=95.
    pub wallpaper_dim: u16,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        Self {
            theme: ThemePref::System,
            ui_font: "jakarta".into(),
            ui_font_size: 14,
            zoom_percent: 100,
            code_theme_light: "github-light".into(),
            code_theme_dark: "github-dark".into(),
            line_numbers: true,
            wrap_lines: false,
            code_font_size: 13,
            native_frame: true,
            accent_color: "#C8F04A".into(),
            density: "comfortable".into(),
            reduce_motion: false,
            show_token_usage: true,
            thinking_style: "scan".into(),
            status_words: "plain".into(),
            backdrop_field: "dither".into(),
            backdrop_motion: "home".into(),
            wallpaper_path: String::new(),
            wallpaper_blur: 8,
            wallpaper_dim: 55,
        }
    }
}

/// Notifications and window behaviour.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct NotificationSettings {
    pub task_done: bool,
    pub sound: bool,
    pub close_to_tray: bool,
    pub keep_awake: bool,
    /// Notify when cargo build / CI finishes (settings S1).
    pub build_done: bool,
    /// Notify when a pull request asks for a review (settings S1).
    pub pr_review: bool,
}

impl Default for NotificationSettings {
    fn default() -> Self {
        Self {
            task_done: true,
            sound: true,
            close_to_tray: false,
            keep_awake: false,
            build_done: true,
            pr_review: true,
        }
    }
}

/// Update channel.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct UpdateSettings {
    pub preview_channel: bool,
    pub auto_download: bool,
}

/// Per-agent preferences (AGT-002, AGT-007). Program/args/env live in `agents.json` (hub-owned launch config).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AgentPrefs {
    pub enabled: bool,
    pub default_mode: Option<String>,
    pub default_model: Option<String>,
    /// Value for the agent's `effort`-like config option, when it advertises one.
    pub default_effort: Option<String>,
}

impl Default for AgentPrefs {
    fn default() -> Self {
        Self {
            enabled: true,
            default_mode: None,
            default_model: None,
            default_effort: None,
        }
    }
}

/// Memory page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MemorySettings {
    pub workspace_memory_enabled: bool,
}

impl Default for MemorySettings {
    fn default() -> Self {
        Self {
            workspace_memory_enabled: true,
        }
    }
}

/// First-run setup state (phase-9-parity.md §9.10).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SetupSettings {
    /// The first-run "Set up agents" dialog was handled (closed or not needed).
    pub first_run_done: bool,
}

/// Phone access (phase-12-remote-phone.md §3): local HTTP/WS server plus the `cftunnel` supervisor.
/// Loopback only; a tunnel is started from Settings, never at launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RemoteSettings {
    /// The local server should run (started from the Settings page, not at launch).
    pub enabled: bool,
    pub port: u16,
    /// Tunnel helper binary: `cftunnel http <port> --name <subdomain>`.
    pub tunnel_bin: String,
    /// Named tunnel subdomain (empty = quick tunnel with a random host).
    pub subdomain: String,
    /// Zone appended when the tunnel prints no URL: `https://<subdomain>.<tunnel_domain>`.
    pub tunnel_domain: String,
}

impl Default for RemoteSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 7878,
            tunnel_bin: "cftunnel".into(),
            subdomain: String::new(),
            tunnel_domain: String::new(),
        }
    }
}

/// Orchestra page (settings redesign S3): how Harness delegates work to subagents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct OrchestraSettings {
    /// Parallel subagents (1..=8); extra delegations wait in the queue.
    pub max_concurrent: u16,
    /// Agent id used when a task does not name one (empty = first connected agent).
    pub default_agent: String,
    /// Permission mode subagents start in: "ask" | "acceptEdits" | "machine".
    pub default_mode: String,
    /// Subagents claim files before editing.
    pub auto_claim: bool,
    /// Markdown appended to every delegated task.
    pub instructions: String,
    /// Orchestra canvas zoom in percent (40..=200).
    pub canvas_zoom: u16,
    pub snap_to_grid: bool,
}

impl Default for OrchestraSettings {
    fn default() -> Self {
        Self {
            max_concurrent: 4,
            default_agent: String::new(),
            default_mode: "acceptEdits".into(),
            auto_claim: true,
            instructions: String::new(),
            canvas_zoom: 100,
            snap_to_grid: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_file_gets_defaults() -> Result<(), serde_json::Error> {
        let s: Settings =
            serde_json::from_str(r#"{"appearance":{"theme":"dark","uiFontSize":99}}"#)?;
        let s = s.normalized();
        assert_eq!(s.appearance.theme, ThemePref::Dark);
        assert_eq!(s.appearance.ui_font_size, 20);
        assert!(s.general.show_reasoning);
        assert_eq!(s.general.language, Language::System);
        assert_eq!(s.schema_version, SCHEMA_VERSION);
        Ok(())
    }

    #[test]
    fn follow_up_accepts_zcode_alias() -> Result<(), serde_json::Error> {
        let g: GeneralSettings = serde_json::from_str(r#"{"followUp":"guide"}"#)?;
        assert_eq!(g.follow_up, FollowUp::Steer);
        assert_eq!(Language::from_tag("zh"), Some(Language::ZhCn));
        Ok(())
    }

    #[test]
    fn native_frame_defaults_to_true_and_round_trips() -> Result<(), serde_json::Error> {
        // Existing settings.json files have no `nativeFrame`: the OS frame is the default.
        let s: Settings = serde_json::from_str(r#"{"appearance":{"theme":"dark"}}"#)?;
        assert!(s.appearance.native_frame);
        let json = serde_json::to_value(&s)?;
        assert_eq!(json["appearance"]["nativeFrame"], true);

        let s: Settings = serde_json::from_str(r#"{"appearance":{"nativeFrame":false}}"#)?;
        assert!(!s.appearance.native_frame);
        // camelCase key, like the rest of settings.json.
        let json = serde_json::to_value(&s)?;
        assert_eq!(json["appearance"]["nativeFrame"], false);
        Ok(())
    }

    #[test]
    fn remote_defaults_and_round_trip() -> Result<(), serde_json::Error> {
        // Missing `remote` in an old file: defaults, no auto-start.
        let s: Settings = serde_json::from_str("{}")?;
        assert_eq!(s.remote, RemoteSettings::default());
        assert!(!s.remote.enabled);
        assert_eq!(s.remote.port, 7878);
        assert_eq!(s.remote.tunnel_bin, "cftunnel");

        let s: Settings = serde_json::from_str(
            r#"{"remote":{"enabled":true,"port":9001,"tunnelBin":"C:\\t.exe","subdomain":"harness","tunnelDomain":"example.com"}}"#,
        )?;
        assert!(s.remote.enabled);
        assert_eq!(s.remote.port, 9001);
        assert_eq!(s.remote.subdomain, "harness");
        assert_eq!(s.remote.tunnel_domain, "example.com");
        // Keys are camelCase, like the rest of settings.json.
        let json = serde_json::to_value(&s)?;
        assert_eq!(json["remote"]["tunnelBin"], "C:\\t.exe");
        assert_eq!(json["remote"]["tunnelDomain"], "example.com");
        // A privileged port falls back to the default instead of failing to bind.
        let s: Settings = serde_json::from_str(r#"{"remote":{"port":80}}"#)?;
        assert_eq!(s.normalized().remote.port, 7878);
        Ok(())
    }
}
