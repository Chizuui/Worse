//! Web tools of the API agent for deep research (docs/specs/harness/deep-research.md): `web_search`
//! (Tavily or Brave Search when their key is set, else DuckDuckGo HTML) and `web_fetch` (one page as text).

use std::fmt::Write as _;
use std::time::Duration;

use serde_json::{json, Value};

use crate::web_text::{decode, encode, entities, html_text, inline_text, page_title};

pub const NAMES: [&str; 2] = ["web_search", "web_fetch"];
const SEARCH_URL: &str = "https://html.duckduckgo.com/html/";
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36";
const TIMEOUT: Duration = Duration::from_secs(30);
const BODY_MAX: usize = 4 * 1024 * 1024;
const PAGE_CHARS: usize = 20_000;
const RESULTS_DEFAULT: usize = 8;
const RESULTS_MAX: usize = 20;

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
            "name": "web_fetch",
            "description": "Fetch a web page (http or https) and return its title and readable text, 20000 characters \
                at a time. When the result says more text follows, call again with the given offset.",
            "parameters": {"type": "object", "properties": {
                "url": {"type": "string"},
                "offset": {"type": "integer", "description": "Character offset to continue from, default 0"}
            }, "required": ["url"]}
        }}),
    ]
}

pub fn title(name: &str, args: &Value) -> String {
    let arg = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
    match name {
        "web_search" => format!("Web search {}", arg("query")),
        _ => format!("Fetch {}", arg("url")),
    }
}

pub async fn run(http: &reqwest::Client, name: &str, args: &Value) -> Result<String, String> {
    match name {
        "web_search" => {
            let query = args.get("query").and_then(Value::as_str).unwrap_or_default().trim();
            if query.is_empty() {
                return Err("query is empty".into());
            }
            let limit = args
                .get("max_results")
                .and_then(Value::as_u64)
                .map_or(RESULTS_DEFAULT, |n| usize::try_from(n).unwrap_or(RESULTS_MAX))
                .clamp(1, RESULTS_MAX);
            search(http, query, limit).await
        }
        _ => {
            let url = args.get("url").and_then(Value::as_str).unwrap_or_default().trim();
            let offset = args
                .get("offset")
                .and_then(Value::as_u64)
                .map_or(0, |n| usize::try_from(n).unwrap_or(usize::MAX));
            fetch(http, url, offset).await
        }
    }
}

async fn get(http: &reqwest::Client, url: &str) -> Result<(String, String), String> {
    let mut response = http
        .get(url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "text/html,application/xhtml+xml,text/plain;q=0.9,*/*;q=0.5")
        .timeout(TIMEOUT)
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
    while let Some(chunk) = response.chunk().await.map_err(|e| format!("read failed: {e}"))? {
        body.extend_from_slice(&chunk);
        if body.len() >= BODY_MAX {
            body.truncate(BODY_MAX);
            break;
        }
    }
    Ok((kind, String::from_utf8_lossy(&body).into_owned()))
}

type Hit = (String, String, String);

async fn search(http: &reqwest::Client, query: &str, limit: usize) -> Result<String, String> {
    let results = if let Some(key) = env_key("TAVILY_API_KEY") {
        tavily(http, &key, query, limit).await?
    } else if let Some(key) = env_key("BRAVE_SEARCH_API_KEY") {
        brave(http, &key, query, limit).await?
    } else {
        let url = format!("{SEARCH_URL}?q={}", encode(query));
        let (_, html) = get(http, &url).await.map_err(|e| {
            format!(
                "{e}. DuckDuckGo is unreachable from this network: set TAVILY_API_KEY or \
                 BRAVE_SEARCH_API_KEY for Harness, or use an agent with its own web search"
            )
        })?;
        parse_results(&html, limit)
    };
    if results.is_empty() {
        return Err("no results; retry with other wording".into());
    }
    let mut out = String::new();
    for (index, (title, link, snippet)) in results.iter().enumerate() {
        let _ = writeln!(out, "{}. {title}\n   {link}", index + 1);
        if !snippet.is_empty() {
            let _ = writeln!(out, "   {snippet}");
        }
    }
    Ok(out)
}

