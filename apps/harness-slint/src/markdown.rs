//! Markdown → Slint blocks (spec `phase-8-ui.md` §3 "Markdown").
//!
//! Top-level headings, code blocks and tables are split out; everything between them goes through Slint's
//! `StyledText::from_markdown` (paragraphs, lists, emphasis, inline code, links). Chunks that `StyledText`
//! cannot represent fall back to plain text. Code blocks carry highlighted light/dark variants
//! (`crate::highlight`) and a file-type icon for their language header.

use std::borrow::Cow;
use std::rc::Rc;

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use slint::{ModelRc, StyledText, VecModel};

use crate::files::lang_icon_index;
use crate::highlight;
use crate::ui::{MdBlock, MdLink, MdRow};

const KIND_TEXT: i32 = 0;
const KIND_HEADING: i32 = 1;
const KIND_CODE: i32 = 2;
const KIND_TABLE: i32 = 3;
const KIND_LIST: i32 = 4;
const KIND_RULE: i32 = 5;

const INLINE_CODE_LIGHT: &str = "#a45a1f";
const INLINE_CODE_DARK: &str = "#e8b48c";
const TAB_WIDTH: usize = 4;

#[derive(Clone, Default)]
struct Styled {
    light: StyledText,
    dark: StyledText,
    plain: String,
    links: Vec<MdLink>,
}

fn links_model(links: Vec<MdLink>) -> ModelRc<MdLink> {
    ModelRc::from(Rc::new(VecModel::from(links)))
}

pub(crate) fn link_at(links: &ModelRc<MdLink>, offset: i32) -> slint::SharedString {
    use slint::Model;
    links
        .iter()
        .find(|link| link.start <= offset && offset < link.end)
        .map(|link| link.url)
        .unwrap_or_default()
}

fn plain_text(source: &str) -> (String, Vec<MdLink>) {
    let mut paragraphs: Vec<String> = Vec::new();
    let mut current: Option<String> = None;
    let mut lists: Vec<Option<u64>> = Vec::new();
    let mut open: Option<(usize, usize, String)> = None;
    let mut spans: Vec<(usize, usize, usize, String)> = Vec::new();
    let begin = |indent: usize, marker: Option<String>| {
        let mut text = "    ".repeat(indent);
        if let Some(marker) = marker {
            text.push_str(&marker);
        }
        text
    };
    for event in Parser::new_ext(source, Options::ENABLE_STRIKETHROUGH) {
        let indent = lists.len().saturating_sub(1);
        match event {
            Event::SoftBreak | Event::HardBreak | Event::Start(Tag::Paragraph) => {
                if let Some(done) = current.replace(begin(indent, None)) {
                    paragraphs.push(done);
                }
            }
            Event::Start(Tag::List(first)) => lists.push(first),
            Event::End(TagEnd::List(_)) => {
                lists.pop();
            }
            Event::Start(Tag::Item) => {
                let marker = match lists.last().copied() {
                    Some(Some(n)) => format!("{n}. "),
                    _ => match indent % 3 {
                        0 => "\u{2022} ".to_string(),
                        1 => "\u{25e6} ".to_string(),
                        _ => "\u{25aa} ".to_string(),
                    },
                };
                if let Some(Some(n)) = lists.last_mut() {
                    *n += 1;
                }
                if let Some(done) = current.replace(begin(indent, Some(marker))) {
                    paragraphs.push(done);
                }
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                let at = current.as_ref().map_or(0, String::len);
                open = Some((paragraphs.len(), at, dest_url.into_string()));
            }
            Event::End(TagEnd::Link) => {
                if let Some((para, start, url)) = open.take() {
                    if para == paragraphs.len() {
                        let end = current.as_ref().map_or(0, String::len);
                        spans.push((para, start, end, url));
                    }
                }
            }
            Event::Text(text) => current.get_or_insert_with(String::new).push_str(&text),
            Event::Code(text) => current
                .get_or_insert_with(String::new)
                .push_str(&expand_tabs(&text)),
            _ => {}
        }
    }
    paragraphs.extend(current);
    let mut offsets = Vec::with_capacity(paragraphs.len());
    let mut total = 0usize;
    for paragraph in &paragraphs {
        offsets.push(total);
        total += paragraph.len() + 1;
    }
    let links = spans
        .into_iter()
        .filter_map(|(para, start, end, url)| {
            let base = *offsets.get(para)?;
            Some(MdLink {
                start: i32::try_from(base + start).ok()?,
                end: i32::try_from(base + end).ok()?,
                url: url.into(),
            })
        })
        .collect();
    (paragraphs.join("\n"), links)
}

