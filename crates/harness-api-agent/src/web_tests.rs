#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

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
    assert_eq!(results[0].2, "An ergonomic & batteries-included HTTP client.");
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
    assert!(!text.contains("var x") && !text.contains("hidden") && !text.contains("p{}"), "{text}");
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
