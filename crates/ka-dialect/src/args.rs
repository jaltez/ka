//! Streaming tool-argument accumulator: collects JSON fragments across
//! deltas and parses lossily at the end — raw parse first, then bounded
//! repair (drop trailing garbage after the last structurally complete
//! top-level value, close unbalanced containers, strip trailing commas).

use serde_json::Value;

/// Accumulator for one tool call's streamed arguments.
#[derive(Debug, Default)]
pub struct ArgsAccumulator {
    buf: String,
}

impl ArgsAccumulator {
    /// New accumulator.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one raw fragment.
    pub fn push(&mut self, chunk: &str) {
        self.buf.push_str(chunk);
    }

    /// The raw accumulated text.
    pub fn raw(&self) -> &str {
        &self.buf
    }

    /// Parse the accumulated arguments, repairing if needed.
    pub fn finish(&self) -> Value {
        parse_lossy(&self.buf)
    }
}

/// Parse a JSON object lossily: raw first, then repairs.
pub fn parse_lossy(raw: &str) -> Value {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Value::Object(Default::default());
    }
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        return v;
    }
    // Repair 1: cut after the last position where the JSON is structurally
    // complete (tracked by scanning outside strings), then close containers.
    if let Some(cut) = last_complete_pos(trimmed) {
        let mut candidate: String = trimmed[..cut].to_string();
        close_open(&mut candidate, &trimmed[..cut]);
        if let Ok(v) = serde_json::from_str::<Value>(&candidate) {
            return v;
        }
        // Repair 2: additionally strip a trailing comma before closing.
        let commaless = strip_trailing_comma(&candidate);
        if let Ok(v) = serde_json::from_str::<Value>(&commaless) {
            return v;
        }
    }
    // Repair 3: whole-input brace close + comma strip.
    let mut candidate = trimmed.to_string();
    close_open(&mut candidate, trimmed);
    let candidate = strip_trailing_comma(&candidate);
    if let Ok(v) = serde_json::from_str::<Value>(&candidate) {
        return v;
    }
    Value::Object(Default::default())
}

/// Scan with string/escape awareness; return the byte offset just after the
/// last top-level value that ended cleanly (i.e., the first position where
/// depth returns to zero after having been > 0), or at the last position
/// where depth is zero and a comma separated a complete value.
fn last_complete_pos(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth: i64 = 0;
    let mut in_string = false;
    let mut escaped = false;
    let mut last_zero_after_value: Option<usize> = None;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
                if depth == 0 {
                    last_zero_after_value = Some(i + 1);
                }
            }
        } else {
            match b {
                b'"' => in_string = true,
                b'{' | b'[' => depth += 1,
                b'}' | b']' => {
                    depth -= 1;
                    if depth == 0 {
                        last_zero_after_value = Some(i + 1);
                    }
                    if depth < 0 {
                        return last_zero_after_value;
                    }
                }
                b',' | b' ' | b'\t' | b'\n' | b'\r' => {}
                _ => {
                    // bare scalar at top level (number/true/false/null):
                    // complete when the next delimiter appears
                    if depth == 0 {
                        let mut j = i;
                        while j < bytes.len()
                            && !matches!(
                                bytes[j],
                                b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r'
                            )
                        {
                            j += 1;
                        }
                        last_zero_after_value = Some(j);
                        i = j;
                        continue;
                    }
                }
            }
        }
        i += 1;
    }
    last_zero_after_value
}

/// Append the closers needed for unbalanced containers in `prefix`, using
/// `scan_target` (same text) to know which closers and in what order.
fn close_open(candidate: &mut String, scan_target: &str) {
    let bytes = scan_target.as_bytes();
    let mut depth_stack: Vec<u8> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    for &b in bytes {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
        } else {
            match b {
                b'"' => in_string = true,
                b'{' => depth_stack.push(b'}'),
                b'[' => depth_stack.push(b']'),
                b'}' | b']' => {
                    depth_stack.pop();
                }
                _ => {}
            }
        }
    }
    // an unterminated string can only be repaired by closing it
    if in_string {
        candidate.push('"');
    }
    while let Some(closer) = depth_stack.pop() {
        candidate.push(closer as char);
    }
}

/// Remove a trailing comma that sits before the end (before closers).
fn strip_trailing_comma(s: &str) -> String {
    let t = s.trim_end();
    let trimmed = t.trim_end_matches(['}', ']']);
    match trimmed.trim_end().strip_suffix(',') {
        Some(head) => format!("{}{}", head, &t[trimmed.trim_end().len()..]),
        None => t.to_string(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn clean_parse_passes_through() {
        let mut acc = ArgsAccumulator::new();
        acc.push(r#"{"path":"a.rs","li"#);
        acc.push(r#"nes":[1,2,3]}"#);
        let v = acc.finish();
        assert_eq!(v["path"], "a.rs");
        assert_eq!(v["lines"][2], 3);
    }

    #[test]
    fn truncated_object_gets_completed() {
        // stream cut mid-value: {"a":"b","c":["x","y"  (missing closers)
        let v = parse_lossy(r#"{"a":"b","c":["x","y""#);
        assert_eq!(v["a"], "b");
        assert_eq!(v["c"][1], "y");
    }

    #[test]
    fn trailing_comma_repaired() {
        let v = parse_lossy(r#"{"a":1,}"#);
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn truncated_string_value_closed() {
        let v = parse_lossy(r#"{"note":"half of a senten"#);
        assert_eq!(v["note"], "half of a senten");
    }

    #[test]
    fn garbage_yields_empty_object_not_panic() {
        assert_eq!(parse_lossy("~~~"), Value::Object(Default::default()));
        assert_eq!(parse_lossy(""), Value::Object(Default::default()));
    }

    #[test]
    fn nested_truncation_after_complete_member() {
        // {"a":{"b":2},"c":[1,2   → keep {"a":{"b":2},"c":[1,2]}
        let v = parse_lossy(r#"{"a":{"b":2},"c":[1,2"#);
        assert_eq!(v["a"]["b"], 2);
        assert_eq!(v["c"][1], 2);
    }

    #[test]
    fn escaped_quotes_do_not_confuse_depth() {
        let v = parse_lossy(r#"{"s":"he said \"hi\" }","n":1"#);
        assert_eq!(v["n"], 1);
        assert!(v["s"].is_string());
    }
}