pub(crate) fn expand_tabs(text: &str) -> Cow<'_, str> {
    if !text.contains('\t') {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 16);
    let mut column = 0usize;
    for ch in text.chars() {
        match ch {
            '\t' => {
                let spaces = TAB_WIDTH - column % TAB_WIDTH;
                out.extend(std::iter::repeat_n(' ', spaces));
                column += spaces;
            }
            '\n' => {
                out.push(ch);
                column = 0;
            }
            _ => {
                out.push(ch);
                column += 1;
            }
        }
    }
    Cow::Owned(out)
}

fn code_spans(source: &str) -> Vec<(std::ops::Range<usize>, String)> {
    if !source.contains('`') {
        return Vec::new();
    }
    Parser::new_ext(source, Options::ENABLE_STRIKETHROUGH)
        .into_offset_iter()
        .filter_map(|(event, range)| match event {
            Event::Code(text) => Some((range, text.into_string())),
            _ => None,
        })
        .collect()
}

fn recolor(source: &str, spans: &[(std::ops::Range<usize>, String)], hex: &str) -> String {
    let mut out = String::with_capacity(source.len() + spans.len() * 32);
    let mut last = 0usize;
    for (range, text) in spans {
        if range.start < last || range.end > source.len() {
            continue;
        }
        out.push_str(&source[last..range.start]);
        let text = expand_tabs(text);
        if text.trim().is_empty() {
            out.push_str(&text);
        } else {
            out.push_str("<font color=\"");
            out.push_str(hex);
            out.push_str("\">");
            for ch in text.chars() {
                if ch.is_ascii_punctuation() {
                    out.push('\\');
                }
                out.push(ch);
            }
            out.push_str("</font>");
        }
        last = range.end;
    }
    out.push_str(&source[last..]);
    out
}

fn styled(source: &str) -> Styled {
    let spans = code_spans(source);
    let parsed = if spans.is_empty() {
        StyledText::from_markdown(source)
            .ok()
            .map(|text| (text.clone(), text))
    } else {
        let variant = |hex: &str| StyledText::from_markdown(&recolor(source, &spans, hex)).ok();
        variant(INLINE_CODE_LIGHT).zip(variant(INLINE_CODE_DARK))
    };
    match parsed {
        Some((light, dark)) => {
            let (plain, links) = plain_text(source);
            Styled {
                light,
                dark,
                plain,
                links,
            }
        }
        None => {
            let (text, (plain, links)) = match StyledText::from_markdown(source) {
                Ok(text) => (text, plain_text(source)),
                Err(_) => (
                    StyledText::from_plain_text(source),
                    (source.to_string(), Vec::new()),
                ),
            };
            Styled {
                dark: text.clone(),
                light: text,
                plain,
                links,
            }
        }
    }
}

fn block(kind: i32) -> MdBlock {
    MdBlock {
        kind,
        ..MdBlock::default()
    }
}

fn text_block(source: &str) -> Option<MdBlock> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return None;
    }
    let text = styled(trimmed);
    Some(MdBlock {
        text: text.light,
        text_dark: text.dark,
        plain: text.plain.into(),
        links: links_model(text.links),
        ..block(KIND_TEXT)
    })
}

fn code_block(lang: &str, body: &str) -> MdBlock {
    let code = body.trim_end_matches('\n');
    let shown = expand_tabs(code);
    let light = highlight::styled(&shown, lang, false);
    let dark = highlight::styled(&shown, lang, true);
    let highlighted = light.is_some() && dark.is_some();
    MdBlock {
        code: code.into(),
        plain: shown.as_ref().into(),
        lang: lang.into(),
        lang_icon: lang_icon_index(lang),
        highlighted,
        code_light: light.unwrap_or_default(),
        code_dark: dark.unwrap_or_default(),
        ..block(KIND_CODE)
    }
}