fn env_key(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

async fn api_json(request: reqwest::RequestBuilder, service: &str) -> Result<Value, String> {
    let response = request
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("{service} request failed: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("{service} returned HTTP {status}"));
    }
    response
        .json()
        .await
        .map_err(|e| format!("{service} answer is not JSON: {e}"))
}

async fn tavily(http: &reqwest::Client, key: &str, query: &str, limit: usize) -> Result<Vec<Hit>, String> {
    let body = json!({"query": query, "max_results": limit, "search_depth": "basic"});
    let value = api_json(
        http.post("https://api.tavily.com/search").bearer_auth(key).json(&body),
        "Tavily",
    )
    .await?;
    Ok(hits(value.get("results"), "content", limit))
}

async fn brave(http: &reqwest::Client, key: &str, query: &str, limit: usize) -> Result<Vec<Hit>, String> {
    let url = format!(
        "https://api.search.brave.com/res/v1/web/search?q={}&count={limit}",
        encode(query)
    );
    let value = api_json(
        http.get(url)
            .header("Accept", "application/json")
            .header("X-Subscription-Token", key),
        "Brave Search",
    )
    .await?;
    Ok(hits(value.pointer("/web/results"), "description", limit))
}

fn hits(list: Option<&Value>, snippet_key: &str, limit: usize) -> Vec<Hit> {
    let text = |item: &Value, key: &str| inline_text(item.get(key).and_then(Value::as_str).unwrap_or_default());
    list.and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let link = item.get("url").and_then(Value::as_str)?.to_owned();
                    Some((text(item, "title"), link, text(item, snippet_key)))
                })
                .take(limit)
                .collect()
        })
        .unwrap_or_default()
}
fn parse_results(html: &str, limit: usize) -> Vec<Hit> {
    let mut out = Vec::new();
    for block in html.split("class=\"result__a\"").skip(1) {
        if out.len() >= limit {
            break;
        }
        let Some(href) = attr(block, "href") else {
            continue;
        };
        let link = result_link(&href);
        if !link.starts_with("http") || link.contains("duckduckgo.com/y.js") {
            continue;
        }
        let title = block
            .split_once('>')
            .and_then(|(_, rest)| rest.split_once("</a>"))
            .map(|(inner, _)| inline_text(inner))
            .unwrap_or_default();
        let snippet = block
            .split_once("result__snippet")
            .and_then(|(_, rest)| rest.split_once('>'))
            .and_then(|(_, rest)| rest.split_once("</a>").or_else(|| rest.split_once("</div>")))
            .map(|(inner, _)| inline_text(inner))
            .unwrap_or_default();
        out.push((title, link, snippet));
    }
    out
}

fn attr(block: &str, name: &str) -> Option<String> {
    let start = block.find(&format!("{name}=\""))? + name.len() + 2;
    let end = block[start..].find('"')? + start;
    Some(entities(&block[start..end]))
}

fn result_link(href: &str) -> String {
    if let Some(index) = href.find("uddg=") {
        let rest = &href[index + 5..];
        let value = rest.split('&').next().unwrap_or_default();
        return decode(value);
    }
    if href.starts_with("//") {
        return format!("https:{href}");
    }
    href.to_owned()
}

async fn fetch(http: &reqwest::Client, url: &str, offset: usize) -> Result<String, String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("only http and https URLs can be fetched".into());
    }
    let (kind, body) = get(http, url).await?;
    let (title, text) = if kind.contains("html") || (kind.is_empty() && body.trim_start().starts_with('<')) {
        (page_title(&body), html_text(&body))
    } else if kind.starts_with("text/") || kind.contains("json") || kind.contains("xml") {
        (String::new(), body)
    } else {
        return Err(format!("{kind} content is not text"));
    };
    let chars: Vec<char> = text.chars().collect();
    if offset >= chars.len() && offset > 0 {
        return Err(format!("offset {offset} is past the end ({} characters)", chars.len()));
    }
    let end = (offset + PAGE_CHARS).min(chars.len());
    let mut out = String::new();
    if !title.is_empty() {
        let _ = writeln!(out, "Title: {title}");
    }
    let _ = writeln!(out, "URL: {url}\n");
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
