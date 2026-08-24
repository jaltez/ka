//! Minimal hand-rolled SSE parser. Handles `data:` lines, ignores `event:`
//! lines (both wires carry full payloads in data), comments, and CRLF.
//! No allocations beyond the collected data strings.

/// One parsed SSE frame (its `data:` payload).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// The data payload (possibly multi-line, joined with `\n`).
    pub data: String,
}

/// Incremental SSE parser: feed raw bytes, receive complete events.
#[derive(Debug, Default)]
pub struct SseParser {
    buf: String,
}

impl SseParser {
    /// New empty parser.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes; returns any complete events decoded from them.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.buf.push_str(&String::from_utf8_lossy(bytes));
        self.drain()
    }

    /// Flush at end-of-stream: a trailing frame without final separator is
    /// still an event if it contains a data line.
    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut out = self.drain();
        if let Some(evt) = parse_frame(&self.buf) {
            out.push(evt);
        }
        self.buf.clear();
        out
    }

    fn drain(&mut self) -> Vec<SseEvent> {
        let mut out = Vec::new();
        // Split on blank line separators; both "\n\n" and "\r\n\r\n".
        loop {
            let Some(idx) = find_separator(&self.buf) else {
                break;
            };
            let frame: String = self.buf.drain(..idx).collect();
            // after draining the frame, the separator sits at the start
            let sep_len = separator_len(&self.buf);
            self.buf.drain(..sep_len);
            if let Some(evt) = parse_frame(&frame) {
                out.push(evt);
            }
        }
        out
    }
}

fn find_separator(s: &str) -> Option<usize> {
    let rn = s.find("\r\n\r\n").unwrap_or(usize::MAX);
    let nn = s.find("\n\n").unwrap_or(usize::MAX);
    if rn == usize::MAX && nn == usize::MAX {
        None
    } else {
        Some(rn.min(nn))
    }
}

fn separator_len(at: &str) -> usize {
    if at.starts_with("\r\n\r\n") { 4 } else { 2 }
}

fn parse_frame(frame: &str) -> Option<SseEvent> {
    let mut data_lines: Vec<&str> = Vec::new();
    for line in frame.split(['\n', '\r']) {
        let line = line.trim_end_matches('\r');
        if line.starts_with(':') || line.is_empty() {
            continue; // comment / blank
        }
        if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
        // event:/id:/retry: ignored — payloads are self-describing JSON
    }
    if data_lines.is_empty() {
        None
    } else {
        Some(SseEvent {
            data: data_lines.join("\n"),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn parses_basic_frames() {
        let mut p = SseParser::new();
        let evts = p.feed(b"data: {\"a\":1}\n\ndata: {\"b\":2}\n\n");
        assert_eq!(evts.len(), 2);
        assert_eq!(evts[0].data, "{\"a\":1}");
        assert_eq!(evts[1].data, "{\"b\":2}");
    }

    #[test]
    fn handles_crlf_and_split_chunks() {
        let mut p = SseParser::new();
        assert!(p.feed(b"data: {\"pa").is_empty());
        assert!(p.feed(b"rt\":1}\r\n\r").is_empty());
        let evts = p.feed(b"\ndata: done\n\n");
        assert_eq!(evts.len(), 2);
        assert_eq!(evts[0].data, "{\"part\":1}");
        assert_eq!(evts[1].data, "done");
    }

    #[test]
    fn ignores_event_lines_and_comments() {
        let mut p = SseParser::new();
        let evts = p.feed(b": keepalive\nevent: message_delta\ndata: {}\n\n");
        assert_eq!(evts.len(), 1);
        assert_eq!(evts[0].data, "{}");
    }

    #[test]
    fn finish_emits_trailing_frame() {
        let mut p = SseParser::new();
        assert!(p.feed(b"data: tail").is_empty());
        let evts = p.finish();
        assert_eq!(evts.len(), 1);
        assert_eq!(evts[0].data, "tail");
    }

    #[test]
    fn finish_drops_partial_non_data() {
        let mut p = SseParser::new();
        p.feed(b"event: x");
        assert!(p.finish().is_empty());
    }

    #[test]
    fn multiline_data_joined() {
        let mut p = SseParser::new();
        let evts = p.feed(b"data: line1\ndata: line2\n\n");
        assert_eq!(evts.len(), 1);
        assert_eq!(evts[0].data, "line1\nline2");
    }
}
