//! Retry policy shared by both wires: classify failures, honor
//! `Retry-After`, back off with jitter, never retry overflow/auth.

use std::time::Duration;

use crate::speaker::SpeakerError;

/// Maximum in-wire attempts (1 initial + retries).
pub const MAX_ATTEMPTS: u32 = 3;

/// What to do with a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Retry after this delay (attempt number included).
    Retry { after: Duration, attempt: u32 },
    /// Give up; the error is terminal.
    Stop,
}

/// Stateless policy evaluator.
#[derive(Debug, Clone, Copy, Default)]
pub struct RetryPolicy {
    /// Base backoff (doubled per attempt).
    pub base: Duration,
}

impl RetryPolicy {
    /// Policy with ka defaults (500ms base, ±25% jitter, 8s cap).
    pub fn new() -> Self {
        Self {
            base: Duration::from_millis(500),
        }
    }

    /// Decide what to do with `error` after `attempt` failed attempts
    /// (1-based). `retry_after` overrides backoff when present.
    pub fn decide(
        &self,
        error: &SpeakerError,
        attempt: u32,
        retry_after: Option<Duration>,
    ) -> Decision {
        if !error.retryable() || attempt >= MAX_ATTEMPTS {
            return Decision::Stop;
        }
        let exp = self.base.saturating_mul(1 << (attempt - 1).min(4));
        let capped = exp.min(Duration::from_secs(8));
        let delay = match retry_after {
            Some(ra) => ra.min(Duration::from_secs(60)),
            None => jitter(capped),
        };
        Decision::Retry {
            after: delay,
            attempt,
        }
    }
}

/// ±25% jitter around `d`, seeded from the clock (no rand crate).
fn jitter(d: Duration) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|t| t.subsec_nanos() as u64 ^ t.as_secs())
        .unwrap_or(0);
    // xorshift-ish mixing, then scale into [-0.25, +0.25]
    let mut x = nanos.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    let unit = x % 1000; // 0..1000
    let factor = 0.75 + (unit as f64 / 2000.0); // 0.75..1.25
    Duration::from_secs_f64(d.as_secs_f64() * factor)
}

/// Parse a `Retry-After` header carrying seconds (HTTP-date forms are
/// ignored; backoff applies instead).
pub fn parse_retry_after(value: Option<&str>) -> Option<Duration> {
    let v = value?.trim();
    let secs: u64 = v.parse().ok()?;
    Some(Duration::from_secs(secs))
}

/// Classify an HTTP status + body snippet into a SpeakerError.
pub fn classify_status(status: u16, body: &str) -> SpeakerError {
    match status {
        401 | 403 => SpeakerError::Auth(format!("status {status}")),
        429 => SpeakerError::RateLimit {
            detail: first_line(body),
        },
        400 | 404 | 422 => {
            if is_overflow(body) {
                SpeakerError::Overflow {
                    detail: first_line(body),
                }
            } else {
                SpeakerError::BadRequest(format!("status {status}: {}", first_line(body)))
            }
        }
        408 | 500 | 502 | 503 | 504 => SpeakerError::Network(format!("status {status}")),
        other => SpeakerError::BadRequest(format!("status {other}")),
    }
}

/// Detect context-window-exceeded conditions across providers.
pub fn is_overflow(body: &str) -> bool {
    let b = body.to_ascii_lowercase();
    b.contains("context_length_exceeded")
        || b.contains("prompt is too long")
        || b.contains("context window")
            && (b.contains("exceed") || b.contains("too large") || b.contains("too long"))
        || b.contains("maximum context length")
}

fn first_line(body: &str) -> String {
    let line = body.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let mut out = String::new();
    for c in line.chars().take(200) {
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn overflow_and_auth_never_retry() {
        let p = RetryPolicy::new();
        let e = SpeakerError::Overflow { detail: "x".into() };
        assert_eq!(p.decide(&e, 1, None), Decision::Stop);
        assert_eq!(
            p.decide(&SpeakerError::Auth("x".into()), 1, None),
            Decision::Stop
        );
    }

    #[test]
    fn rate_limit_retries_up_to_max() {
        let p = RetryPolicy::new();
        let e = SpeakerError::RateLimit {
            detail: "429".into(),
        };
        assert!(matches!(p.decide(&e, 1, None), Decision::Retry { .. }));
        assert!(matches!(p.decide(&e, 2, None), Decision::Retry { .. }));
        assert_eq!(p.decide(&e, 3, None), Decision::Stop);
    }

    #[test]
    fn retry_after_overrides_backoff_capped() {
        let p = RetryPolicy::new();
        let e = SpeakerError::RateLimit { detail: "x".into() };
        match p.decide(&e, 1, Some(Duration::from_secs(300))) {
            Decision::Retry { after, .. } => assert_eq!(after, Duration::from_secs(60)),
            other => panic!("expected retry, got {other:?}"),
        }
        match p.decide(&e, 1, Some(Duration::from_millis(1200))) {
            Decision::Retry { after, .. } => assert_eq!(after, Duration::from_millis(1200)),
            other => panic!("expected retry, got {other:?}"),
        }
    }

    #[test]
    fn jitter_stays_in_band() {
        let d = Duration::from_millis(500);
        for _ in 0..50 {
            let j = jitter(d).as_millis();
            assert!((350..=700).contains(&j), "out of band: {j}");
        }
    }

    #[test]
    fn retry_after_parsing() {
        assert_eq!(parse_retry_after(Some("7")), Some(Duration::from_secs(7)));
        assert_eq!(
            parse_retry_after(Some(" 12 ")),
            Some(Duration::from_secs(12))
        );
        assert_eq!(parse_retry_after(Some("Tue, 4 Sep 2026")), None);
        assert_eq!(parse_retry_after(None), None);
    }

    #[test]
    fn status_classification() {
        assert!(matches!(
            classify_status(429, "slow down"),
            SpeakerError::RateLimit { .. }
        ));
        assert!(matches!(
            classify_status(400, "prompt is too long: 210000 tokens > 200000 maximum"),
            SpeakerError::Overflow { .. }
        ));
        assert!(matches!(
            classify_status(400, "invalid model id"),
            SpeakerError::BadRequest(_)
        ));
        assert!(matches!(
            classify_status(503, "unavailable"),
            SpeakerError::Network(_)
        ));
        assert!(matches!(classify_status(401, ""), SpeakerError::Auth(_)));
        // openai-style body also detected
        assert!(is_overflow(
            r#"{"error":{"code":"context_length_exceeded","message":"too many tokens"}}"#
        ));
    }
}
