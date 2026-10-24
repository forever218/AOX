//! Why a model request failed, and whether trying again is worth it.
//!
//! Before this module every request failure was a formatted string ([`anyhow::bail!`]), so the loop
//! could not tell `429 Too Many Requests` from `401 invalid api key` — both arrived as "some error"
//! and both were handled the same way (fail the turn, or retry blindly). The two need opposite
//! treatment: one is a queue with a clock on it, the other will fail identically every time.
//!
//! So the transport layer now attaches a **cause** to the error it returns:
//!
//! - [`HttpFailure`] — a non-2xx response, carrying the status and the `Retry-After` the server sent;
//! - [`StreamCut`] — 200 OK, but the stream stopped before the model finished. This used to be
//!   silently accepted as a half-sentence reply (see `openai::chat_stream`);
//! - [`Stalled`] — accepted, then no bytes at all for [`STREAM_IDLE_SECS`];
//! - a bare [`reqwest::Error`] — connection refused, reset, read timeout.
//!
//! The public surface is deliberately three functions ([`retryable`], [`retry_after`], [`delay`]) so
//! the *policy* lives in one file and the loop stays a loop.

use std::time::Duration;

use crate::config::RetryConfig;

/// A non-2xx response from the endpoint.
#[derive(Debug)]
pub struct HttpFailure {
    pub status: u16,
    /// `Retry-After`, already parsed. `None` when the header was absent or written as an HTTP-date
    /// (which these endpoints do not send; see [`parse_retry_after`]).
    pub retry_after: Option<Duration>,
    /// The readable half of the body — `error.message` when the endpoint sent one.
    pub detail: String,
}

impl std::fmt::Display for HttpFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Same wording as the `bail!` this replaced, so nothing downstream has to change.
        write!(f, "{}", crate::tf!("LLM request failed HTTP {}: {}", self.status, self.detail))
    }
}

impl std::error::Error for HttpFailure {}

/// The response stopped before the model finished — no `finish_reason` chunk and no `data: [DONE]`.
///
/// This is not an edge case: it is what a dropped connection, a proxy cutting a long body, or a
/// server that gives up halfway looks like. Treating it as success wrote a half-sentence into the
/// conversation file and ended the turn as if the model had answered.
#[derive(Debug)]
pub struct StreamCut {
    /// What had arrived by then, for the error message ("312 chars, 41 chunks").
    pub detail: String,
}

impl std::fmt::Display for StreamCut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", crate::tf!("the model response stopped mid-stream ({})", self.detail))
    }
}

impl std::error::Error for StreamCut {}

/// The endpoint took the request and then sent nothing for [`STREAM_IDLE_SECS`].
#[derive(Debug)]
pub struct Stalled {
    pub idle_secs: u64,
}

impl std::fmt::Display for Stalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", crate::tf!("the model sent nothing for {} s; the stream was abandoned", self.idle_secs))
    }
}

impl std::error::Error for Stalled {}

/// How long a stream may stay **silent** before it counts as dead.
///
/// A **stall** watchdog, not a total timeout: a thinking model writing 32k tokens legitimately takes
/// minutes, and the 180 s total timeout this replaces would kill it mid-answer (and then, with
/// retries on, pay for it twice). Silence is the only thing that means "this is not coming".
pub const STREAM_IDLE_SECS: u64 = 120;

/// Cap on the whole **non-streaming** request. Same number the client used to apply to everything.
pub const BLOCKING_TOTAL_SECS: u64 = 180;

/// Connect deadline, streaming or not: a TCP handshake that takes longer than this is a failure, not
/// slowness.
pub const CONNECT_SECS: u64 = 15;

/// Is this status worth another attempt?
///
/// The split is "would the identical request plausibly succeed a second later?" — rate limits,
/// upstream overload and gateway hiccups yes; a bad key, a bad model name, a context that is too
/// long, or a malformed tool schema no. Retrying the second group only delays the error the user has
/// to see anyway (and, for `429`-style billing endpoints, costs money).
pub fn retryable_status(status: u16) -> bool {
    matches!(
        status,
        // Request timeout / too early / rate limited
        408 | 425 | 429
        // Server-side: these are "try again" by definition
        | 500 | 502 | 503 | 504
        // Non-standard but widely used for "overloaded" (Anthropic, some gateways)
        | 529
    )
}

/// Should the loop send this error's request again?
pub fn retryable(e: &anyhow::Error) -> bool {
    if let Some(h) = e.downcast_ref::<HttpFailure>() {
        return retryable_status(h.status);
    }
    // A cut or stalled stream is by definition transient: the next attempt starts from a new
    // connection. A `StreamCut` caused by *the server refusing to stream* would burn one extra
    // attempt and then fail with the same message, which is the right price.
    if e.downcast_ref::<StreamCut>().is_some() || e.downcast_ref::<Stalled>().is_some() {
        return true;
    }
    // Transport: refused / reset / TLS / read timeout. `reqwest::Error` is the only source of these
    // (everything we generate ourselves is one of the types above or a parse error, which is not
    // retryable: the same bytes would parse the same way).
    e.downcast_ref::<reqwest::Error>().is_some()
}

