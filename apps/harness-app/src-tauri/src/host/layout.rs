//! Single resolver for where the Host launch files live.
//!
//! Dev builds and packaged builds share one layout: every file sits at the
//! same path relative to a *layout root*. In a dev build the root is the
//! `ZCode` checkout; in a release build it is the `host/` bundle resource
//! staged by `Worse/scripts/stage-sidecar-resources.mjs`, which mirrors the
//! checkout paths. Mirroring keeps the Host's own resolvers working unchanged:
//! `findUpward("apps/zcode-cli/packages/cli/dist/zcode.cjs")` from the Host
//! cwd, and the rust-bridge lookup of `Worse/target/release/harness-host`.

use std::path::{Path, PathBuf};

use tauri::{AppHandle, Manager};

use super::paths::{absolute, env_path, simplify_verbatim};

/// Bundle resource directory that holds the staged layout root.
const STAGED_RESOURCE_DIR: &str = "host";
/// Command that fills the staged layout before `tauri build`.
pub(super) const STAGE_COMMAND: &str = "node Worse/scripts/stage-sidecar-resources.mjs";

/// Layout-relative path of the Node sidecar launcher.
const LAUNCHER: &str = "Worse/sidecars/host-node/dist/main.js";
/// Layout-relative path of the `ZCode` Host bundle entry.
const HOST_ENTRY: &str = "packages/desktop/out/host/index.js";
/// Layout-relative directory of the built-in provider configs.
const PROVIDER_DIR: &str = "config/provider";
/// Layout-relative directory of the release `harness-host` (and, on Windows, the
/// staged `conpty.dll` + `OpenConsole.exe` that portable-pty loads from there).
const RUST_HOST_DIR: &str = "Worse/target/release";
/// Layout-relative path of `ZCode`'s terminal policy bundle used by the Rust PTY route.
const TERMINAL_POLICY: &str = "Worse/sidecars/rust-bridge/dist-vendor/terminalPolicy.js";
/// Layout-relative directory of the bundled Node.js runtime (Phase 6; release layouts only).
const NODE_RUNTIME_DIR: &str = "runtime/node";
/// Layout-relative directory of the bundled native search tools (`<os>-<arch>` appended).
const BUNDLED_TOOLS_DIR: &str = "packages/desktop/bundled-tools";
/// Executable suffix on the build target.
const EXE: &str = if cfg!(windows) { ".exe" } else { "" };

/// Why the layout root was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LayoutSource {
    /// `HARNESS_REPO_ROOT` points at a checkout or a staged directory.
    EnvOverride,
    /// Release build: the bundled `host/` resource.
    Packaged,
    /// Debug build: the checkout this binary was compiled from.
    DevCheckout,
}

/// Resolved layout root for the Host launch files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostLayout {
    root: PathBuf,
    source: LayoutSource,
}

impl HostLayout {
    /// Picks the root: `HARNESS_REPO_ROOT`, else the bundled resource in a
    /// release build, else the dev checkout.
    pub(crate) fn resolve(app: &AppHandle) -> Self {
        let packaged = !cfg!(debug_assertions);
        let resource_dir = if packaged {
            app.path()
                .resource_dir()
                .map_err(|error| {
                    tracing::warn!(%error, "resource directory unavailable; using the dev checkout");
                })
                .ok()
        } else {
            None
        };
        let layout = Self::select(env_path("HARNESS_REPO_ROOT"), resource_dir);
        tracing::info!(
            root = %layout.root.display(),
            source = ?layout.source,
            "resolved the Host launch layout"
        );
        layout
    }

