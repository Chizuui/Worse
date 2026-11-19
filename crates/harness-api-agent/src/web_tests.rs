#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::papers::{parse_crossref, parse_openalex, render as render_papers};
use crate::web_search::{parse_brave, parse_ddg as parse_results};
use crate::web_text::{decode, encode};

#[test]
fn search_results_are_parsed_and_redirects_decoded() {
    let html = r#"<div class="result"><h2><a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdocs.rs%2Freqwest%3Fa%3D1%26b%3D2&amp;rut=abc">Reqwest <b>docs</b></a></h2>
<a class="result__snippet" href="x">An ergonomic &amp; batteries-included HTTP client.</a></div>
<div class="result"><a class="result__a" href="https://duckduckgo.com/y.js?ad=1">Ad</a></div>
<div class="result"><a class="result__a" href="https://example.com/b">Second</a><div class="result__snippet">Plain</div></div>"#;
    let results = parse_results(html, 8);
    assert_eq!(results.len(), 2, "{results:?}");
    assert_eq!(results[0].0, "Reqwest docs");
    assert_eq!(results[0].1, "https://docs.rs/reqwest?a=1&b=2");
    assert_eq!(
        results[0].2,
        "An ergonomic & batteries-included HTTP client."
    );
    assert_eq!(results[1].1, "https://example.com/b");
    assert_eq!(parse_results(html, 1).len(), 1);
}

#[test]
fn html_becomes_readable_text() {
    let html = "<html><head><title>T &amp; U</title><style>p{}</style></head><body>\
        <script>var x = '<p>no</p>';</script><!-- hidden --><h1>Heading</h1>\
        <p>One <b>bold</b>&nbsp;word</p><ul><li>a</li><li>b</li></ul><p>&#169; &#x41;</p></body></html>";
    assert_eq!(page_title(html), "T & U");
    let text = html_text(html);
    assert!(
        !text.contains("var x") && !text.contains("hidden") && !text.contains("p{}"),
        "{text}"
    );
    assert!(text.contains("Heading\n"), "{text}");
    assert!(text.contains("One bold word"), "{text}");
    assert!(text.contains("- a\n- b"), "{text}");
    assert!(text.contains("© A"), "{text}");
}

#[test]
fn query_encoding_round_trips() {
    let query = "rust ACP & \"deep research\"/ü";
    assert_eq!(decode(&encode(query)), query);
    assert!(!encode(query).contains(' '));
}

#[test]
fn brave_search_results_are_parsed() {
    let html = r#"<div class="snippet svelte-x" data-type="web" data-keynav="true"><div class="result-content"><a href="https://theaviationist.com/b-2-flutter/" class="l1"><img src="https://imgs.search.brave.com/x"/><cite class="snippet-url">theaviationist.com</cite><div class="title search-snippet-title line-clamp-1" title="B-2 Wing Flutter &amp; Video">B-2 Wing Flutter</div></a><div class="generic-snippet"><div class="content desktop-default-regular t-primary"><span class="t-secondary">December 29, 2022 -</span> <!---->Wing flutter testing on June 14, 1995.<!----></div></div></div></div>
<div class="snippet" data-type="web"><a href="/search?q=more">More</a></div>"#;
    let results = parse_brave(html, 8);
    assert_eq!(results.len(), 1, "{results:?}");
    assert_eq!(results[0].0, "B-2 Wing Flutter & Video");
    assert_eq!(results[0].1, "https://theaviationist.com/b-2-flutter/");
    assert!(
        results[0]
            .2
            .contains("Wing flutter testing on June 14, 1995."),
        "{}",
        results[0].2
    );
}

#[test]
fn papers_are_parsed_from_openalex_and_crossref() {
    let openalex = serde_json::json!({"results": [{
        "doi": "https://doi.org/10.2514/2.2674",
        "display_name": "Aeroservoelastic Analysis of the B-2 Bomber",
        "publication_year": 2000,
        "authorships": [{"author": {"display_name": "R. Britt"}}, {"author": {"display_name": "S. Jacobson"}},
            {"author": {"display_name": "T. Arthurs"}}, {"author": {"display_name": "D. Fourth"}}],
        "primary_location": {"source": {"display_name": "Journal of Aircraft"}},
        "open_access": {"oa_url": null},
        "cited_by_count": 42
    }]});
    let papers = parse_openalex(&openalex);
    assert_eq!(papers.len(), 1);
    assert_eq!(papers[0].doi, "10.2514/2.2674");
    let text = render_papers(&papers);
    assert!(
        text.starts_with("1. Aeroservoelastic Analysis of the B-2 Bomber (2000)\n"),
        "{text}"
    );
    assert!(
        text.contains(
            "R. Britt, S. Jacobson, T. Arthurs et al. · Journal of Aircraft · cited by 42"
        ),
        "{text}"
    );
    assert!(
        text.contains("DOI: https://doi.org/10.2514/2.2674") && !text.contains("Open access"),
        "{text}"
    );

    let crossref = serde_json::json!({"message": {"items": [{
        "DOI": "10.1000/x", "title": ["Flying wings"], "author": [{"given": "Ann", "family": "Lee"}],
        "issued": {"date-parts": [[1995, 3]]}, "container-title": ["AIAA"],
        "link": [{"URL": "https://example.org/x.pdf", "content-type": "application/pdf"}]
    }]}});
    let papers = parse_crossref(&crossref);
    assert_eq!(papers[0].authors, vec!["Ann Lee".to_owned()]);
    assert_eq!(papers[0].year, Some(1995));
    assert_eq!(papers[0].pdf, "https://example.org/x.pdf");
}
