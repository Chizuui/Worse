//! Minimal YAML front matter for markdown definitions (skills, commands, subagents).
//!
//! Only top-level `key: value` lines are interpreted; every other line (comments, nested blocks) is kept verbatim
//! with its key, so untouched keys round-trip byte for byte.

/// Parsed markdown file: front matter entries plus body.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Doc {
    /// `(key, raw value)`; the raw value keeps continuation lines. Empty key = verbatim line.
    entries: Vec<(String, String)>,
    had_front_matter: bool,
    pub body: String,
}

impl Doc {
    pub(crate) fn parse(text: &str) -> Self {
        let normalized = text.strip_prefix('\u{feff}').unwrap_or(text);
        let Some(rest) = normalized
            .strip_prefix("---\n")
            .or_else(|| normalized.strip_prefix("---\r\n"))
        else {
            return Self {
                body: normalized.to_owned(),
                ..Self::default()
            };
        };
        let mut entries: Vec<(String, String)> = Vec::new();
        let mut offset = 0;
        let mut closed = None;
        for line in rest.split_inclusive('\n') {
            let content = line.trim_end_matches(['\n', '\r']);
            if content == "---" {
                closed = Some(offset + line.len());
                break;
            }
            offset += line.len();
            let is_key = !content.starts_with([' ', '\t', '-', '#']) && content.contains(':');
            if is_key {
                let (key, value) = content.split_once(':').unwrap_or((content, ""));
                entries.push((key.trim().to_owned(), value.trim_start().to_owned()));
            } else if let Some(last) = entries.last_mut().filter(|(k, _)| !k.is_empty()) {
                if content.starts_with([' ', '\t', '-']) {
                    last.1.push('\n');
                    last.1.push_str(content);
                } else {
                    entries.push((String::new(), content.to_owned()));
                }
            } else {
                entries.push((String::new(), content.to_owned()));
            }
        }
        match closed {
            Some(end) => Self {
                entries,
                had_front_matter: true,
                body: rest[end..].to_owned(),
            },
            // Unterminated front matter: treat the whole file as body.
            None => Self {
                body: normalized.to_owned(),
                ..Self::default()
            },
        }
    }

    /// Scalar value of `key` (quotes removed; block lists joined with `, `).
    pub(crate) fn get(&self, key: &str) -> Option<String> {
        let raw = &self.entries.iter().find(|(k, _)| k == key)?.1;
        let mut lines = raw.lines();
        let first = lines.next().unwrap_or("").trim();
        let items: Vec<String> = lines
            .filter_map(|l| l.trim().strip_prefix('-'))
            .map(|i| unquote(i.trim()))
            .collect();
        if first.is_empty() && !items.is_empty() {
            return Some(items.join(", "));
        }
        let value = unquote(first);
        (!value.is_empty()).then_some(value)
    }

    /// Sets (or removes with `None`) `key`. Unchanged values are left byte for byte.
    pub(crate) fn set(&mut self, key: &str, value: Option<&str>) {
        let position = self.entries.iter().position(|(k, _)| k == key);
        match (position, value) {
            (Some(i), Some(v)) => {
                if self.get(key).as_deref() != Some(v) {
                    self.entries[i].1 = yaml_scalar(v);
                }
            }
            (Some(i), None) => {
                self.entries.remove(i);
            }
            (None, Some(v)) => self.entries.push((key.to_owned(), yaml_scalar(v))),
            (None, None) => {}
        }
    }

    /// Raw value of `key`, continuation lines included (no unquoting).
    ///
    /// Needed by the skill catalog: block scalars (`description: >-`) carry the text in the
    /// continuation lines, which [`Doc::get`] cannot express.
    pub(crate) fn raw(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Entries other than `known` (for `extra_front_matter`), as `(key, raw value)`.
    pub(crate) fn extra(&self, known: &[&str]) -> Vec<(String, String)> {
        self.entries
            .iter()
            .filter(|(k, _)| !k.is_empty() && !known.contains(&k.as_str()))
            .cloned()
            .collect()
    }

    /// Replaces the non-`known` keyed entries by `extra` (raw values), keeping verbatim lines.
    pub(crate) fn set_extra(&mut self, known: &[&str], extra: &[(String, String)]) {
        if self.extra(known) == extra {
            return;
        }
        self.entries
            .retain(|(k, _)| k.is_empty() || known.contains(&k.as_str()));
        for (k, v) in extra {
            if !k.is_empty() && !known.contains(&k.as_str()) {
                self.entries.push((k.clone(), v.clone()));
            }
        }
    }

    pub(crate) fn render(&self) -> String {
        if self.entries.is_empty() && !self.had_front_matter {
            return self.body.clone();
        }
        let mut out = String::from("---\n");
        for (k, v) in &self.entries {
            if k.is_empty() {
                out.push_str(v);
            } else if v.is_empty() || v.starts_with('\n') {
                out.push_str(&format!("{k}:{v}"));
            } else {
                out.push_str(&format!("{k}: {v}"));
            }
            out.push('\n');
        }
        out.push_str("---\n");
        out.push_str(&self.body);
        out
    }
}

pub(crate) fn unquote(value: &str) -> String {
    let v = value.trim();
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        return v[1..v.len() - 1]
            .replace("\\\"", "\"")
            .replace("\\\\", "\\");
    }
    if v.len() >= 2 && v.starts_with('\'') && v.ends_with('\'') {
        return v[1..v.len() - 1].replace("''", "'");
    }
    v.to_owned()
}

fn yaml_scalar(value: &str) -> String {
    let needs_quotes = value.is_empty()
        || value != value.trim()
        || value.contains(": ")
        || value.contains(" #")
        || value.contains('\n')
        || value.starts_with([
            '-', '?', ':', ',', '[', ']', '{', '}', '#', '&', '*', '!', '|', '>', '\'', '"', '%',
            '@', '`',
        ])
        || matches!(
            value.to_ascii_lowercase().as_str(),
            "true" | "false" | "yes" | "no" | "null" | "~" | "on" | "off"
        )
        || value.parse::<f64>().is_ok();
    if needs_quotes {
        format!(
            "\"{}\"",
            value
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n")
        )
    } else {
        value.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_edits() {
        let text = "---\nname: review\n# comment\ndescription: \"Reviews: code\"\ntools:\n  - Read\n  - Grep\ncustom: x\n---\nBody line\n";
        let mut doc = Doc::parse(text);
        assert_eq!(doc.render(), text);
        assert_eq!(doc.get("description").as_deref(), Some("Reviews: code"));
        assert_eq!(doc.get("tools").as_deref(), Some("Read, Grep"));
        doc.set("description", Some("Reviews: code"));
        assert_eq!(doc.render(), text);
        doc.set("model", Some("sonnet"));
        doc.set("custom", None);
        let out = doc.render();
        assert!(out.contains("model: sonnet\n---\nBody line\n"));
        assert!(!out.contains("custom"));
        assert!(out.contains("# comment"));
    }

    #[test]
    fn body_only_files() {
        let doc = Doc::parse("Just a prompt\n");
        assert_eq!(doc.render(), "Just a prompt\n");
        let mut doc = Doc::parse("Just a prompt\n");
        doc.set("description", Some("d"));
        assert_eq!(doc.render(), "---\ndescription: d\n---\nJust a prompt\n");
    }
}
