//! Which model failures are worth retrying, and how long to wait between
//! attempts. Ported from OpenCode's `classifyProviderFailure`
//! (`packages/ai/src/provider-error.ts`), `ModelRetry.policy`
//! (`packages/core/src/model-retry.ts`) and the retry half of
//! `toSessionError`; the tests at the bottom port
//! `session-runner-retry.test.ts` and `session-error.test.ts`.
//!
//! Deliberate differences:
//! - `codesmith_ai::Error` keeps the status and body but not the response
//!   headers, so `retry-after` and `x-should-retry` are not honoured; the
//!   schedule's own delays apply.
//! - Transport failures arrive as reqwest's message text, so timeouts and
//!   unreachable hosts are recognised by wording rather than by error code.
//! - Error frames inside a stream carry no status; they are classified by
//!   their codes and wording like OpenCode's status-less failures, and a frame
//!   that is not JSON at all is invalid provider output (not retried).

use codesmith_ai::Error;
use rand::Rng;
use std::time::{Duration, Instant};

/// What kind of failure a provider error is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// The prompt does not fit the model's context window.
    ContextOverflow,
    /// The request body is too large to send.
    PayloadTooLarge,
    /// The provider refused the content.
    ContentPolicy,
    /// The account is out of credit or over a usage cap (not a throttle).
    Quota,
    Authentication,
    RateLimit,
    /// A server-side failure: 5xx, 408, 409, overloaded.
    ProviderInternal,
    /// A deterministic rejection of this request (any other 4xx).
    InvalidRequest,
    /// The stream could not be parsed.
    InvalidOutput,
    /// The connection failed or dropped.
    Transport,
    /// Nothing recognisable: retried, as transient failures come in odd shapes.
    Unknown,
}

impl Class {
    /// Whether the same request may succeed if sent again.
    pub fn retryable(self) -> bool {
        matches!(
            self,
            Class::RateLimit | Class::ProviderInternal | Class::Transport | Class::Unknown
        )
    }
}

const QUOTA_CODES: &[&str] = &[
    "insufficient_quota",
    "usage_not_included",
    "billing_error",
    "gousagelimiterror",
    "budgetlimiterror",
    "freeusagelimiterror",
    "creditlimitexceeded",
];
const AUTH_CODES: &[&str] = &[
    "authentication_error",
    "permission_error",
    "permission_denied",
    "api_key_invalid",
];
const SERVER_CODES: &[&str] = &[
    "api_error",
    "internal_error",
    "internalserverexception",
    "modelstreamerrorexception",
    "overloaded_error",
    "server_error",
    "server_is_overloaded",
    "slow_down",
    "serviceunavailableexception",
];
const INVALID_REQUEST_CODES: &[&str] = &[
    "model_not_found",
    "invalid_prompt",
    "invalid_request",
    "invalid_request_error",
    "validationexception",
];
const CONTENT_POLICY_CODES: &[&str] = &[
    "content_filter",
    "content_moderation",
    "responsibleaipolicyviolation",
    "content_policy_violation",
    "image_content_policy_violation",
    "cyber_policy",
    "refusal",
    "datainspectionfailed",
    "moderation_blocked",
    "safety",
    "prohibited_content",
    "blocklist",
    "image_safety",
    "content_blocked",
];

fn re(pattern: &str) -> regex::Regex {
    regex::RegexBuilder::new(pattern)
        .case_insensitive(true)
        .build()
        .expect("valid pattern")
}

