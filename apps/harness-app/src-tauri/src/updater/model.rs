//! Pure updater model: renderer payloads, the update phase machine's views,
//! and the feed/key policy (spec `phase-6-updater.md` sections 2, 3, 5, 9).
//!
//! Nothing here touches the network, the filesystem or Tauri, so every rule
//! is unit-tested.

use serde::{Deserialize, Serialize};
use tauri::Url;

/// Prefix of the committed placeholder key; such a key keeps the updater off.
pub(crate) const PUBKEY_PLACEHOLDER_PREFIX: &str = "HARNESS_UPDATER_PUBKEY_PLACEHOLDER";

/// Values of `HARNESS_UPDATE_FEED_URL` that switch the updater off explicitly.
const FEED_OFF_VALUES: &[&str] = &["off", "none", "0", "false"];

/// `PostUpdateReleaseNotesPayload` (`packages/shared/src/update.ts`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReleaseNotes {
    pub(crate) version: String,
    pub(crate) title: String,
    pub(crate) markdown: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) release_date: Option<String>,
}

/// `UpdateStatePayload` (`packages/shared/src/update.ts`). Single channel, so
/// `channel` is never sent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub(crate) enum StatePayload {
    Idle {
        enabled: bool,
    },
    Checking {
        enabled: bool,
    },
    UpdateAvailable {
        enabled: bool,
        version: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        release_notes: Option<ReleaseNotes>,
    },
    DownloadProgress {
        enabled: bool,
        progress: String,
        transferred_bytes: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        total_bytes: Option<u64>,
        version: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        release_notes: Option<ReleaseNotes>,
    },
    UpdateDownloaded {
        enabled: bool,
        version: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        release_notes: Option<ReleaseNotes>,
    },
}

/// `UpdateCheckResultPayload` (`packages/shared/src/update.ts`), sent for
/// manual checks and download failures.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub(crate) enum CheckResult {
    UpToDate {
        current_version: String,
    },
    Available {
        version: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        release_notes: Option<ReleaseNotes>,
    },
    AlreadyDownloading {
        version: String,
        progress: String,
    },
    Ready {
        version: String,
    },
    DevSkipped,
    Error {
        message: String,
    },
}

/// The shell-owned update phase. `Installing` is the admission lock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Idle,
    Checking,
    Available {
        version: String,
        notes: Option<ReleaseNotes>,
    },
    Downloading {
        version: String,
        notes: Option<ReleaseNotes>,
        transferred: u64,
        total: Option<u64>,
    },
    Ready {
        version: String,
        notes: Option<ReleaseNotes>,
    },
    Installing {
        version: String,
        notes: Option<ReleaseNotes>,
    },
}

impl Phase {
    /// Stable name for logs, errors and diagnostics.
    pub(crate) const fn kind(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Checking => "checking",
            Self::Available { .. } => "update-available",
            Self::Downloading { .. } => "download-progress",
            Self::Ready { .. } => "update-downloaded",
            Self::Installing { .. } => "installing",
        }
    }

    /// Whether an update target is shown to the user (faster poll, `ZCode` row 1).
    pub(crate) const fn has_visible_target(&self) -> bool {
        matches!(self, Self::Available { .. } | Self::Ready { .. })
    }

    /// The renderer view of this phase. `Installing` stays "downloaded" but
    /// disabled, like `ZCode`'s admission lock.
    pub(crate) fn payload(&self) -> StatePayload {
        match self {
            Self::Idle => StatePayload::Idle { enabled: true },
            Self::Checking => StatePayload::Checking { enabled: false },
            Self::Available { version, notes } => StatePayload::UpdateAvailable {
                enabled: true,
                version: version.clone(),
                release_notes: notes.clone(),
            },
            Self::Downloading {
                version,
                notes,
                transferred,
                total,
            } => StatePayload::DownloadProgress {
                enabled: false,
                progress: progress_percent(*transferred, *total),
                transferred_bytes: *transferred,
                total_bytes: *total,
                version: version.clone(),
                release_notes: notes.clone(),
            },
            Self::Ready { version, notes } => StatePayload::UpdateDownloaded {
                enabled: true,
                version: version.clone(),
                release_notes: notes.clone(),
            },
            Self::Installing { version, notes } => StatePayload::UpdateDownloaded {
                enabled: false,
                version: version.clone(),
                release_notes: notes.clone(),
            },
        }
    }
}

/// Whole-percent progress string (`"0"`..`"100"`); `"0"` when the size is unknown.
pub(crate) fn progress_percent(transferred: u64, total: Option<u64>) -> String {
    match total {
        Some(total) if total > 0 => (transferred.saturating_mul(100) / total)
            .min(100)
            .to_string(),
        _ => "0".to_owned(),
    }
}

