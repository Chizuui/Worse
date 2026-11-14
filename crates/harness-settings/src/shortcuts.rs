//! Keyboard shortcut normalisation and conflict detection (SET shortcuts page).
//!
//! Bindings are strings such as `Mod+Shift+K` or chords `Ctrl+K Ctrl+S`. `Mod` is Ctrl on Windows/Linux and Cmd on
//! macOS. Normalisation ignores modifier order and case, so `shift+ctrl+k` and `Ctrl+Shift+K` collide.

use std::collections::BTreeMap;

/// Bindings that more than one command uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// Normalised binding.
    pub binding: String,
    /// Command ids using it (sorted).
    pub commands: Vec<String>,
}

const MODIFIER_ORDER: &[&str] = &["ctrl", "cmd", "alt", "shift"];

/// Normalised form of one binding for the current platform.
#[must_use]
pub fn normalize(binding: &str) -> String {
    normalize_for(binding, cfg!(target_os = "macos"))
}

/// Normalised form of one binding; `mac` selects what `Mod` means.
#[must_use]
pub fn normalize_for(binding: &str, mac: bool) -> String {
    binding
        .split_whitespace()
        .map(|stroke| normalize_stroke(stroke, mac))
        .collect::<Vec<_>>()
        .join(" ")
}

fn normalize_stroke(stroke: &str, mac: bool) -> String {
    let mut modifiers = Vec::new();
    let mut key = String::new();
    for part in stroke.split('+').map(str::trim).filter(|p| !p.is_empty()) {
        let lower = part.to_ascii_lowercase();
        let modifier = match lower.as_str() {
            "mod" | "cmdorctrl" | "commandorcontrol" => Some(if mac { "cmd" } else { "ctrl" }),
            "ctrl" | "control" => Some("ctrl"),
            "cmd" | "command" | "meta" | "super" | "win" => Some("cmd"),
            "alt" | "option" | "opt" => Some("alt"),
            "shift" => Some("shift"),
            _ => None,
        };
        match modifier {
            Some(m) if !modifiers.contains(&m) => modifiers.push(m),
            Some(_) => {}
            None => key = lower,
        }
    }
    modifiers.sort_by_key(|m| MODIFIER_ORDER.iter().position(|o| o == m));
    let mut parts: Vec<&str> = modifiers;
    if !key.is_empty() {
        parts.push(&key);
    }
    parts.join("+")
}

/// Conflicts of the effective keymap: `defaults` overlaid by `overrides` (an override replaces all bindings of its
/// command; an empty list unbinds it).
#[must_use]
pub fn conflicts(
    defaults: &BTreeMap<String, Vec<String>>,
    overrides: &BTreeMap<String, Vec<String>>,
) -> Vec<Conflict> {
    let mut by_binding: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let commands = defaults.keys().chain(overrides.keys());
    let mut seen = std::collections::BTreeSet::new();
    for command in commands {
        if !seen.insert(command) {
            continue;
        }
        let bindings = overrides
            .get(command)
            .or_else(|| defaults.get(command))
            .map_or(&[][..], Vec::as_slice);
        for binding in bindings {
            let key = normalize(binding);
            if key.is_empty() {
                continue;
            }
            let users = by_binding.entry(key).or_default();
            if !users.contains(command) {
                users.push(command.clone());
            }
        }
    }
    by_binding
        .into_iter()
        .filter(|(_, users)| users.len() > 1)
        .map(|(binding, mut commands)| {
            commands.sort();
            Conflict { binding, commands }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(entries: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        entries
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.iter().map(|s| (*s).to_owned()).collect()))
            .collect()
    }

    #[test]
    fn normalises_order_case_and_mod() {
        assert_eq!(normalize_for("shift+Ctrl+k", false), "ctrl+shift+k");
        assert_eq!(normalize_for("Mod+Shift+K", false), "ctrl+shift+k");
        assert_eq!(normalize_for("Mod+K", true), "cmd+k");
        assert_eq!(normalize_for("Ctrl+K  Ctrl+S", false), "ctrl+k ctrl+s");
    }

    #[test]
    fn finds_conflicts_after_overrides() {
        let defaults = map(&[("a", &["Ctrl+K"]), ("b", &["Ctrl+J"]), ("c", &["Ctrl+L"])]);
        let overrides = map(&[("b", &["ctrl+k"]), ("c", &[])]);
        let found = conflicts(&defaults, &overrides);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].commands, vec!["a".to_owned(), "b".to_owned()]);
        assert_eq!(conflicts(&defaults, &BTreeMap::new()), Vec::new());
    }
}