/// How long the server asked us to wait, if it said so.
pub fn retry_after(e: &anyhow::Error) -> Option<Duration> {
    e.downcast_ref::<HttpFailure>().and_then(|h| h.retry_after)
}

/// The wait before attempt `n` (1-based): `base * 2^(n-1)`, never shorter than the server's hint,
/// never longer than `max_ms`.
///
/// The cap applies to the hint **as well**: a `Retry-After: 3600` is a real answer from the server,
/// but honouring it means the turn sits silent for an hour. Better to give up after a bounded wait
/// and say so than to hang — see the caller, which reports the failure with the delay it used.
pub fn delay(attempt: usize, cfg: &RetryConfig, hint: Option<Duration>) -> Duration {
    let shift = attempt.saturating_sub(1).min(16) as u32;
    let backoff = cfg.base_ms.saturating_mul(1u64 << shift);
    let hinted = hint.map(|d| d.as_millis() as u64).unwrap_or(0);
    Duration::from_millis(backoff.max(hinted).min(cfg.max_ms))
}

/// `Retry-After` → duration. Only the `delta-seconds` form is honoured.
///
/// The header may also be an HTTP-date, which would need a date parser (and the machine's own
/// clock, which can be wrong) to turn into a delay. None of the OpenAI-compatible endpoints AOX
/// talks to use it, so an unparseable value means "no hint" rather than "guess".
pub fn parse_retry_after(v: &str) -> Option<Duration> {
    v.trim().parse::<u64>().ok().map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    fn cfg() -> RetryConfig {
        RetryConfig { attempts: 2, base_ms: 1000, max_ms: 30_000 }
    }

    fn http(status: u16, hint: Option<u64>) -> anyhow::Error {
        anyhow::Error::new(HttpFailure {
            status,
            retry_after: hint.map(Duration::from_secs),
            detail: "detail".into(),
        })
    }

    #[test]
    fn a_rate_limit_is_retryable_and_a_bad_key_is_not() {
        // The whole point of carrying structure: these two used to be the same string.
        assert!(retryable(&http(429, None)));
        assert!(retryable(&http(503, None)));
        assert!(!retryable(&http(401, None)), "a bad key fails identically every time");
        assert!(!retryable(&http(400, None)));
        assert!(!retryable(&http(413, None)), "the context is too long; re-sending makes it longer");
        assert!(!retryable(&anyhow!("LLM returned invalid JSON: x")), "same bytes parse the same way");
    }

    #[test]
    fn a_cut_or_stalled_stream_is_retryable() {
        let cut = anyhow::Error::new(StreamCut { detail: "312 chars".into() });
        assert!(retryable(&cut));
        let stalled = anyhow::Error::new(Stalled { idle_secs: STREAM_IDLE_SECS });
        assert!(retryable(&stalled));
    }

    #[test]
    fn backoff_doubles_and_stops_at_the_ceiling() {
        let c = cfg();
        assert_eq!(delay(1, &c, None), Duration::from_secs(1));
        assert_eq!(delay(2, &c, None), Duration::from_secs(2));
        assert_eq!(delay(3, &c, None), Duration::from_secs(4));
        assert_eq!(delay(99, &c, None), Duration::from_secs(30), "the doubling is capped, not the attempt count");
    }

    #[test]
    fn the_servers_own_hint_wins_but_is_still_capped() {
        let c = cfg();
        // A hint longer than our backoff is honoured …
        assert_eq!(delay(1, &c, Some(Duration::from_secs(5))), Duration::from_secs(5));
        // … but never turns into an hour of silence.
        assert_eq!(delay(1, &c, Some(Duration::from_secs(3600))), Duration::from_secs(30));
        // A shorter hint does not shorten our own backoff.
        assert_eq!(delay(3, &c, Some(Duration::from_secs(1))), Duration::from_secs(4));
    }

    #[test]
    fn retry_after_is_seconds_or_nothing() {
        assert_eq!(parse_retry_after("5"), Some(Duration::from_secs(5)));
        assert_eq!(parse_retry_after(" 0 "), Some(Duration::ZERO));
        assert_eq!(parse_retry_after("Wed, 21 Oct 2026 07:28:00 GMT"), None, "the date form is not guessed at");
        assert_eq!(parse_retry_after(""), None);
    }

    #[test]
    fn the_hint_travels_on_the_error() {
        let e = http(429, Some(7));
        assert_eq!(retry_after(&e), Some(Duration::from_secs(7)));
        assert_eq!(retry_after(&http(429, None)), None);
        assert_eq!(retry_after(&anyhow!("no cause")), None);
    }

    #[test]
    fn the_message_is_the_one_the_bail_used_to_produce() {
        // Nothing downstream (UI, log, conversation file) has to change wording.
        assert_eq!(http(429, None).to_string(), "LLM request failed HTTP 429: detail");
    }
}
