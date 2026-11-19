//! `web_search` backends (`docs/specs/harness/deep-research.md`): Tavily or Brave Search API when their key
//! is set; else `DuckDuckGo` HTML, Brave Search's web page, then `DuckDuckGo` read through the `r.jina.ai` reader.

use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{json, Value};

use crate::web::{get, get_with, Hit};
use crate::web_text::{decode, encode, entities, inline_text};

const DDG_URL: &str = "https://html.duckduckgo.com/html/";
const BRAVE_URL: &str = "https://search.brave.com/search";
pub const READER: &str = "https://r.jina.ai/";

static DDG_DOWN: AtomicBool = AtomicBool::new(false);

pub async fn search(http: &reqwest::Client, query: &str, limit: usize) -> Result<Vec<Hit>, String> {
    if let Some(key) = env_key("TAVILY_API_KEY") {
        return tavily(http, &key, query, limit).await;
    }
    if let Some(key) = env_key("BRAVE_SEARCH_API_KEY") {
        return brave_api(http, &key, query, limit).await;
    }
    let mut errors = Vec::new();
    if !DDG_DOWN.load(Ordering::Relaxed) {
        match get(http, &format!("{DDG_URL}?q={}", encode(query))).await {
            Ok((_, html)) => {
                let hits = parse_ddg(&html, limit);
                if !hits.is_empty() {
                    return Ok(hits);
                }
                errors.push("DuckDuckGo: no results".to_owned());
            }
            Err(e) => {
                if e.starts_with("request failed") {
                    DDG_DOWN.store(true, Ordering::Relaxed);
                }
                errors.push(format!("DuckDuckGo: {e}"));
            }
        }
    }
    match get(http, &format!("{BRAVE_URL}?q={}&source=web", encode(query))).await {
        Ok((_, html)) => {
            let hits = parse_brave(&html, limit);
            if !hits.is_empty() {
                return Ok(hits);
            }
            errors.push("Brave Search: no results".to_owned());
        }
        Err(e) => errors.push(format!("Brave Search: {e}")),
    }
    let reader = format!("{READER}{DDG_URL}?q={}", encode(query));
    match get_with(http, &reader, &[("X-Return-Format", "html")]).await {
        Ok((_, html)) => {
            let hits = parse_ddg(&html, limit);
            if !hits.is_empty() {
                return Ok(hits);
            }
            errors.push("DuckDuckGo via r.jina.ai: no results".to_owned());
        }
        Err(e) => errors.push(format!("DuckDuckGo via r.jina.ai: {e}")),
    }
    Err(format!(
        "every search backend failed ({}). Retry with other wording, use paper_search for papers, or set \
         TAVILY_API_KEY or BRAVE_SEARCH_API_KEY for Harness",
        errors.join("; ")
    ))
}

fn env_key(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

pub async fn api_json(request: reqwest::RequestBuilder, service: &str) -> Result<Value, String> {
    let response = request
        .timeout(crate::web::TIMEOUT)
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

async fn tavily(
    http: &reqwest::Client,
    key: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<Hit>, String> {
    let body = json!({"query": query, "max_results": limit, "search_depth": "basic"});
    let value = api_json(
        http.post("https://api.tavily.com/search")
            .bearer_auth(key)
            .json(&body),
        "Tavily",
    )
    .await?;
    Ok(hits(value.get("results"), "content", limit))
}

async fn brave_api(
    http: &reqwest::Client,
    key: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<Hit>, String> {
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
    let text = |item: &Value, key: &str| {
        inline_text(item.get(key).and_then(Value::as_str).unwrap_or_default())
    };
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

pub fn parse_ddg(html: &str, limit: usize) -> Vec<Hit> {
    let mut out = Vec::new();
    for block in html.split("class=\"result__a\"").skip(1) {
        if out.len() >= limit {
            break;
        }
        let Some(href) = attr(block, "href") else {
            continue;
        };
        let link = ddg_link(&href);
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
            .and_then(|(_, rest)| {
                rest.split_once("</a>")
                    .or_else(|| rest.split_once("</div>"))
            })
            .map(|(inner, _)| inline_text(inner))
            .unwrap_or_default();
        out.push((title, link, snippet));
    }
    out
}

pub fn parse_brave(html: &str, limit: usize) -> Vec<Hit> {
    let mut out = Vec::new();
    for block in html.split("data-type=\"web\"").skip(1) {
        if out.len() >= limit {
            break;
        }
        let Some(link) = attr(block, "href").filter(|l| l.starts_with("http")) else {
            continue;
        };
        if link.contains("search.brave.com") {
            continue;
        }
        let title = block
            .split_once("search-snippet-title")
            .and_then(|(_, rest)| attr(rest, "title"))
            .unwrap_or_default();
        let snippet = block
            .split_once("class=\"content ")
            .and_then(|(_, rest)| rest.split_once('>'))
            .and_then(|(_, rest)| rest.split_once("</div>"))
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

fn ddg_link(href: &str) -> String {
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
