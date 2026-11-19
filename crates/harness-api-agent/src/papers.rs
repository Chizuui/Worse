//! `paper_search` (`docs/specs/harness/deep-research.md`): scholarly works from `OpenAlex`, Crossref when
//! `OpenAlex` fails. Both are free and need no key.

use std::fmt::Write as _;

use serde_json::Value;

use crate::web_search::api_json;
use crate::web_text::encode;

pub struct Paper {
    pub title: String,
    pub authors: Vec<String>,
    pub year: Option<i64>,
    pub venue: String,
    pub doi: String,
    pub pdf: String,
    pub cited: Option<i64>,
}

pub async fn search(
    http: &reqwest::Client,
    query: &str,
    limit: usize,
    year_from: Option<u64>,
) -> Result<String, String> {
    let papers = match openalex(http, query, limit, year_from).await {
        Ok(found) if !found.is_empty() => found,
        first => {
            let crossref = crossref(http, query, limit, year_from).await;
            match (first, crossref) {
                (_, Ok(found)) if !found.is_empty() => found,
                (Err(a), Err(b)) => return Err(format!("{a}; {b}")),
                _ => return Err("no papers found; retry with other wording".into()),
            }
        }
    };
    Ok(render(&papers))
}

pub fn render(papers: &[Paper]) -> String {
    let mut out = String::new();
    for (index, p) in papers.iter().enumerate() {
        let year = p.year.map(|y| format!(" ({y})")).unwrap_or_default();
        let _ = writeln!(out, "{}. {}{year}", index + 1, p.title);
        let mut meta = Vec::new();
        if !p.authors.is_empty() {
            let mut names = p
                .authors
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            if p.authors.len() > 3 {
                names.push_str(" et al.");
            }
            meta.push(names);
        }
        if !p.venue.is_empty() {
            meta.push(p.venue.clone());
        }
        if let Some(cited) = p.cited {
            meta.push(format!("cited by {cited}"));
        }
        if !meta.is_empty() {
            let _ = writeln!(out, "   {}", meta.join(" · "));
        }
        if !p.doi.is_empty() {
            let _ = writeln!(out, "   DOI: https://doi.org/{}", p.doi);
        }
        if !p.pdf.is_empty() {
            let _ = writeln!(out, "   Open access: {}", p.pdf);
        }
    }
    out
}

fn text(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned()
}

fn bare_doi(doi: &str) -> String {
    doi.trim_start_matches("https://doi.org/").to_owned()
}

async fn openalex(
    http: &reqwest::Client,
    query: &str,
    limit: usize,
    year_from: Option<u64>,
) -> Result<Vec<Paper>, String> {
    let mut url = format!(
        "https://api.openalex.org/works?search={}&per-page={limit}\
         &select=doi,display_name,publication_year,authorships,primary_location,open_access,cited_by_count",
        encode(query)
    );
    if let Some(year) = year_from {
        let _ = write!(url, "&filter=from_publication_date:{year}-01-01");
    }
    let value = api_json(
        http.get(url).header("Accept", "application/json"),
        "OpenAlex",
    )
    .await?;
    Ok(parse_openalex(&value))
}

pub fn parse_openalex(value: &Value) -> Vec<Paper> {
    let Some(items) = value.get("results").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .map(|item| Paper {
            title: text(item.get("display_name")),
            authors: item
                .get("authorships")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .map(|a| text(a.pointer("/author/display_name")))
                        .filter(|n| !n.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
            year: item.get("publication_year").and_then(Value::as_i64),
            venue: text(item.pointer("/primary_location/source/display_name")),
            doi: bare_doi(&text(item.get("doi"))),
            pdf: text(item.pointer("/open_access/oa_url")),
            cited: item.get("cited_by_count").and_then(Value::as_i64),
        })
        .filter(|p| !p.title.is_empty())
        .collect()
}

async fn crossref(
    http: &reqwest::Client,
    query: &str,
    limit: usize,
    year_from: Option<u64>,
) -> Result<Vec<Paper>, String> {
    let mut url = format!(
        "https://api.crossref.org/works?query={}&rows={limit}\
         &select=DOI,title,author,issued,container-title,is-referenced-by-count,link",
        encode(query)
    );
    if let Some(year) = year_from {
        let _ = write!(url, "&filter=from-pub-date:{year}");
    }
    let value = api_json(
        http.get(url).header("Accept", "application/json"),
        "Crossref",
    )
    .await?;
    Ok(parse_crossref(&value))
}

pub fn parse_crossref(value: &Value) -> Vec<Paper> {
    let Some(items) = value.pointer("/message/items").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .map(|item| Paper {
            title: text(item.pointer("/title/0")),
            authors: item
                .get("author")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .map(|a| {
                            format!("{} {}", text(a.get("given")), text(a.get("family")))
                                .trim()
                                .to_owned()
                        })
                        .filter(|n| !n.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
            year: item
                .pointer("/issued/date-parts/0/0")
                .and_then(Value::as_i64),
            venue: text(item.pointer("/container-title/0")),
            doi: text(item.get("DOI")),
            pdf: item
                .get("link")
                .and_then(Value::as_array)
                .and_then(|links| {
                    links
                        .iter()
                        .find(|l| text(l.get("content-type")) == "application/pdf")
                })
                .map(|l| text(l.get("URL")))
                .unwrap_or_default(),
            cited: item.get("is-referenced-by-count").and_then(Value::as_i64),
        })
        .filter(|p| !p.title.is_empty())
        .collect()
}
