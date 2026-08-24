//! The canned speaker: deterministic reply chunks for the keyless path
//! (`ka run` with no model configured). Also the Phase-0 exit-criterion
//! fixture — real wires live in ka-dialect.

/// Build the canned reply for a prompt, as streamed text chunks.
pub fn reply(prompt: &str) -> Vec<String> {
    vec![
        "(ka, no model configured) ".to_string(),
        format!("heard: {prompt} — "),
        "set a model with --model or KA_MODEL to speak for real.".to_string(),
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
