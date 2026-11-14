//! Derived view data of the Instructions / Memory pages (design S7/S8): line count, unsaved diff against the
//! saved text, and the memory "entries" (one markdown section per entry). Pure helpers; `mod.rs` owns the state.

use std::path::Path;

use slint::SharedString;

use super::{diff_lines, model, sv};
use crate::ui::{DiffLine, DocEntry, DocUi};

/// One section of the document: `[start, end)` line range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Section {
    pub start: usize,
    pub end: usize,
}

fn is_heading(line: &str) -> bool {
    line.trim_start().starts_with('#')
}

fn clean(line: &str) -> String {
    line.trim()
        .trim_start_matches('#')
        .trim_start_matches("- ")
        .trim_start_matches("* ")
        .trim()
        .to_owned()
}

/// Splits `text` into sections: each heading starts one; a non-blank preamble before the first heading is its
/// own section.
pub(super) fn sections(text: &str) -> Vec<Section> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut start = 0;
    for (i, line) in lines.iter().enumerate() {
        if is_heading(line) && i > start {
            if lines[start..i].iter().any(|l| !l.trim().is_empty()) {
                out.push(Section { start, end: i });
            }
            start = i;
        }
    }
    if start < lines.len() && lines[start..].iter().any(|l| !l.trim().is_empty()) {
        out.push(Section {
            start,
            end: lines.len(),
        });
    }
    out
}

fn section_text(lines: &[&str], s: Section) -> String {
    lines[s.start..s.end].join("\n")
}

/// Text of section `index`, empty when out of range.
pub(super) fn entry_text(text: &str, index: i32) -> String {
    let lines: Vec<&str> = text.lines().collect();
    usize::try_from(index)
        .ok()
        .and_then(|i| sections(text).get(i).copied())
        .map(|s| section_text(&lines, s))
        .unwrap_or_default()
}

/// `text` with section `index` replaced by `replacement` (`None` removes it).
pub(super) fn replace_entry(text: &str, index: i32, replacement: Option<&str>) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let Some(s) = usize::try_from(index)
        .ok()
        .and_then(|i| sections(text).get(i).copied())
    else {
        return text.to_owned();
    };
    let mut out: Vec<&str> = lines[..s.start].to_vec();
    if let Some(r) = replacement {
        out.extend(r.split('\n'));
    }
    out.extend_from_slice(&lines[s.end..]);
    let mut joined = out.join("\n");
    if text.ends_with('\n') && !joined.is_empty() {
        joined.push('\n');
    }
    joined
}

/// `text` with a new empty memory section appended; returns the new text and the new section index.
pub(super) fn append_entry(text: &str) -> (String, i32) {
    let mut t = text.trim_end().to_owned();
    if !t.is_empty() {
        t.push_str("\n\n");
    }
    t.push_str("## New memory\n- ");
    let index = i32::try_from(sections(&t).len()).unwrap_or(1) - 1;
    (t, index)
}

/// Recomputes every derived property from the current text.
pub(super) fn refresh(ui: &DocUi, original: &str, path: &Path) {
    let text = ui.get_text().to_string();
    ui.set_file_name(sv(path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()));
    let count = text.lines().count() + usize::from(text.ends_with('\n') || text.is_empty());
    ui.set_line_count(i32::try_from(count.max(1)).unwrap_or(i32::MAX));

    let changes: Vec<DiffLine> = if text == original {
        Vec::new()
    } else {
        let diff = harness_agent_config::unified_diff("", original, &text);
        diff_lines(&diff)
            .into_iter()
            .filter(|l| l.kind == 1 || l.kind == 2)
            .map(|l| {
                let body = l.text.get(1..).unwrap_or_default();
                let sign = if l.kind == 1 { "+" } else { "-" };
                DiffLine {
                    kind: l.kind,
                    text: sv(format!("{sign} {body}")),
                }
            })
            .collect()
    };
    let added = changes.iter().filter(|l| l.kind == 1).count();
    let removed = changes.len() - added;
    ui.set_added(i32::try_from(added).unwrap_or(i32::MAX));
    ui.set_removed(i32::try_from(removed).unwrap_or(i32::MAX));
    ui.set_changes(model(changes));

    let query = ui.get_query().to_lowercase();
    let lines: Vec<&str> = text.lines().collect();
    let all = sections(&text);
    ui.set_entry_count(i32::try_from(all.len()).unwrap_or(i32::MAX));
    let entries: Vec<DocEntry> = all
        .iter()
        .enumerate()
        .filter(|(_, s)| query.is_empty() || section_text(&lines, **s).to_lowercase().contains(&query))
        .map(|(i, s)| {
            let body = &lines[s.start..s.end];
            let title = body
                .iter()
                .find(|l| !l.trim().is_empty())
                .map(|l| clean(l))
                .unwrap_or_default();
            let first_is_heading = body.iter().find(|l| !l.trim().is_empty()).is_some_and(|l| is_heading(l));
            let detail = body
                .iter()
                .filter(|l| !l.trim().is_empty())
                .nth(usize::from(first_is_heading))
                .map(|l| clean(l))
                .unwrap_or_default();
            DocEntry {
                title: sv(title),
                detail: sv(detail),
                index: i32::try_from(i).unwrap_or(i32::MAX),
            }
        })
        .collect();
    ui.set_entries(model(entries));
    let selected = ui.get_selected();
    if usize::try_from(selected).map_or(selected >= 0, |s| s >= all.len()) {
        ui.set_selected(-1);
    }
    // The entry editor owns `entry-text` while a section is selected; only (re)select here.
    if ui.get_selected() < 0 {
        if ui.get_kind() == "memory" && !all.is_empty() {
            ui.set_selected(0);
            ui.set_entry_text(sv(entry_text(&text, 0)));
        } else {
            ui.set_entry_text(SharedString::new());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_split_on_headings() {
        let t = "intro\n\n## A\n- a1\n## B\nb";
        let s = sections(t);
        assert_eq!(s.len(), 3);
        assert_eq!(entry_text(t, 1), "## A\n- a1");
        assert_eq!(replace_entry(t, 1, None), "intro\n\n## B\nb");
        assert_eq!(replace_entry(t, 2, Some("## C")), "intro\n\n## A\n- a1\n## C");
        let (n, i) = append_entry(t);
        assert_eq!(i, 3);
        assert!(n.ends_with("## New memory\n- "));
    }
}