/// The `code` and `type` strings anywhere in a JSON error body.
fn codes(body: &str) -> Vec<String> {
    fn walk(v: &serde_json::Value, out: &mut Vec<String>) {
        match v {
            serde_json::Value::Object(m) => {
                for (k, v) in m {
                    if matches!(k.as_str(), "code" | "type" | "status" | "error_type") {
                        if let Some(s) = v.as_str() {
                            out.push(s.to_lowercase());
                        }
                    }
                    walk(v, out);
                }
            }
            serde_json::Value::Array(a) => a.iter().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        walk(&v, &mut out);
    }
    out
}

fn quota_text(text: &str) -> bool {
    let t = text.to_lowercase();
    if re(r"insufficient[-_\s]?(?:quota|balance)|budget exceeded|usage limit|package has expired|plan does not yet include").is_match(&t) {
        return true;
    }
    // Cerebras's per-minute `token_quota_exceeded` is a throttle, not a cap.
    let quota = re(r"quota[-_\s]?exceeded");
    if quota
        .find_iter(&t)
        .any(|m| !t[..m.start()].ends_with("token_"))
    {
        return true;
    }
    let exhausted = re(r"limit exhausted");
    let capped = exhausted.find_iter(&t).any(|m| {
        let before = t[..m.start()].trim_end_matches([' ', '-', '_']);
        !before.ends_with("rate")
    });
    capped
}

/// Classify a failure from its HTTP status (when there is one) and body text.
pub fn classify_failure(status: Option<u16>, body: &str) -> Class {
    let codes = codes(body);
    let has = |set: &[&str]| codes.iter().any(|c| set.contains(&c.as_str()));
    let client = status.is_none_or(|s| (400..500).contains(&s));
    let lower = body.to_lowercase();
    if client
        && (codes.iter().any(|c| {
            matches!(
                c.as_str(),
                "context_length_exceeded" | "model_context_window_exceeded" | "request_too_large"
            )
        }) || (status != Some(429)
            && !codes.iter().any(|c| c == "throttlingexception")
            && crate::compaction::is_overflow(body)))
    {
        return Class::ContextOverflow;
    }
    if status == Some(413)
        || lower.contains("payload too large")
        || lower.contains("request entity too large")
    {
        return Class::PayloadTooLarge;
    }
    if has(CONTENT_POLICY_CODES)
        || (client && re(r"violating our usage policy|blocked by content filtering policy|content[-_\s]?policy|rejected (?:as a result of our|by the) safety system|detected potentially unsafe or sensitive content|rejected in accordance with our terms of use").is_match(body))
    {
        return Class::ContentPolicy;
    }
    if status == Some(402) || has(QUOTA_CODES) || (status == Some(429) && quota_text(body)) {
        return Class::Quota;
    }
    if matches!(status, Some(401 | 403))
        || has(AUTH_CODES)
        || (status == Some(400) && lower.contains("incorrect api key provided"))
    {
        return Class::Authentication;
    }
    if status == Some(429)
        || codes.iter().any(|c| {
            c.contains("rate_limit") || c == "too_many_requests" || c == "throttlingexception"
        })
        || re(r"rate increased too quickly|rate[-_\s]?limit|too[_\s]?many[_\s]?requests")
            .is_match(body)
    {
        return Class::RateLimit;
    }
    let server_text = re(
        r"\b(?:try again|(?:please |you can )?retry (?:the |this |your )?request|try (?:the |this |your )?request again|(?:currently |temporarily )?at capacity|overloaded|temporarily unavailable|service[-_\s]?unavailable|(?:server|internal)[-_\s]?error|server (?:is )?busy|provider returned (?:an )?error|resource[-_\s]?exhausted|upstream (?:connect|connection|request)|request buffer limit while retrying upstream)\b",
    );
    if matches!(status, Some(408 | 409))
        || status.is_some_and(|s| s >= 500)
        || (status.is_none_or(|s| s < 400)
            && ((!has(INVALID_REQUEST_CODES) && server_text.is_match(body))
                || codes.iter().any(|c| {
                    SERVER_CODES.contains(&c.as_str())
                        || c.contains("exhausted")
                        || c.contains("unavailable")
                })))
    {
        return Class::ProviderInternal;
    }
    if has(INVALID_REQUEST_CODES) || status.is_some_and(|s| (400..500).contains(&s)) {
        return Class::InvalidRequest;
    }
    Class::Unknown
}

/// Classify a provider error.
pub fn classify(e: &Error) -> Class {
    match e {
        Error::Status { status, body } => classify_failure(Some(*status), body),
        Error::Transport(_) => Class::Transport,
        // Providers report a stream's error frame as its JSON; anything else
        // here is a frame that could not be parsed.
        Error::Decode(body) if body.trim_start().starts_with('{') => classify_failure(None, body),
        Error::Decode(_) => Class::InvalidOutput,
        // Rejected before sending (e.g. media the protocol cannot carry).
        Error::Invalid(_) => Class::InvalidRequest,
        // An error a provider reported inside its stream, already judged
        // retryable or not by the protocol; its text refines the class.
        Error::Provider { message, retryable } => match classify_failure(None, message) {
            // The text did not decide it: trust the protocol's judgement,
            // and never retry what it marked non-retryable.
            Class::Unknown if *retryable => Class::ProviderInternal,
            Class::Unknown => Class::InvalidRequest,
            c => c,
        },
    }
}

/// A transport failure that timed out (it already waited a long time).
pub fn is_timeout(e: &Error) -> bool {
    matches!(e, Error::Transport(m) if { let m = m.to_lowercase(); m.contains("timed out") || m.contains("timeout") })
}

/// A transport failure meaning this machine has no network path to the
/// provider's host (OpenCode's `ENOTFOUND`, `EAI_AGAIN`, `ENETUNREACH`,
/// `EHOSTUNREACH`, `ENETDOWN`). A refused connection is not one: something
/// answered.
pub fn is_unreachable(e: &Error) -> bool {
    let Error::Transport(m) = e else {
        return false;
    };
    let m = m.to_lowercase();
    [
        "dns error",
        "failed to lookup address",
        "name or service not known",
        "nodename nor servname",
        "temporary failure in name resolution",
        "network is unreachable",
        "no route to host",
        "host is unreachable",
        "network is down",
    ]
    .iter()
    .any(|p| m.contains(p))
}

/// Retries within one model step.
pub const MAX_RETRIES: u32 = 10;
/// Timed-out attempts already waited minutes, so they get fewer retries.
pub const MAX_TIMEOUT_RETRIES: u32 = 3;
/// How long an unreachable host keeps being retried.
pub const UNREACHABLE_WINDOW: Duration = Duration::from_secs(3600);
const UNREACHABLE_GAP_MAX: Duration = Duration::from_secs(30);

/// OpenCode's step retry schedule: exponential from 2s, capped at 10s per
/// gap, for 10 retries (about 84s of waiting in all); timeouts stop after 3;
/// an unreachable host retries on its own budget, gaps up to 30s, for an hour.
#[derive(Debug, Default)]
pub struct Policy {
    /// Retry only failures the provider never ran, at most this many times.
    one_shot: Option<u32>,
    general: u32,
    timeouts: u32,
    unreachable: u32,
    unreachable_since: Option<Instant>,
}

fn jitter(d: Duration) -> Duration {
    d.mul_f64(rand::thread_rng().gen_range(0.8..=1.2))
}

fn exponential(n: u32, cap: Duration) -> Duration {
    let base = Duration::from_secs(2).saturating_mul(1u32 << n.min(16));
    base.min(cap)
}

/// Whether the provider clearly never ran the request: a throttle, an
/// overloaded server, or a connection that was never made.
fn not_run(e: &Error) -> bool {
    match e {
        Error::Status { status, .. } if matches!(*status, 503 | 529) => true,
        // Only failures before a connection exists: a dropped interface or
        // route can also fail a response already being read.
        Error::Transport(m) => {
            let m = m.to_lowercase();
            [
                "connection refused",
                "dns error",
                "failed to lookup address",
                "name or service not known",
                "nodename nor servname",
                "temporary failure in name resolution",
            ]
            .iter()
            .any(|p| m.contains(p))
        }
        _ => classify(e) == Class::RateLimit,
    }
}

impl Policy {
    /// For one-shot calls (titles), which can be billed per attempt: retry
    /// only failures the provider never ran, three times (OpenCode's
    /// `retryOneShot`).
    pub fn one_shot() -> Policy {
        Policy {
            one_shot: Some(3),
            ..Policy::default()
        }
    }

    /// The wait before the next attempt, or `None` to give up. `now` is
    /// passed in so tests can step a clock.
    pub fn next(&mut self, e: &Error, now: Instant) -> Option<Duration> {
        if let Some(left) = &mut self.one_shot {
            if *left == 0 || !not_run(e) {
                return None;
            }
            *left -= 1;
        }
        if !classify(e).retryable() {
            return None;
        }
        if is_unreachable(e) {
            let since = *self.unreachable_since.get_or_insert(now);
            if now.duration_since(since) >= UNREACHABLE_WINDOW {
                return None;
            }
            let n = self.unreachable;
            self.unreachable += 1;
            return Some(jitter(exponential(n, UNREACHABLE_GAP_MAX)).min(UNREACHABLE_GAP_MAX));
        }
        if self.general >= MAX_RETRIES {
            return None;
        }
        if is_timeout(e) {
            self.timeouts += 1;
            if self.timeouts > MAX_TIMEOUT_RETRIES {
                return None;
            }
        }
        let n = self.general;
        self.general += 1;
        Some(jitter(exponential(n, Duration::from_secs(10))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(s: u16, body: &str) -> Error {
        Error::Status {
            status: s,
            body: body.into(),
        }
    }

    fn decisions(e: &Error, n: usize) -> Vec<bool> {
        let mut p = Policy::default();
        let now = Instant::now();
        (0..n).map(|_| p.next(e, now).is_some()).collect()
    }

    // session-runner-retry.test.ts

    #[test]
    fn oc_session_retry_stops_retrying_transport_timeouts_after_three_attempts() {
        let timeout = Error::Transport("error sending request: operation timed out".into());
        assert_eq!(decisions(&timeout, 4), [true, true, true, false]);
    }

    #[test]
    fn oc_session_retry_keeps_the_general_allowance_for_other_transient_failures() {
        let internal = status(500, "internal");
        let mut want = vec![true; 10];
        want.push(false);
        assert_eq!(decisions(&internal, 11), want);
    }

    #[test]
    fn oc_session_retry_keeps_retrying_an_unreachable_host_for_an_hour_with_gaps_of_at_most_30_seconds(
    ) {
        let dns = Error::Transport(
            "error sending request for url (https://opencode.ai/zen/v1/responses): client error (Connect): dns error: failed to lookup address information: nodename nor servname provided, or not known".into(),
        );
        let mut p = Policy::default();
        let start = Instant::now();
        let mut now = start;
        let mut delays = vec![];
        while let Some(d) = p.next(&dns, now) {
            delays.push(d);
            now += d;
        }
        let waited = now - start;
        assert!(delays.len() > 100, "{}", delays.len());
        assert!(delays.iter().all(|d| *d <= UNREACHABLE_GAP_MAX));
        assert!(waited >= UNREACHABLE_WINDOW);
        assert!(waited <= UNREACHABLE_WINDOW + UNREACHABLE_GAP_MAX);
    }

    #[test]
    fn oc_session_retry_keeps_a_refused_connection_on_the_general_allowance() {
        let refused = Error::Transport(
            "error sending request: client error (Connect): tcp connect error: Connection refused (os error 61)".into(),
        );
        assert!(!is_unreachable(&refused));
        let mut want = vec![true; 10];
        want.push(false);
        assert_eq!(decisions(&refused, 11), want);
    }

    #[test]
    fn oc_session_retry_doesnt_spend_the_general_allowance_on_an_unreachable_host() {
        let dns = Error::Transport("dns error: failed to lookup address information".into());
        let internal = status(503, "overloaded");
        let mut p = Policy::default();
        let now = Instant::now();
        for _ in 0..20 {
            assert!(p.next(&dns, now).is_some());
        }
        let got: Vec<bool> = (0..11).map(|_| p.next(&internal, now).is_some()).collect();
        let mut want = vec![true; 10];
        want.push(false);
        assert_eq!(got, want);
    }

    #[test]
    fn oc_model_retry_one_shot_doesnt_replay_a_call_cut_off_by_the_network_going_down() {
        let cut = Error::Transport("error decoding response body: Network is down".into());
        // A dropped interface can also fail a response already being read.
        let mut p = Policy::one_shot();
        assert!(p.next(&cut, Instant::now()).is_none());
        let mut p = Policy::one_shot();
        let limited = status(429, "slow down");
        let got: Vec<bool> = (0..4)
            .map(|_| p.next(&limited, Instant::now()).is_some())
            .collect();
        assert_eq!(got, [true, true, true, false]);
        assert!(Policy::one_shot()
            .next(&status(500, "x"), Instant::now())
            .is_none());
    }

    #[test]
    fn general_gaps_grow_from_two_seconds_to_a_ten_second_cap() {
        let mut p = Policy::default();
        let now = Instant::now();
        let e = status(500, "x");
        let gaps: Vec<Duration> = (0..10).map(|_| p.next(&e, now).unwrap()).collect();
        for (gap, nominal) in gaps.iter().zip([2u64, 4, 8, 10, 10, 10, 10, 10, 10, 10]) {
            let n = Duration::from_secs(nominal);
            assert!(
                *gap >= n.mul_f64(0.8) && *gap <= n.mul_f64(1.2),
                "{gap:?} vs {n:?}"
            );
        }
    }

    // session-error.test.ts

    #[test]
    fn oc_session_error_retries_rate_limits_provider_internal_transport_and_unrecognized_failures()
    {
        let eligible = [
            status(
                429,
                r#"{"error":{"message":"rate","type":"rate_limit_error"}}"#,
            ),
            status(500, "internal"),
            Error::Transport("connection reset by peer".into()),
            Error::Decode(r#"{"type":"mystery","message":"unknown"}"#.into()),
        ];
        let ineligible = [
            status(401, r#"{"error":{"message":"auth"}}"#),
            status(
                429,
                r#"{"error":{"message":"You exceeded your current quota","code":"insufficient_quota"}}"#,
            ),
            status(
                400,
                r#"{"error":{"message":"blocked","code":"content_policy_violation"}}"#,
            ),
            Error::Decode("expected value at line 1 column 1: not json".into()),
            status(
                400,
                r#"{"error":{"message":"bad request","type":"invalid_request_error"}}"#,
            ),
            status(404, "unsupported"),
        ];
        assert!(
            eligible.iter().all(|e| classify(e).retryable()),
            "{eligible:?}"
        );
        for e in &ineligible {
            assert!(!classify(e).retryable(), "{e:?} -> {:?}", classify(e));
        }
    }

    #[test]
    fn oc_session_error_separates_quota_caps_from_rate_limits() {
        assert_eq!(classify(&status(402, "pay")), Class::Quota);
        assert_eq!(
            classify(&status(429, "Your package has expired")),
            Class::Quota
        );
        assert_eq!(
            classify(&status(
                429,
                r#"{"error":{"code":"token_quota_exceeded","message":"Tokens per minute limit exceeded"}}"#
            )),
            Class::RateLimit
        );
        assert_eq!(
            classify(&status(429, "Too many requests")),
            Class::RateLimit
        );
    }

    #[test]
    fn mid_stream_error_frames_are_classified_by_their_codes() {
        // Anthropic reports overload inside a 200 stream.
        let overloaded =
            Error::Decode(r#"{"type":"overloaded_error","message":"Overloaded"}"#.into());
        assert_eq!(classify(&overloaded), Class::ProviderInternal);
        let overflow = Error::Decode(
            r#"{"message":"This model's maximum context length is 8192 tokens"}"#.into(),
        );
        assert_eq!(classify(&overflow), Class::ContextOverflow);
        let gateway = status(400, r#"{"error":{"type":"server_error","message":"x"}}"#);
        assert_eq!(
            classify(&gateway),
            Class::InvalidRequest,
            "a 4xx stays a rejection"
        );
    }

    #[test]
    fn non_retryable_failures_are_not_retried_at_all() {
        assert_eq!(decisions(&status(400, "bad"), 1), [false]);
        assert_eq!(decisions(&status(401, "no"), 1), [false]);
    }

    #[cfg(test)]
    mod provider_error_tests {
        use super::*;

        #[test]
        fn stream_errors_from_providers_are_classified() {
            let p = |m: &str, r: bool| {
                classify(&Error::Provider {
                    message: m.into(),
                    retryable: r,
                })
            };
            assert_eq!(
                p("overloaded_error: Overloaded", true),
                Class::ProviderInternal
            );
            assert_eq!(
                p("something odd happened", true),
                Class::ProviderInternal,
                "the protocol's retryable judgement is kept"
            );
            assert_eq!(
                p("something odd happened", false),
                Class::InvalidRequest,
                "a non-retryable error is not retried"
            );
            assert!(!p("something odd happened", false).retryable());
            assert_ne!(
                p("insufficient_quota: You exceeded your current quota", false),
                Class::ProviderInternal,
                "quota is not a transient failure"
            );
        }
    }
}
