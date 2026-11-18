//! Deep research mode (docs/specs/harness/deep-research.md): the instructions a turn carrying
//! `ContextItem::Research` sends to whichever agent runs it.

pub(crate) const LABEL: &str = "Deep research";

pub(crate) const INSTRUCTIONS: &str = r"<deep-research>
Deep research mode is on for this message. Treat the user's message as a research question and produce a sourced report, not a quick answer from memory.

1. Plan. Restate the question in one line, then list 3 to 7 sub-questions that together answer it, and say what evidence would change the conclusion.
2. Search. Answer each sub-question from current sources on the web with your web search and page fetch tools (the Harness API agent has web_search and web_fetch). Search several times with different wording, open and read the most relevant pages instead of relying on snippets, prefer primary sources (official documentation, papers, standards, original data, filings, release notes) over summaries, and note each source's publication date.
3. Parallelise broad questions. When the harness-delegate tools are available, hand independent sub-questions to subagents (list_agents, then delegate), ask each for findings with source URLs, then read their results and check the key claims yourself.
4. Verify. Confirm every important claim with at least two independent sources where possible. Report disagreements between sources instead of silently picking one. Never invent a source, quote, number or URL; say plainly when something could not be confirmed.
5. Keep the context small. Once you have taken notes from a long page, keep only the notes with their URLs (use context_compress when you have it).
6. Report. Write the report as Markdown to research/<short-topic-slug>.md in the working directory, creating the folder; if you cannot write files, put the whole report in your reply. Sections: title and date; Summary (3 to 6 sentences that answer the question); Findings per sub-question with numbered citations like [1]; Disagreements and open questions; Confidence (high, medium or low, with the reason); Sources (numbered: title, URL, publication date).
7. Reply. Give the summary, the most important findings with their citations, and the report path.

If none of your tools can reach the web, say so first and stop. Answer from memory only if the user then asks for it, and label that answer as unverified.
Write the report and the reply in the language of the user's message.
</deep-research>";

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use harness_core_api::ContextItem;

    #[test]
    fn research_item_wire_format_matches_the_phone_client() {
        let value = serde_json::to_value(ContextItem::Research).unwrap();
        assert_eq!(value, serde_json::json!({"type": "research"}));
        let back: ContextItem = serde_json::from_str(r#"{"type":"research"}"#).unwrap();
        assert_eq!(back, ContextItem::Research);
        assert!(
            super::INSTRUCTIONS.contains("web_search") && super::INSTRUCTIONS.contains("research/")
        );
    }
}
