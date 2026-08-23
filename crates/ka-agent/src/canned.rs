//! Phase-0 canned speaker. Produces the deterministic reply chunks the exit
//! criterion streams through the real queues. Phase 1 replaces this module
//! with the ka-dialect wires (anthropic-messages, openai-chat).

/// Build the canned reply for a prompt, as streamed text chunks.
pub fn reply(prompt: &str) -> Vec<String> {
    vec![
        "(ka phase 0) ".to_string(),
        format!("heard: {prompt} — "),
        "canned reply; real wires land in phase 1.".to_string(),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    #[test]
    fn reply_has_three_chunks_containing_prompt() {
        let chunks = super::reply("hi");
        assert_eq!(chunks.len(), 3);
        assert!(chunks[1].contains("hi"));
    }
}
