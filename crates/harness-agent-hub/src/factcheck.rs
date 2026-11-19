//! Fact check for deep research reports (docs/specs/harness/deep-research.md, section "Fact check"): the
//! instructions a research turn sends after `research.rs` and `research_figures.rs`.

pub(crate) const INSTRUCTIONS: &str = r#"<deep-research-factcheck>
After the report is written, check its factual claims against sources before you reply. Skip this step only when the message says fact checking is off.

1. Extract. Turn the report body into atomic claims: one assertion each, self-contained (replace pronouns with the noun they stand for), numbers, dates and names copied verbatim, each claim kept together with its section and sentence index. Leave out the abstract's restatement, the method sentences (what you searched, what tools you used), definitions and opinions. When the fact_check tool is available, call it with action "extract", the question and the report text; it returns the same claim list, already filtered and capped.
2. Filter. Drop claims that do not bear on the research question. Never drop a claim only because it is hard to check: mark it unclear instead.
3. Verify each claim. Search for it with web_search, and with paper_search when it is scientific, using at most three queries per claim: the claim's wording, then a shorter keyword-only variant. Read the snippets first and fetch only the pages that look decisive. A claim is supported only when a source you actually opened states it; a snippet or a summary that repeats the wording is not enough. Record the verdict supported, notSupported or unclear, the source URLs with their titles, and a one-line reason.
4. Keep the cost bounded. At most 40 claims per report (say how many were left unchecked), at most three queries per claim, no repeated identical queries, at most 120 searches for the whole step, and stop searching once a claim has its verdict. When the fact_check tool is available, call it with action "record" for the verdicts, which deduplicates the queries and scores the set.
5. Write the result. With the fact_check tool, call it with action "write", the same question and dir research/<slug>; it reuses every verdict recorded earlier (record can be called in batches), so do not send them again, and it writes research/<slug>/factcheck.json and appends a Fact-check appendix table to report.tex once. Without the tool, write the same JSON file yourself (question, report, checkedAt, summary with total, supported, notSupported, unclear and unchecked, and one entry per claim with text, location, verdict, reason, sources and queries) and append a booktabs table (Claim, Verdict, Source) before the closing document command, escaping LaTeX special characters.
6. Revise once. For every notSupported claim, go back to the writer's section: re-source it from a primary source, correct it to what the sources say, or remove it. If it still has no source after that pass, keep it but leave it flagged; never raise the score without a new source.
7. Reply. State the score (for example 41 of 45 claims supported, 3 not supported, 1 unclear), list the flagged claims with their sources, and name the path of factcheck.json.
</deep-research-factcheck>"#;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    #[test]
    fn block_names_the_output_and_the_verdicts() {
        let text = super::INSTRUCTIONS;
        assert!(
            text.starts_with("<deep-research-factcheck>")
                && text.ends_with("</deep-research-factcheck>")
        );
        for needle in [
            "factcheck.json",
            "supported",
            "notSupported",
            "unclear",
            "40 claims",
            "report.tex",
        ] {
            assert!(text.contains(needle), "missing {needle}");
        }
    }
}
