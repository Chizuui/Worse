//! Plain text from HTML and URL encoding for the web tools (`web.rs`).

use std::fmt::Write as _;

pub fn page_title(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let Some(start) = lower.find("<title") else {
        return String::new();
    };
    let Some(open) = lower[start..].find('>') else {
        return String::new();
    };
    let from = start + open + 1;
    let to = lower[from..].find("</title>").map_or(from, |i| from + i);
    inline_text(&html[from..to])
}

const SKIP: [&str; 6] = ["script", "style", "noscript", "svg", "template", "head"];
const BLOCK: [&str; 22] = [
    "p",
    "div",
    "br",
    "li",
    "ul",
    "ol",
    "tr",
    "table",
    "section",
    "article",
    "header",
    "footer",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "pre",
    "blockquote",
    "main",
    "nav",
];

pub fn html_text(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len() / 3);
    let mut i = 0;
    while i < html.len() {
        let Some(rel) = html[i..].find('<') else {
            out.push_str(&entities(&html[i..]));
            break;
        };
        out.push_str(&entities(&html[i..i + rel]));
        let tag_start = i + rel;
        let Some(close) = html[tag_start..].find('>') else {
            break;
        };
        let tag_end = tag_start + close + 1;
        let inner = &lower[tag_start + 1..tag_end - 1];
        let closing = inner.starts_with('/');
        let name: String = inner
            .trim_start_matches('/')
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .collect();
        i = tag_end;
        if inner.starts_with("!--") {
            i = lower[tag_start..]
                .find("-->")
                .map_or(html.len(), |e| tag_start + e + 3);
            continue;
        }
        if !closing && SKIP.contains(&name.as_str()) {
            let end_tag = format!("</{name}");
            i = lower[i..].find(&end_tag).map_or(html.len(), |e| {
                let at = i + e;
                lower[at..].find('>').map_or(html.len(), |g| at + g + 1)
            });
            continue;
        }
        if BLOCK.contains(&name.as_str()) && !(closing && name == "li") {
            out.push('\n');
            if name == "li" && !closing {
                out.push_str("- ");
            }
        } else if name == "td" || name == "th" {
            out.push(' ');
        }
    }
    tidy(&out)
}

fn tidy(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank = 0;
    for line in text.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(&line);
        out.push('\n');
    }
    out.trim().to_owned()
}

pub fn inline_text(html: &str) -> String {
    tidy(&html_text(html)).replace('\n', " ")
}

pub fn entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        let Some(semi) = tail[..tail.len().min(12)].find(';') else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let name = &tail[1..semi];
        let decoded = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            _ => name
                .strip_prefix("#x")
                .or_else(|| name.strip_prefix("#X"))
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .or_else(|| name.strip_prefix('#').and_then(|dec| dec.parse().ok()))
                .and_then(char::from_u32),
        };
        if let Some(c) = decoded {
            out.push(c);
            rest = &tail[semi + 1..];
        } else {
            out.push('&');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    out
}

pub fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 3);
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(byte));
            }
            b' ' => out.push('+'),
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

pub fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(value) = u8::from_str_radix(hex, 16) {
                    out.push(value);
                    i += 3;
                    continue;
                }
                out.push(b'%');
            }
            b'+' => out.push(b' '),
            other => out.push(other),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