/// Builds the release notes from the feed's `notes` and `pub_date`.
pub(crate) fn release_notes(
    version: &str,
    body: Option<&str>,
    pub_date: Option<&str>,
) -> Option<ReleaseNotes> {
    let markdown = body.map(str::trim).filter(|text| !text.is_empty())?;
    let title = markdown
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with('#'))
        .map(|line| line.trim_start_matches('#').trim().to_owned())
        .filter(|line| !line.is_empty())
        .unwrap_or_else(|| format!("Harness {version}"));
    Some(ReleaseNotes {
        version: version.to_owned(),
        title,
        markdown: markdown.to_owned(),
        release_date: pub_date
            .map(str::trim)
            .filter(|date| !date.is_empty())
            .map(ToOwned::to_owned),
    })
}

/// Outcome of the feed URL policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FeedDecision {
    On(Url),
    Off(String),
}

/// Applies the feed rules: runtime env first, then the built config; https
/// only, plain http only for a loopback host when the build allows it.
pub(crate) fn resolve_feed(
    env_value: Option<&str>,
    configured: Option<&str>,
    insecure_loopback_allowed: bool,
) -> FeedDecision {
    let env_value = env_value.map(str::trim).filter(|value| !value.is_empty());
    if let Some(value) = env_value {
        if FEED_OFF_VALUES
            .iter()
            .any(|off| value.eq_ignore_ascii_case(off))
        {
            return FeedDecision::Off("disabled by HARNESS_UPDATE_FEED_URL".to_owned());
        }
    }
    let Some(candidate) =
        env_value.or_else(|| configured.map(str::trim).filter(|value| !value.is_empty()))
    else {
        return FeedDecision::Off("no update feed is configured".to_owned());
    };
    let url = match Url::parse(candidate) {
        Ok(url) => url,
        Err(error) => return FeedDecision::Off(format!("the update feed URL is invalid: {error}")),
    };
    match url.scheme() {
        "https" => FeedDecision::On(url),
        "http" if is_loopback(&url) && insecure_loopback_allowed => FeedDecision::On(url),
        "http" if is_loopback(&url) => FeedDecision::Off(
            "plain http feeds need a test build (dangerousInsecureTransportProtocol)".to_owned(),
        ),
        other => FeedDecision::Off(format!(
            "update feeds must use https (got `{other}`; http only for loopback test feeds)"
        )),
    }
}

/// True when the URL's host is a loopback name or address.
pub(crate) fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

