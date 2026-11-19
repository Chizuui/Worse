//! Web tools of the API agent for deep research (docs/specs/harness/deep-research.md): `web_search`
//! (backends in `web_search.rs`), `paper_search` (`papers.rs`) and `web_fetch` (one page or PDF as text).

use std::fmt::Write as _;
use std::time::Duration;

use serde_json::{json, Value};

use crate::web_search::READER;
use crate::web_text::{html_text, page_title};

pub const NAMES: [&str; 3] = ["web_search", "paper_search", "web_fetch"];
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36";
pub const TIMEOUT: Duration = Duration::from_secs(30);
const READER_TIMEOUT: Duration = Duration::from_secs(60);
const BODY_MAX: usize = 4 * 1024 * 1024;
const PAGE_CHARS: usize = 20_000;
const RESULTS_DEFAULT: usize = 8;
const RESULTS_MAX: usize = 20;

pub type Hit = (String, String, String);

pub fn handles(name: &str) -> bool {
    NAMES.contains(&name)
}

pub fn definitions() -> Vec<Value> {
    vec![
        json!({"type": "function", "function": {
            "name": "web_search",
            "description": "Search the web. Returns numbered results with title, URL and snippet. Snippets can be \
                outdated or wrong: open the pages that matter with web_fetch.",
            "parameters": {"type": "object", "properties": {
                "query": {"type": "string"},
                "max_results": {"type": "integer", "description": "Default 8, at most 20"}
            }, "required": ["query"]}
        }}),
        json!({"type": "function", "function": {
            "name": "paper_search",
            "description": "Search scholarly papers (OpenAlex, Crossref). Returns title, year, authors, venue, \
                citation count, DOI and an open-access PDF link when one exists. Read a paper with web_fetch on \
                its PDF or DOI link.",
            "parameters": {"type": "object", "properties": {
                "query": {"type": "string"},
                "max_results": {"type": "integer", "description": "Default 8, at most 20"},
                "year_from": {"type": "integer", "description": "Only papers published in or after this year"}
            }, "required": ["query"]}
        }}),
        json!({"type": "function", "function": {
            "name": "web_fetch",
            "description": "Fetch a web page or PDF (http or https) and return its title and readable text, 20000 \
                characters at a time. When the result says more text follows, call again with the given offset.",
            "parameters": {"type": "object", "properties": {
                "url": {"type": "string"},
                "offset": {"type": "integer", "description": "Character offset to continue from, default 0"}
            }, "required": ["url"]}
        }}),
    ]
}

pub fn title(name: &str, args: &Value) -> String {
    let arg = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    match name {
        "web_search" => format!("Web search {}", arg("query")),
        "paper_search" => format!("Paper search {}", arg("query")),
        _ => format!("Fetch {}", arg("url")),
    }
}

pub async fn run(http: &reqwest::Client, name: &str, args: &Value) -> Result<String, String> {
    let limit = args
        .get("max_results")
        .and_then(Value::as_u64)
        .map_or(RESULTS_DEFAULT, |n| {
            usize::try_from(n).unwrap_or(RESULTS_MAX)
        })
        .clamp(1, RESULTS_MAX);
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    match name {
        "web_search" | "paper_search" if query.is_empty() => Err("query is empty".into()),
        "web_search" => Ok(render(
            &crate::web_search::search(http, query, limit).await?,
        )),
        "paper_search" => {
            let year_from = args.get("year_from").and_then(Value::as_u64);
            crate::papers::search(http, query, limit, year_from).await
        }
        _ => {
            let url = args
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim();
            let offset = args
                .get("offset")
                .and_then(Value::as_u64)
                .map_or(0, |n| usize::try_from(n).unwrap_or(usize::MAX));
            fetch(http, url, offset).await
        }
    }
}

fn render(results: &[Hit]) -> String {
    let mut out = String::new();
    for (index, (title, link, snippet)) in results.iter().enumerate() {
        let _ = writeln!(out, "{}. {title}\n   {link}", index + 1);
        if !snippet.is_empty() {
            let _ = writeln!(out, "   {snippet}");
        }
    }
    out
}

pub async fn get(http: &reqwest::Client, url: &str) -> Result<(String, String), String> {
    get_with(http, url, &[]).await
}

pub async fn get_with(
    http: &reqwest::Client,
    url: &str,
    headers: &[(&str, &str)],
) -> Result<(String, String), String> {
    let mut request = http
        .get(url)
        .header("User-Agent", USER_AGENT)
        .header(
            "Accept",
            "text/html,application/xhtml+xml,text/plain;q=0.9,*/*;q=0.5",
        )
        .header("Accept-Language", "en-US,en;q=0.9")
        .timeout(if url.starts_with(READER) {
            READER_TIMEOUT
        } else {
            TIMEOUT
        });
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let mut response = request
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    let kind = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("read failed: {e}"))?
    {
        body.extend_from_slice(&chunk);
        if body.len() >= BODY_MAX {
            body.truncate(BODY_MAX);
            break;
        }
    }
    Ok((kind, String::from_utf8_lossy(&body).into_owned()))
}

fn readable(kind: &str, body: String) -> Option<(String, String)> {
    if kind.contains("html") || (kind.is_empty() && body.trim_start().starts_with('<')) {
        Some((page_title(&body), html_text(&body)))
    } else if kind.starts_with("text/") || kind.contains("json") || kind.contains("xml") {
        Some((String::new(), body))
    } else {
        None
    }
}

async fn fetch(http: &reqwest::Client, url: &str, offset: usize) -> Result<String, String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("only http and https URLs can be fetched".into());
    }
    let direct = match get(http, url).await {
        Ok((kind, body)) => readable(&kind, body).ok_or(format!("{kind} content is not text")),
        Err(e) => Err(e),
    };
    let (title, text, via) = match direct {
        Ok((title, text)) if !text.trim().is_empty() => (title, text, ""),
        failed => {
            let first = failed
                .err()
                .unwrap_or_else(|| "page has no text".to_owned());
            let (_, body) = get(http, &format!("{READER}{url}"))
                .await
                .map_err(|e| format!("{first}; r.jina.ai reader: {e}"))?;
            (String::new(), body, " (read through r.jina.ai)")
        }
    };
    let chars: Vec<char> = text.chars().collect();
    if offset >= chars.len() && offset > 0 {
        return Err(format!(
            "offset {offset} is past the end ({} characters)",
            chars.len()
        ));
    }
    let end = (offset + PAGE_CHARS).min(chars.len());
    let mut out = String::new();
    if !title.is_empty() {
        let _ = writeln!(out, "Title: {title}");
    }
    let _ = writeln!(out, "URL: {url}{via}\n");
    out.extend(&chars[offset..end]);
    if end < chars.len() {
        let _ = write!(
            out,
            "\n\n[{} more characters; call web_fetch with offset {end} to continue]",
            chars.len() - end
        );
    }
    Ok(out)
}

#[cfg(test)]
#[path = "web_tests.rs"]
mod tests;
