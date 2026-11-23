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

/// A provider failure as the session reports it. An unreachable host names
/// the network as the problem (OpenCode's `toSessionError`), so a DNS
/// outage does not read like a broken model or provider; anything else is
/// the error's own text.
pub fn message(e: &Error) -> String {
    match e {
        Error::Transport(m) if is_unreachable(e) => {
            // reqwest names the URL as "... for url (https://host/...)".
            let host = m
                .split_once("for url (")
                .and_then(|(_, rest)| rest.split(')').next())
                .and_then(|u| reqwest::Url::parse(u).ok())
                .and_then(|u| u.host_str().map(str::to_string));
            format!(
                "Can't reach {}. Check the network connection. ({m})",
                host.as_deref().unwrap_or("the provider")
            )
        }
        _ => e.to_string(),
    }
}

/// Where a failed request went, for naming the step that fixes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Target {
    pub provider_id: String,
    /// The provider's name (`LM Studio`), else its id.
    pub provider_name: String,
    pub base_url: String,
    /// A server on this machine or network.
    pub local: bool,
    /// Whether the request carried a key.
    pub has_key: bool,
    /// For Anthropic while Claude Code is signed in: the Claude Code model
    /// to switch to (`claude-agent/opus`).
    pub claude_code: Option<String>,
}

impl Target {
    /// The target of requests to `model`; `config` names the provider and
    /// says whether Claude Code is signed in.
    pub fn of(model: &crate::config::ModelRef, config: Option<&crate::config::Config>) -> Target {
        let pid = model.provider_id.clone();
        let provider_name = config
            .and_then(|c| c.provider(&pid))
            .and_then(|p| p.name)
            // "LM Studio (local)": the sentence says it is a server.
            .map(|n| n.trim_end_matches(" (local)").to_string())
            .unwrap_or_else(|| pid.clone());
        let claude_code = config.filter(|_| pid == "anthropic").and_then(|c| {
            let access = crate::model_spec::Access::current(c);
            crate::model_spec::claude_code_for(c, &model.model_id, &access)
        });
        Target {
            provider_id: pid,
            provider_name,
            base_url: model.provider.base_url.clone(),
            local: model.local,
            has_key: model
                .provider
                .api_key
                .as_deref()
                .is_some_and(|k| !k.trim().is_empty()),
            claude_code,
        }
    }
}

/// The scheme, host and port of the URL a transport error names.
fn error_origin(m: &str) -> Option<String> {
    let url = m.split_once("for url (")?.1.split(')').next()?;
    let u = reqwest::Url::parse(url).ok()?;
    let host = u.host_str()?;
    Some(match u.port() {
        Some(p) => format!("{}://{host}:{p}", u.scheme()),
        None => format!("{}://{host}", u.scheme()),
    })
}

/// The step that fixes `e`, when it is a missing or rejected login or a
/// server that is not there, named for where the request went.
pub fn advice(e: &Error, t: &Target) -> Option<String> {
    let name = if t.provider_name.is_empty() {
        t.provider_id.as_str()
    } else {
        t.provider_name.as_str()
    };
    let pid = t.provider_id.as_str();
    let switch = "switch models with ctrl+x m";
    if let Error::Transport(m) = e {
        // A hosted provider refusing is reported in its own words
        // (OpenCode's `toSessionError`); a local server is most likely
        // just not started.
        if !t.local || !m.to_lowercase().contains("connection refused") {
            return None;
        }
        let url = Some(t.base_url.as_str())
            .filter(|u| !u.is_empty())
            .map(str::to_string)
            .or_else(|| error_origin(m))
            .unwrap_or_else(|| "its configured URL".into());
        return Some(format!(
            "The {name} server isn't running at {url}; start it, or {switch}."
        ));
    }
    let (status, body) = match e {
        Error::Status { status, body } => (Some(*status), body.as_str()),
        Error::Provider { message, .. } => (None, message.as_str()),
        Error::Decode(body) => (None, body.as_str()),
        _ => return None,
    };
    if classify(e) != Class::Authentication {
        return None;
    }
    // Claude Code's own login failed: it is fixed there.
    if pid == crate::providers::claude_agent::ID {
        return Some(codesmith_ai::claude_agent::login_hint(
            "Claude Code isn't signed in. Run `claude auth login` in a terminal, then /login → Claude Pro/Max; or switch models with ctrl+x m.",
            codesmith_ai::claude_agent::saved_token().is_some(),
            codesmith_ai::claude_agent::keychain_locked,
        ));
    }
    let lower = body.to_lowercase();
    let missing = !t.has_key
        || [
            "is required",
            "missing",
            "no api key",
            "not provided",
            "no auth",
        ]
        .iter()
        .any(|w| lower.contains(w));
    let login = format!("`codesmith auth login {pid}`");
    Some(match (&t.claude_code, missing) {
        (Some(m), true) if pid == "anthropic" => format!(
            "No Anthropic API key. You're signed into Claude Code — switch to {m}, or run {login}."
        ),
        (Some(m), false) if pid == "anthropic" => format!(
            "Anthropic rejected the API key. You're signed into Claude Code — switch to {m}, or run {login} with a valid key."
        ),
        (_, true) => format!("No API key for {name}. Run {login}, or {switch}."),
        _ if status == Some(403) => format!(
            "{name} refused the request (HTTP 403): the key may not have access to this model. Run {login} with another key, or {switch}."
        ),
        _ => format!(
            "{name} rejected the API key{}. Run {login} to sign in again, or {switch}.",
            status.map(|s| format!(" (HTTP {s})")).unwrap_or_default()
        ),
    })
}