/// Table cell source → styled text (cell ranges include the surrounding pipes on some rows).
fn cell(source: &str) -> Styled {
    let inner = source
        .trim()
        .trim_start_matches('|')
        .trim_end_matches('|')
        .trim();
    // 修复：单元格内容单独交给 StyledText 解析时，"-"、"1." 等会被当成列表/标题/引用等块级标记（"-" 显示成圆点）；
    // 表格单元格只允许行内格式，所以转义行首的块级标记（保留 `*强调*` 等行内语法）。
    let bullet = |c: char| {
        inner
            .strip_prefix(c)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
    };
    if bullet('-') || bullet('*') || bullet('+') || bullet('#') || inner.starts_with('>') {
        return styled(&format!("\\{inner}"));
    }
    if let Some(at) = inner.find(['.', ')']) {
        if at > 0 && inner[..at].bytes().all(|b| b.is_ascii_digit()) {
            return styled(&format!("{}\\{}", &inner[..at], &inner[at..]));
        }
    }
    styled(inner)
}
/// Plain text of a table cell (copy as CSV): inline Markdown markers and the pipe escapes removed.
fn cell_plain(source: &str) -> String {
    let inner = source
        .trim()
        .trim_start_matches('|')
        .trim_end_matches('|')
        .trim();
    let mut out = String::new();
    for event in Parser::new(inner) {
        match event {
            Event::Text(t) | Event::Code(t) => out.push_str(&t),
            Event::SoftBreak | Event::HardBreak => out.push(' '),
            _ => {}
        }
    }
    if out.is_empty() {
        inner.replace("\\|", "|")
    } else {
        out
    }
}

/// RFC 4180 CSV: fields with commas, quotes or line breaks are quoted.
fn csv(rows: &[Vec<String>]) -> String {
    let mut out = String::new();
    for row in rows {
        let fields: Vec<String> = row
            .iter()
            .map(|f| {
                if f.contains([',', '"', '\n', '\r']) {
                    format!("\"{}\"", f.replace('"', "\"\""))
                } else {
                    f.clone()
                }
            })
            .collect();
        out.push_str(&fields.join(","));
        out.push_str("\r\n");
    }
    out
}

fn styled_row(mut cells: Vec<Styled>) -> MdRow {
    let (plain, links) = match cells.first_mut() {
        Some(first) => (
            std::mem::take(&mut first.plain),
            std::mem::take(&mut first.links),
        ),
        None => (String::new(), Vec::new()),
    };
    let (light, dark): (Vec<StyledText>, Vec<StyledText>) =
        cells.into_iter().map(|c| (c.light, c.dark)).unzip();
    MdRow {
        cells: ModelRc::from(Rc::new(VecModel::from(light))),
        cells_dark: ModelRc::from(Rc::new(VecModel::from(dark))),
        plain: plain.into(),
        links: links_model(links),
    }
}

fn table_block(rows: Vec<Vec<Styled>>, plain: &[Vec<String>], source: &str) -> MdBlock {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let rows: Vec<MdRow> = rows
        .into_iter()
        .map(|mut cells| {
            // 短行补空单元格，保证每行列数一致，表格列才能对齐。
            cells.resize_with(columns, Styled::default);
            styled_row(cells)
        })
        .collect();
    MdBlock {
        level: i32::try_from(columns).unwrap_or(i32::MAX),
        rows: ModelRc::from(Rc::new(VecModel::from(rows))),
        code: source.trim_end().into(),
        csv: csv(plain).into(),
        ..block(KIND_TABLE)
    }
}

/// One top-level list item without its marker; continuation lines lose the marker's indent.
fn item_marker(text: &str) -> (usize, usize) {
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    let marker = if text.starts_with(['-', '*', '+']) {
        1
    } else if digits > 0 && text[digits..].starts_with(['.', ')']) {
        digits + 1
    } else {
        0
    };
    let rest = &text[marker..];
    let width = marker + (rest.len() - rest.trim_start_matches(' ').len()).max(1);
    (marker, width)
}

fn dedent(text: &str, width: usize) -> String {
    let mut out = String::new();
    for (i, line) in text.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let indent = line.len() - line.trim_start_matches(' ').len();
        out.push_str(&line[indent.min(width)..]);
    }
    out
}

fn list_item(source: &str) -> Styled {
    let text = source.trim_start();
    let (marker, width) = item_marker(text);
    let rest = text[marker..].trim_start_matches(' ');
    let out = match rest.split_once('\n') {
        Some((first, tail)) => format!("{}\n{}", first.trim_end_matches('\r'), dedent(tail, width)),
        None => rest.to_string(),
    };
    styled(out.trim_end())
}