/// Picks the minisign public key: the build-time env wins over the config.
/// Empty or placeholder keys keep the updater off (fail closed).
pub(crate) fn resolve_pubkey(build_env: Option<&str>, configured: Option<&str>) -> Option<String> {
    let usable = |value: &str| {
        let value = value.trim();
        (!value.is_empty() && !value.starts_with(PUBKEY_PLACEHOLDER_PREFIX))
            .then(|| value.to_owned())
    };
    build_env
        .and_then(usable)
        .or_else(|| configured.and_then(usable))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on(url: &str) -> FeedDecision {
        match Url::parse(url) {
            Ok(url) => FeedDecision::On(url),
            Err(error) => FeedDecision::Off(error.to_string()),
        }
    }

    #[test]
    fn feed_defaults_to_off() {
        assert!(matches!(
            resolve_feed(None, None, true),
            FeedDecision::Off(_)
        ));
        assert!(matches!(
            resolve_feed(Some("  "), Some(""), true),
            FeedDecision::Off(_)
        ));
    }

    #[test]
    fn env_feed_wins_and_can_disable() {
        let configured = Some("https://updates.example.test/latest.json");
        assert_eq!(
            resolve_feed(Some("https://other.example.test/f.json"), configured, false),
            on("https://other.example.test/f.json")
        );
        assert!(matches!(
            resolve_feed(Some("OFF"), configured, false),
            FeedDecision::Off(_)
        ));
        assert_eq!(
            resolve_feed(None, configured, false),
            on("https://updates.example.test/latest.json")
        );
    }

    #[test]
    fn http_only_for_allowed_loopback() {
        assert_eq!(
            resolve_feed(Some("http://127.0.0.1:4100/latest.json"), None, true),
            on("http://127.0.0.1:4100/latest.json")
        );
        assert_eq!(
            resolve_feed(Some("http://localhost:4100/latest.json"), None, true),
            on("http://localhost:4100/latest.json")
        );
        assert_eq!(
            resolve_feed(Some("http://[::1]:9/l.json"), None, true),
            on("http://[::1]:9/l.json")
        );
        assert!(matches!(
            resolve_feed(Some("http://127.0.0.1:4100/latest.json"), None, false),
            FeedDecision::Off(_)
        ));
        assert!(matches!(
            resolve_feed(Some("http://updates.example.test/latest.json"), None, true),
            FeedDecision::Off(_)
        ));
        assert!(matches!(
            resolve_feed(Some("file:///tmp/latest.json"), None, true),
            FeedDecision::Off(_)
        ));
        assert!(matches!(
            resolve_feed(Some("not a url"), None, true),
            FeedDecision::Off(_)
        ));
    }

    #[test]
    fn pubkey_placeholder_keeps_updater_off() {
        let placeholder = format!("{PUBKEY_PLACEHOLDER_PREFIX} (supply at build time)");
        assert_eq!(resolve_pubkey(None, Some(&placeholder)), None);
        assert_eq!(resolve_pubkey(Some(""), Some(" ")), None);
        assert_eq!(
            resolve_pubkey(Some("KEY"), Some(&placeholder)),
            Some("KEY".to_owned())
        );
        assert_eq!(resolve_pubkey(None, Some(" CFG ")), Some("CFG".to_owned()));
        assert_eq!(
            resolve_pubkey(Some("ENV"), Some("CFG")),
            Some("ENV".to_owned())
        );
    }

    #[test]
    fn progress_is_whole_percent() {
        assert_eq!(progress_percent(0, Some(200)), "0");
        assert_eq!(progress_percent(101, Some(200)), "50");
        assert_eq!(progress_percent(500, Some(200)), "100");
        assert_eq!(progress_percent(10, None), "0");
        assert_eq!(progress_percent(10, Some(0)), "0");
    }

    #[test]
    fn release_notes_title_and_date() {
        let notes = release_notes(
            "0.1.1",
            Some("# Harness 0.1.1\n\n- fix"),
            Some("2026-11-01T00:00:00Z"),
        );
        assert_eq!(
            notes,
            Some(ReleaseNotes {
                version: "0.1.1".to_owned(),
                title: "Harness 0.1.1".to_owned(),
                markdown: "# Harness 0.1.1\n\n- fix".to_owned(),
                release_date: Some("2026-11-01T00:00:00Z".to_owned()),
            })
        );
        let plain = release_notes("0.2.0", Some("- only bullets"), None);
        assert_eq!(plain.map(|n| n.title), Some("Harness 0.2.0".to_owned()));
        assert_eq!(release_notes("0.2.0", Some("   "), None), None);
        assert_eq!(release_notes("0.2.0", None, None), None);
    }

    #[test]
    fn payloads_match_the_zcode_wire_shape() -> Result<(), serde_json::Error> {
        let downloading = Phase::Downloading {
            version: "0.1.1".to_owned(),
            notes: None,
            transferred: 50,
            total: Some(100),
        };
        assert_eq!(
            serde_json::to_value(downloading.payload())?,
            serde_json::json!({
                "kind": "download-progress",
                "enabled": false,
                "progress": "50",
                "transferredBytes": 50,
                "totalBytes": 100,
                "version": "0.1.1"
            })
        );
        let installing = Phase::Installing {
            version: "0.1.1".to_owned(),
            notes: None,
        };
        assert_eq!(
            serde_json::to_value(installing.payload())?,
            serde_json::json!({ "kind": "update-downloaded", "enabled": false, "version": "0.1.1" })
        );
        assert_eq!(
            serde_json::to_value(Phase::Idle.payload())?,
            serde_json::json!({ "kind": "idle", "enabled": true })
        );
        assert_eq!(
            serde_json::to_value(CheckResult::DevSkipped)?,
            serde_json::json!({ "kind": "dev-skipped" })
        );
        assert_eq!(
            serde_json::to_value(CheckResult::UpToDate {
                current_version: "0.1.0".to_owned()
            })?,
            serde_json::json!({ "kind": "up-to-date", "currentVersion": "0.1.0" })
        );
        assert_eq!(
            serde_json::to_value(CheckResult::AlreadyDownloading {
                version: "0.1.1".to_owned(),
                progress: "3".to_owned()
            })?,
            serde_json::json!({ "kind": "already-downloading", "version": "0.1.1", "progress": "3" })
        );
        Ok(())
    }

    #[test]
    fn visible_targets() {
        assert!(!Phase::Idle.has_visible_target());
        assert!(Phase::Ready {
            version: "1".to_owned(),
            notes: None
        }
        .has_visible_target());
    }
}