/// A provider failure as the session reports it: the step that fixes it
/// first, when there is one ([`advice`]), then [`message`].
pub fn explain(e: &Error, t: &Target) -> String {
    let raw = message(e);
    match advice(e, t) {
        Some(a) => format!("{a}\n{raw}"),
        None => raw,
    }
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
    /// Never retry.
    never: bool,
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

    /// This policy for a request on `protocol`. Claude Code already retries
    /// API failures (Claude Agent), so retrying its turn would only repeat
    /// permanent ones, such as a billing error (OpenCode's retry hook).
    pub fn for_protocol(self, protocol: codesmith_ai::Protocol) -> Policy {
        Policy {
            never: self.never || protocol == codesmith_ai::Protocol::ClaudeAgent,
            ..self
        }
    }

    /// The wait before the next attempt, or `None` to give up. `now` is
    /// passed in so tests can step a clock.
    pub fn next(&mut self, e: &Error, now: Instant) -> Option<Duration> {
        if self.never {
            return None;
        }
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
    fn oc_session_runner_retry_names_the_network_when_the_providers_host_cant_be_reached() {
        let dns = Error::Transport("error sending request for url (https://opencode.ai/zen/v1/responses): client error (Connect): dns error: failed to lookup address information: nodename nor servname provided, or not known".into());
        assert_eq!(
            message(&dns),
            format!(
                "Can't reach opencode.ai. Check the network connection. ({})",
                match &dns {
                    Error::Transport(m) => m,
                    _ => unreachable!(),
                }
            )
        );
        // Without a URL it names the provider.
        let bare = Error::Transport("dns error: failed to lookup address information".into());
        assert!(
            message(&bare).starts_with("Can't reach the provider. Check the network connection. (")
        );
        // A refused connection reached something: its own text.
        let refused = Error::Transport("error sending request for url (http://127.0.0.1:9/v1): client error (Connect): tcp connect error: Connection refused (os error 61)".into());
        assert_eq!(message(&refused), refused.to_string());
    }

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

/// [`advice`]: provider failures named by the step that fixes them.
#[cfg(test)]
mod advice_tests {
    use super::*;

    fn anthropic(claude_code: Option<&str>, has_key: bool) -> Target {
        Target {
            provider_id: "anthropic".into(),
            provider_name: "Anthropic".into(),
            base_url: "https://api.anthropic.com/v1".into(),
            local: false,
            has_key,
            claude_code: claude_code.map(str::to_string),
        }
    }

    fn lmstudio() -> Target {
        Target {
            provider_id: "lmstudio".into(),
            provider_name: "LM Studio".into(),
            base_url: "http://127.0.0.1:1234/v1".into(),
            local: true,
            has_key: false,
            claude_code: None,
        }
    }

    const NO_KEY: &str = r#"{"type":"error","error":{"type":"authentication_error","message":"x-api-key header is required"}}"#;
    const BAD_KEY: &str =
        r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
    const REFUSED: &str = "error sending request for url (http://127.0.0.1:1234/v1/chat/completions): client error (Connect): tcp connect error: Connection refused (os error 61)";

    fn status(s: u16, body: &str) -> Error {
        Error::Status {
            status: s,
            body: body.into(),
        }
    }

    // Regression: "HTTP 401: x-api-key header is required" was all a
    // Claude Code user without an Anthropic key was told.
    #[test]
    fn a_missing_anthropic_key_with_claude_code_signed_in_names_both_fixes() {
        let e = status(401, NO_KEY);
        let t = anthropic(Some("claude-agent/opus"), false);
        assert_eq!(
            advice(&e, &t).as_deref(),
            Some("No Anthropic API key. You're signed into Claude Code — switch to claude-agent/opus, or run `codesmith auth login anthropic`.")
        );
        // The summary leads; the raw error follows.
        let full = explain(&e, &t);
        let mut lines = full.lines();
        assert!(lines.next().unwrap().starts_with("No Anthropic API key."));
        assert_eq!(lines.next().unwrap(), e.to_string());
    }

    #[test]
    fn a_rejected_anthropic_key_with_claude_code_signed_in_says_it_was_rejected() {
        let a = advice(
            &status(401, BAD_KEY),
            &anthropic(Some("claude-agent/opus"), true),
        )
        .unwrap();
        assert!(a.starts_with("Anthropic rejected the API key."), "{a}");
        assert!(a.contains("claude-agent/opus"), "{a}");
    }

    #[test]
    fn without_claude_code_a_missing_key_names_the_login_command() {
        let a = advice(&status(401, NO_KEY), &anthropic(None, false)).unwrap();
        assert_eq!(
            a,
            "No API key for Anthropic. Run `codesmith auth login anthropic`, or switch models with ctrl+x m."
        );
        // A key that was sent and refused.
        let a = advice(&status(401, BAD_KEY), &anthropic(None, true)).unwrap();
        assert_eq!(
            a,
            "Anthropic rejected the API key (HTTP 401). Run `codesmith auth login anthropic` to sign in again, or switch models with ctrl+x m."
        );
        // A 403 is about access, not the key's existence.
        let a = advice(
            &status(403, r#"{"error":{"type":"permission_error"}}"#),
            &anthropic(None, true),
        )
        .unwrap();
        assert!(a.contains("HTTP 403") && a.contains("access"), "{a}");
    }

    #[test]
    fn other_providers_auth_failures_name_their_own_login() {
        let t = Target {
            provider_id: "openai".into(),
            provider_name: "OpenAI".into(),
            has_key: true,
            ..Target::default()
        };
        let a = advice(&status(401, r#"{"error":{"code":"invalid_api_key"}}"#), &t).unwrap();
        assert!(a.contains("`codesmith auth login openai`"), "{a}");
        // An auth error inside a stream (no status).
        let e = Error::Provider {
            message: r#"{"error":{"type":"authentication_error"}}"#.into(),
            retryable: false,
        };
        assert!(advice(&e, &t).unwrap().contains("rejected the API key"));
        // Claude Code's own login.
        let t = Target {
            provider_id: "claude-agent".into(),
            ..Target::default()
        };
        let a = advice(&status(401, "{}"), &t).unwrap();
        assert!(a.contains("claude auth login"), "{a}");
        // No name: the id.
        let t = Target {
            provider_id: "p".into(),
            ..Target::default()
        };
        assert!(advice(&status(401, "{}"), &t)
            .unwrap()
            .contains("No API key for p."));
    }

    // Regression: a fresh install's local default whose server was not
    // running reported only "Connection refused (os error 61)".
    #[test]
    fn a_refused_local_server_is_named_with_its_url() {
        let e = Error::Transport(REFUSED.into());
        assert_eq!(
            advice(&e, &lmstudio()).as_deref(),
            Some("The LM Studio server isn't running at http://127.0.0.1:1234/v1; start it, or switch models with ctrl+x m.")
        );
        let full = explain(&e, &lmstudio());
        assert!(full.ends_with(&message(&e)), "{full}");
        // No configured URL: the one the error names.
        let t = Target {
            base_url: String::new(),
            ..lmstudio()
        };
        assert!(advice(&e, &t)
            .unwrap()
            .contains("at http://127.0.0.1:1234;"));
        // Neither: still a sentence.
        let bare = Error::Transport("tcp connect error: Connection refused".into());
        assert!(advice(&bare, &t).unwrap().contains("its configured URL"));
    }

    #[test]
    fn failures_with_no_step_to_name_keep_their_own_words() {
        let hosted = Target {
            provider_id: "fake".into(),
            local: false,
            ..lmstudio()
        };
        // A hosted provider refusing (OpenCode reports it as is).
        let e = Error::Transport(REFUSED.into());
        assert_eq!(advice(&e, &hosted), None);
        assert_eq!(explain(&e, &hosted), message(&e));
        for e in [
            status(500, "internal"),
            status(429, "rate limited"),
            status(400, r#"{"error":{"type":"invalid_request_error"}}"#),
            Error::Transport("dns error: failed to lookup address information".into()),
            Error::Transport("operation timed out".into()),
            Error::Decode("garbage".into()),
            Error::Invalid("no images".into()),
        ] {
            assert_eq!(advice(&e, &lmstudio()), None, "{e}");
            assert_eq!(
                advice(&e, &anthropic(Some("claude-agent/opus"), false)),
                None,
                "{e}"
            );
            assert_eq!(explain(&e, &lmstudio()), message(&e));
        }
    }

    // Retrying is unchanged: advice only words the final error.
    #[test]
    fn advice_does_not_change_what_is_retried() {
        let auth = status(401, NO_KEY);
        let mut p = Policy::default();
        assert_eq!(p.next(&auth, Instant::now()), None);
        let refused = Error::Transport(REFUSED.into());
        assert!(Policy::default().next(&refused, Instant::now()).is_some());
    }
}