fn fence_lang(kind: CodeBlockKind<'_>) -> String {
    match kind {
        CodeBlockKind::Fenced(info) => info
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string(),
        CodeBlockKind::Indented => String::new(),
    }
}

fn list_block(ordered: Option<u64>, items: Vec<Styled>) -> MdBlock {
    let rows: Vec<MdRow> = items
        .into_iter()
        .map(|text| styled_row(vec![text]))
        .collect();
    MdBlock {
        lang: if ordered.is_some() { "ol" } else { "ul" }.into(),
        level: i32::try_from(ordered.unwrap_or(1)).unwrap_or(1),
        rows: ModelRc::from(Rc::new(VecModel::from(rows))),
        ..block(KIND_LIST)
    }
}

/// Splits `markdown` into renderable blocks. Called on every streaming flush for the growing message.
// One flat match over the pulldown-cmark events; each arm is short.
#[allow(clippy::too_many_lines)]
pub fn render(markdown: &str) -> Vec<MdBlock> {
    let mut blocks = Vec::new();
    let mut depth = 0usize;
    let mut last_end = 0usize;
    let mut heading: Option<(u8, std::ops::Range<usize>)> = None;
    let mut code: Option<(String, String, std::ops::Range<usize>)> = None;
    // Table rows (first = header) and the table's source range start.
    let mut table: Option<(Vec<Vec<Styled>>, usize)> = None;
    // Plain cell text of the same table (CSV copy).
    let mut plain: Vec<Vec<String>> = Vec::new();
    // Top-level list: ordered start number, items, source start.
    let mut list: Option<(Option<u64>, Vec<Styled>, usize)> = None;
    let mut item: Option<(usize, usize, Option<usize>)> = None;
    let parser = Parser::new_ext(
        markdown,
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH,
    );
    for (event, range) in parser.into_offset_iter() {
        match event {
            Event::Start(Tag::Heading { level, .. }) if depth == 0 => {
                heading = Some((level as u8, range));
                depth += 1;
            }
            Event::Start(Tag::CodeBlock(kind)) if depth == 0 => {
                code = Some((fence_lang(kind), String::new(), range));
                depth += 1;
            }
            Event::Start(Tag::CodeBlock(kind)) if item.is_some() && code.is_none() => {
                if let Some((start, width, cursor)) = item.as_mut() {
                    match *cursor {
                        None => {
                            if let Some((first, items, list_start)) = list.as_mut() {
                                if let Some(last) = items.last_mut() {
                                    *last = list_item(&markdown[*start..range.start]);
                                }
                                blocks.extend(text_block(&markdown[last_end..*list_start]));
                                let done = std::mem::take(items);
                                let count = u64::try_from(done.len()).unwrap_or(0);
                                blocks.push(list_block(*first, done));
                                *first = (*first).map(|n| n + count);
                                *list_start = range.start;
                            }
                        }
                        Some(at) => {
                            blocks.extend(text_block(&dedent(&markdown[at..range.start], *width)));
                        }
                    }
                }
                last_end = range.start;
                code = Some((fence_lang(kind), String::new(), range));
                depth += 1;
            }
            Event::Start(Tag::Table(_)) if depth == 0 => {
                table = Some((Vec::new(), range.start));
                plain.clear();
                depth += 1;
            }
            Event::Start(Tag::TableHead | Tag::TableRow) if table.is_some() => {
                if let Some((rows, _)) = table.as_mut() {
                    rows.push(Vec::new());
                }
                plain.push(Vec::new());
                depth += 1;
            }
            Event::Start(Tag::TableCell) if table.is_some() => {
                if let Some((rows, _)) = table.as_mut() {
                    if let Some(row) = rows.last_mut() {
                        row.push(cell(&markdown[range.clone()]));
                    }
                }
                if let Some(row) = plain.last_mut() {
                    row.push(cell_plain(&markdown[range]));
                }
                depth += 1;
            }
            // 与 ZCode 一致（markdown-list.tsx：列表 my-3、列表项 space-y-1.5）：顶层列表拆成独立块，逐项排版。
            Event::Start(Tag::List(first)) if depth == 0 => {
                list = Some((first, Vec::new(), range.start));
                depth += 1;
            }
            Event::Start(Tag::Item) if depth == 1 && list.is_some() => {
                let source = &markdown[range.clone()];
                if let Some((_, items, _)) = list.as_mut() {
                    items.push(list_item(source));
                }
                item = Some((range.start, item_marker(source.trim_start()).1, None));
                depth += 1;
            }
            Event::Start(_) => depth += 1,
            Event::Rule if depth == 0 => {
                blocks.extend(text_block(&markdown[last_end..range.start]));
                blocks.push(block(KIND_RULE));
                last_end = range.end;
            }
            Event::Text(text) if code.is_some() => {
                if let Some((_, body, _)) = code.as_mut() {
                    body.push_str(&text);
                }
            }
            Event::End(TagEnd::Heading(_)) if depth == 1 => {
                depth = 0;
                if let Some((level, range)) = heading.take() {
                    blocks.extend(text_block(&markdown[last_end..range.start]));
                    let inner = markdown[range.clone()]
                        .trim()
                        .trim_start_matches('#')
                        .trim_end_matches('#')
                        .trim();
                    // StyledText 不支持标题与字重属性：用加粗标记呈现标题字重，字号由 UI 按 level 设置。
                    let text = if inner.is_empty() {
                        Styled::default()
                    } else {
                        styled(&format!("**{inner}**"))
                    };
                    blocks.push(MdBlock {
                        level: i32::from(level),
                        text: text.light,
                        text_dark: text.dark,
                        plain: text.plain.into(),
                        links: links_model(text.links),
                        ..block(KIND_HEADING)
                    });
                    last_end = range.end;
                }
            }
            Event::End(TagEnd::CodeBlock) if depth == 1 => {
                depth = 0;
                if let Some((lang, body, range)) = code.take() {
                    blocks.extend(text_block(&markdown[last_end..range.start]));
                    blocks.push(code_block(&lang, &body));
                    last_end = range.end;
                }
            }
            Event::End(TagEnd::CodeBlock) if item.is_some() && code.is_some() => {
                depth = depth.saturating_sub(1);
                if let Some((lang, body, _)) = code.take() {
                    blocks.push(code_block(&lang, &body));
                }
                if let Some((_, _, cursor)) = item.as_mut() {
                    *cursor = Some(range.end);
                }
                if let Some((_, _, list_start)) = list.as_mut() {
                    *list_start = range.end;
                }
                last_end = range.end;
            }
            Event::End(TagEnd::Item) if depth == 2 && item.is_some() => {
                depth = 1;
                if let Some((_, width, Some(at))) = item.take() {
                    blocks.extend(text_block(&dedent(&markdown[at..range.end], width)));
                    if let Some((_, _, list_start)) = list.as_mut() {
                        *list_start = range.end;
                    }
                    last_end = range.end;
                }
            }
            Event::End(TagEnd::Table) if depth == 1 => {
                depth = 0;
                if let Some((rows, start)) = table.take() {
                    blocks.extend(text_block(&markdown[last_end..start]));
                    blocks.push(table_block(rows, &plain, &markdown[start..range.end]));
                    last_end = range.end;
                }
            }
            Event::End(TagEnd::List(_)) if depth == 1 && list.is_some() => {
                depth = 0;
                item = None;
                if let Some((first, items, start)) = list.take() {
                    blocks.extend(text_block(&markdown[last_end..start]));
                    if !items.is_empty() {
                        blocks.push(list_block(first, items));
                    }
                    last_end = range.end;
                }
            }
            Event::End(_) => {
                depth = depth.saturating_sub(1);
                // 修复（段落间距与 ZCode 不一致）：多个段落放在同一个 StyledText 里时，段间距由 Slint 决定且偏小；
                // 每个顶层块结束时单独成块，块间距统一为 MarkdownView 的 12px（ZCode 段落 my-3）。
                if depth == 0 {
                    blocks.extend(text_block(&markdown[last_end..range.end]));
                    last_end = range.end;
                }
            }
            _ => {}
        }
    }
    // 流式输出中代码块尚未闭合时，pulldown-cmark 仍会给出 End 事件，这里只处理剩余文本。
    blocks.extend(text_block(&markdown[last_end.min(markdown.len())..]));
    blocks
}