    /// Pure selection, separated for tests. `resource_dir` is `Some` only in
    /// a release build.
    fn select(env_root: Option<PathBuf>, resource_dir: Option<PathBuf>) -> Self {
        if let Some(root) = env_root {
            return Self {
                root: absolute(&root),
                source: LayoutSource::EnvOverride,
            };
        }
        if let Some(resources) = resource_dir {
            return Self {
                root: simplify_verbatim(resources).join(STAGED_RESOURCE_DIR),
                source: LayoutSource::Packaged,
            };
        }
        Self {
            root: absolute(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../..")),
            source: LayoutSource::DevCheckout,
        }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Joins a `/`-separated layout-relative path one component at a time, so
    /// Windows paths handed to Node and logged use one separator style.
    fn path(&self, relative: &str) -> PathBuf {
        let mut path = self.root.clone();
        path.extend(relative.split('/'));
        path
    }

    pub(crate) fn launcher(&self) -> PathBuf {
        self.path(LAUNCHER)
    }

    pub(crate) fn host_entry(&self) -> PathBuf {
        self.path(HOST_ENTRY)
    }

    pub(crate) fn provider_config(&self, file: &str) -> PathBuf {
        self.path(PROVIDER_DIR).join(file)
    }

    /// `true` for the bundled `host/` resource of a release build.
    pub(crate) fn is_packaged(&self) -> bool {
        self.source == LayoutSource::Packaged
    }

    /// The bundled Node.js executable; absent in a dev checkout.
    pub(crate) fn bundled_node(&self) -> PathBuf {
        self.path(NODE_RUNTIME_DIR).join(format!("node{EXE}"))
    }

    /// The application install directory (parent of the bundled `host/`
    /// resource), only for a packaged layout.
    pub(crate) fn install_dir(&self) -> Option<PathBuf> {
        if !self.is_packaged() {
            return None;
        }
        self.root.parent().map(Path::to_path_buf)
    }

    /// The release `harness-host` inside this layout.
    pub(crate) fn rust_host(&self) -> PathBuf {
        self.path(RUST_HOST_DIR).join(format!("harness-host{EXE}"))
    }

    /// Resources the Host can run without (the bridge falls back to TS, tools to
    /// `PATH`) but that a complete layout ships. Reported at spawn so an
    /// incomplete bundle is visible in the log instead of silently slower.
    pub(crate) fn optional_resources(&self) -> Vec<(&'static str, PathBuf)> {
        let tools =
            self.path(BUNDLED_TOOLS_DIR)
                .join(format!("{}-{}", node_platform(), node_arch()));
        let mut resources = vec![
            ("harness-host", self.rust_host()),
            ("terminal policy bundle", self.path(TERMINAL_POLICY)),
            ("ripgrep", tools.join("ripgrep").join(format!("rg{EXE}"))),
        ];
        if cfg!(windows) {
            let dir = self.path(RUST_HOST_DIR);
            resources.push(("conpty.dll", dir.join("conpty.dll")));
            resources.push(("OpenConsole.exe", dir.join("OpenConsole.exe")));
        }
        resources
    }

    /// How to produce a missing artifact for this layout.
    pub(crate) fn remedy(&self, dev_build: &str) -> String {
        match self.source {
            LayoutSource::Packaged => format!(
                "the bundled Host resources are incomplete; reinstall Harness, or run \
                 `{STAGE_COMMAND}` before `tauri build`"
            ),
            LayoutSource::EnvOverride | LayoutSource::DevCheckout => {
                format!("build it with `{dev_build}`")
            }
        }
    }
}

/// Node's `process.platform` for the build target (the Host keys bundled tools by it).
fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        other => other,
    }
}

/// Node's `process.arch` for the build target.
fn node_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{HostLayout, LayoutSource};

    fn resources() -> PathBuf {
        std::env::temp_dir().join("harness-resources")
    }

    #[test]
    fn env_override_wins_over_the_bundle() {
        let explicit = std::env::temp_dir().join("checkout");
        let layout = HostLayout::select(Some(explicit.clone()), Some(resources()));
        assert_eq!(layout.source, LayoutSource::EnvOverride);
        assert_eq!(layout.root(), explicit.as_path());
    }

    #[test]
    fn release_builds_use_the_staged_resource() {
        let layout = HostLayout::select(None, Some(resources()));
        assert_eq!(layout.source, LayoutSource::Packaged);
        assert_eq!(
            layout.launcher(),
            resources().join("host/Worse/sidecars/host-node/dist/main.js")
        );
        assert_eq!(
            layout.host_entry(),
            resources().join("host/packages/desktop/out/host/index.js")
        );
        assert!(layout.remedy("x").contains("stage-sidecar-resources"));
        assert!(layout.is_packaged());
        let exe = if cfg!(windows) { ".exe" } else { "" };
        assert_eq!(
            layout.bundled_node(),
            resources().join(format!("host/runtime/node/node{exe}"))
        );
        assert_eq!(layout.install_dir(), Some(resources()));
        assert_eq!(
            layout.rust_host(),
            resources().join(format!("host/Worse/target/release/harness-host{exe}"))
        );
        let optional = layout.optional_resources();
        assert!(optional
            .iter()
            .all(|(_, path)| path.starts_with(resources().join("host"))));
        assert!(optional
            .iter()
            .any(|(_, path)| path
                .ends_with("Worse/sidecars/rust-bridge/dist-vendor/terminalPolicy.js")));
        if cfg!(all(windows, target_arch = "x86_64")) {
            assert!(
                optional
                    .iter()
                    .any(|(_, path)| path
                        == &resources().join("host/Worse/target/release/conpty.dll"))
            );
            assert!(optional.iter().any(|(_, path)| path
                .ends_with("packages/desktop/bundled-tools/win32-x64/ripgrep/rg.exe")));
        }
    }

    #[test]
    fn dev_builds_use_the_checkout_root() {
        let layout = HostLayout::select(None, None);
        assert_eq!(layout.source, LayoutSource::DevCheckout);
        assert!(layout
            .launcher()
            .ends_with("Worse/sidecars/host-node/dist/main.js"));
        assert!(layout
            .root()
            .join("Worse/apps/harness-app/src-tauri")
            .is_dir());
        assert_eq!(layout.remedy("pnpm build"), "build it with `pnpm build`");
        assert!(!layout.is_packaged());
        assert_eq!(layout.install_dir(), None);
        assert!(layout
            .bundled_node()
            .starts_with(layout.root().join("runtime")));
    }
}
