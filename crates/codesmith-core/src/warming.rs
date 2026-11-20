//! Prompt-cache warming: after a request, a session that stays idle sends
//! a small keep-alive request on the same prefix before the provider's
//! cache expires, so the next real request still reads it. Ported from
//! OpenCode's `WarmingPlugin` (`packages/core/src/plugin/warming.ts`) and
//! pinned to it by the `oc_warming_*` tests below (from
//! `plugin/warming.test.ts` and `config/warming.test.ts`).
//!
//! The top-level `warming` (`true`, or `{prompt, interval, duration}`)
//! turns it on; a provider's or model's `cache.warming` overrides it.
//! `interval` `"auto"` pings at 90% of the model's `cache.ttl` (4 minutes
//! without one); pings stop `duration` (30 minutes) after the last real
//! request, after a ping that rebuilt the cache instead of reading it, and
//! while a request or compaction runs, waiting for it while the cache lasts.
//!
//! Deliberate differences: warming is per session object (a session that
//! is closed stops), and a ping repeats the session's latest request with
//! the keep-alive prompt added, rather than generating from the stored
//! session.

use codesmith_ai::Usage;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

pub const DEFAULT_PROMPT: &str =
    "This is a keep-alive request. Do not perform any work or use tools. Reply with exactly: OK";
/// The "auto" interval for a model without a `cache.ttl`: a little under
/// the five minutes most explicit caches last.
pub const FALLBACK_INTERVAL: Duration = Duration::from_secs(4 * 60);
pub const FALLBACK_TTL: Duration = Duration::from_secs(5 * 60);
pub const DEFAULT_DURATION: Duration = Duration::from_secs(30 * 60);

/// Resolved warming settings.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub prompt: String,
    /// Idle time between pings.
    pub interval: Duration,
    /// How long after the latest real request pings continue.
    pub duration: Duration,
    /// How long the provider keeps a cache entry after the request that
    /// last sent it.
    pub ttl: Duration,
}

/// Whether a session is warmed after a request, and why not.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Warm(Settings),
    /// Neither the top-level `warming` nor the model's `cache.warming`
    /// turns it on.
    Off,
    /// `"auto"` timing on a cache that lasts longer than `duration`.
    CacheOutlivesWindow,
    /// An explicit interval that is not positive and shorter than `duration`.
    InvalidInterval,
}

/// A duration such as `"2 minutes"`, `"1 hour"`, `"90s"` or `"5m"`.
pub fn parse_duration(text: &str) -> Option<Duration> {
    let t = text.trim();
    let split = t.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    let (n, unit) = t.split_at(split);
    let n: f64 = n.parse().ok()?;
    let ms = match unit.trim().to_lowercase().as_str() {
        "ms" | "millis" | "millisecond" | "milliseconds" => 1.0,
        "s" | "sec" | "secs" | "second" | "seconds" => 1_000.0,
        "m" | "min" | "mins" | "minute" | "minutes" => 60_000.0,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3_600_000.0,
        "d" | "day" | "days" => 86_400_000.0,
        _ => return None,
    };
    (n >= 0.0).then(|| Duration::from_millis((n * ms) as u64))
}

/// A `warming` setting: `true`/`false`, or an object of settings. `None`
/// when it is neither (an invalid setting).
fn valid(v: &Value) -> Option<&Value> {
    match v {
        Value::Bool(_) => Some(v),
        Value::Object(m) => {
            let ok = m.iter().all(|(k, x)| match k.as_str() {
                "prompt" => x.is_string(),
                "interval" => x == "auto" || x.as_str().and_then(parse_duration).is_some(),
                "duration" => x.as_str().and_then(parse_duration).is_some(),
                _ => true,
            });
            ok.then_some(v)
        }
        _ => None,
    }
}

fn enabled(v: &Value) -> bool {
    !matches!(v, Value::Bool(false))
}

/// Whether to warm, from the top-level `warming`, the model's
/// `cache.warming` (which overrides it; an invalid one falls back to the
/// top level) and the model's `cache.ttl`.
pub fn decide(global: Option<&Value>, local: Option<&Value>, ttl: Option<&str>) -> Decision {
    let local = local.and_then(valid);
    let global = global.and_then(valid);
    if !local.or(global).is_some_and(enabled) {
        return Decision::Off;
    }
    // The keep-alive prompt is only set at the top level; a model's
    // setting changes the timing.
    let field = |v: Option<&Value>, k: &str| v.and_then(|v| v.get(k)).cloned();
    let prompt = field(global, "prompt")
        .and_then(|p| p.as_str().map(String::from))
        .unwrap_or_else(|| DEFAULT_PROMPT.into());
    let pick = |k: &str| field(local, k).or_else(|| field(global, k));
    let duration = pick("duration")
        .and_then(|d| d.as_str().and_then(parse_duration))
        .unwrap_or(DEFAULT_DURATION);
    let ttl = ttl.and_then(parse_duration);
    let auto = pick("interval").is_none_or(|i| i == "auto");
    // "auto" pings at 90% of the cache's lifetime, so it lands in time.
    let interval = if auto {
        ttl.map_or(FALLBACK_INTERVAL, |t| t.mul_f64(0.9))
    } else {
        pick("interval")
            .and_then(|i| i.as_str().and_then(parse_duration))
            .unwrap_or(FALLBACK_INTERVAL)
    };
    // A cache that outlives the window needs no keep-alive.
    if auto && interval >= duration {
        return Decision::CacheOutlivesWindow;
    }
    if interval.is_zero() || interval >= duration {
        return Decision::InvalidInterval;
    }
    // Without a cache.ttl, an explicit interval longer than five minutes
    // implies a cache that lasts at least that long.
    Decision::Warm(Settings {
        prompt,
        interval,
        duration,
        ttl: ttl.unwrap_or(FALLBACK_TTL.max(interval)),
    })
}

/// Sends one keep-alive request and returns its usage (`None` when it
/// failed or reported none).
pub type Ping =
    Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = Option<Usage>> + Send>> + Send + Sync>;

/// One session's warming: at most one loop, replaced by each real request.
pub struct Warmer {
    task: Option<tokio::task::JoinHandle<()>>,
    /// Whether a request (or compaction) is running.
    running: watch::Sender<bool>,
}

impl Default for Warmer {
    fn default() -> Self {
        Warmer {
            task: None,
            running: watch::channel(false).0,
        }
    }
}

impl Drop for Warmer {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Warmer {
    /// A request is being sent with `decision`. `tracked`: it reports its
    /// end with [`Warmer::end`]. A `Warm` decision replaces the loop, which
    /// restarts the interval and the window and cancels a ping in flight;
    /// any other stops it.
    pub fn send(&mut self, decision: Decision, tracked: bool, ping: Ping) {
        if tracked {
            self.running.send_replace(true);
        }
        self.stop();
        if let Decision::Warm(settings) = decision {
            let running = self.running.subscribe();
            self.task = Some(tokio::spawn(warm(settings, Instant::now(), running, ping)));
        }
    }

    /// The request or compaction ended.
    pub fn end(&self) {
        self.running.send_replace(false);
    }

    /// Stop pinging.
    pub fn stop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }

    /// Whether a loop is running.
    pub fn active(&self) -> bool {
        self.task.as_ref().is_some_and(|t| !t.is_finished())
    }
}

/// The loop: the interval counts from when the latest request or ping was
/// sent, since that is when the cache's lifetime starts.
async fn warm(
    settings: Settings,
    mut sent: Instant,
    mut running: watch::Receiver<bool>,
    ping: Ping,
) {
    let expires = sent + settings.duration;
    loop {
        let due = (sent + settings.interval).min(expires);
        tokio::time::sleep_until(due).await;
        if Instant::now() >= expires {
            return;
        }
        if *running.borrow() {
            // Ping as soon as the request ends, unless it outlasts the
            // cache: then a ping would only write a new one.
            let left = (sent + settings.ttl).saturating_duration_since(Instant::now());
            let ended = running.wait_for(|r| !*r);
            if tokio::time::timeout(left, ended).await.is_err() {
                return;
            }
        }
        sent = Instant::now();
        // A ping that wrote more than it read rebuilt the cache instead of
        // keeping it; providers that report no cache usage never miss.
        if let Some(u) = ping(settings.prompt.clone()).await {
            if u.cache_read < u.cache_write {
                return;
            }
        }
    }
}

/// The `cache.warming` of a model in config: the model's own, else its
/// provider's (in `cache`, `settings.cache` or `options.cache`).
pub fn model_setting(config: &crate::config::Config, spec: &str) -> Option<Value> {
    let (provider, model) = spec.split_once('/')?;
    let model = model.split('#').next().unwrap_or(model);
    let p = config.providers.get(provider)?;
    let from = |c: Option<&Value>| c.and_then(|c| c.get("warming")).cloned();
    let m = p.models.get(model);
    m.and_then(|m| from(m.cache.as_ref()).or_else(|| from(m.options.get("cache"))))
        .or_else(|| from(p.cache.as_ref()))
        .or_else(|| from(p.settings.cache.as_ref()))
        .or_else(|| from(p.options.get("cache")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    const MIN: f64 = 60.0;

    fn hit() -> Usage {
        Usage {
            cache_read: 1000,
            cache_write: 10,
            ..Default::default()
        }
    }
    fn miss() -> Usage {
        Usage {
            cache_read: 0,
            cache_write: 1000,
            ..Default::default()
        }
    }
    fn unreported() -> Usage {
        Usage::default()
    }

    /// The plugin under test, with a `warming` setting, the model's cache
    /// (`ttl`, `warming`), the cache usage each ping reports and how long a
    /// ping takes to respond.
    struct Run {
        warmer: Warmer,
        global: Value,
        cache: Arc<Mutex<Value>>,
        warms: Arc<Mutex<Vec<f64>>>,
        ping: Ping,
        start: Instant,
    }

    impl Run {
        fn new(global: Value, cache: Value, usage: Vec<Usage>, respond: Duration) -> Run {
            let start = Instant::now();
            let warms: Arc<Mutex<Vec<f64>>> = Arc::default();
            let seen = warms.clone();
            let ping: Ping = Arc::new(move |_prompt| {
                let mut w = seen.lock().unwrap();
                let u = usage.get(w.len()).copied().unwrap_or_else(unreported);
                w.push((Instant::now() - start).as_secs_f64() / MIN);
                Box::pin(async move {
                    tokio::time::sleep(respond).await;
                    Some(u)
                })
            });
            Run {
                warmer: Warmer::default(),
                global,
                cache: Arc::new(Mutex::new(cache)),
                warms,
                ping,
                start,
            }
        }

        fn decision(&self) -> Decision {
            let cache = self.cache.lock().unwrap().clone();
            decide(
                Some(&self.global),
                cache.get("warming"),
                cache.get("ttl").and_then(Value::as_str),
            )
        }

        /// A request being sent.
        fn send(&mut self) {
            let d = self.decision();
            self.warmer.send(d, true, self.ping.clone());
        }
        /// A compaction request being sent.
        fn compact(&mut self) {
            self.send();
        }
        /// The turn (or a compaction) ending.
        fn end(&self) {
            self.warmer.end();
        }
        /// One whole turn whose response ends right away.
        fn turn(&mut self) {
            self.send();
            self.end();
        }
        async fn at(&self, minutes: f64) {
            tokio::time::sleep_until(self.start + Duration::from_secs_f64(minutes * MIN)).await;
            for _ in 0..20 {
                tokio::task::yield_now().await;
            }
        }
        fn warms(&self) -> Vec<f64> {
            self.warms.lock().unwrap().clone()
        }
    }

    fn run(global: Value, cache: Value) -> Run {
        Run::new(global, cache, vec![], Duration::ZERO)
    }

    #[test]
    fn oc_warming_config_accepts_boolean_enablement() {
        assert_eq!(decide(None, None, None), Decision::Off);
        assert_eq!(decide(Some(&json!(false)), None, None), Decision::Off);
        assert!(matches!(
            decide(Some(&json!(true)), None, None),
            Decision::Warm(_)
        ));
        let c: crate::config::Config = serde_json::from_value(json!({"warming": true})).unwrap();
        assert_eq!(c.warming, Some(json!(true)));
    }

    #[test]
    fn oc_warming_config_decodes_custom_durations() {
        let w = json!({"prompt": "Reply pong", "interval": "2 minutes", "duration": "1 hour"});
        let Decision::Warm(s) = decide(Some(&w), None, None) else {
            panic!("warm");
        };
        assert_eq!(s.prompt, "Reply pong");
        assert_eq!(s.interval, Duration::from_secs(120));
        assert_eq!(s.duration, Duration::from_secs(3600));
    }

    #[tokio::test(start_paused = true)]
    async fn oc_warming_warms_every_4_minutes_when_the_model_has_no_cache_ttl() {
        let mut w = run(json!(true), json!({}));
        w.turn();
        for m in [4.0, 8.0] {
            w.at(m).await;
        }
        assert_eq!(w.warms(), [4.0, 8.0]);
    }

    #[tokio::test(start_paused = true)]
    async fn oc_warming_warms_at_90_percent_of_the_models_cache_ttl_by_default() {
        let mut w = run(json!(true), json!({"ttl": "5m"}));
        w.turn();
        for m in [4.0, 4.5, 9.0] {
            w.at(m).await;
        }
        assert_eq!(w.warms(), [4.5, 9.0]);
    }

    #[tokio::test(start_paused = true)]
    async fn oc_warming_doesnt_warm_a_cache_that_outlives_the_warming_window() {
        let mut w = run(json!(true), json!({"ttl": "1h"}));
        w.turn();
        for m in [4.0, 27.0, 54.0] {
            w.at(m).await;
        }
        assert!(w.warms().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn oc_warming_keeps_an_explicit_interval() {
        let mut w = run(json!({"interval": "2 minutes"}), json!({"ttl": "5m"}));
        w.turn();
        for m in [2.0, 4.0] {
            w.at(m).await;
        }
        assert_eq!(w.warms(), [2.0, 4.0]);
    }

    #[tokio::test(start_paused = true)]
    async fn oc_warming_keeps_warming_after_a_ping_hits_the_cache() {
        let mut w = Run::new(
            json!(true),
            json!({}),
            vec![hit(), hit(), hit()],
            Duration::ZERO,
        );
        w.turn();
        for m in [4.0, 8.0, 12.0] {
            w.at(m).await;
        }
        assert_eq!(w.warms(), [4.0, 8.0, 12.0]);
    }

    #[tokio::test(start_paused = true)]
    async fn oc_warming_stops_warming_after_a_ping_misses_the_cache() {
        let mut w = Run::new(
            json!(true),
            json!({}),
            vec![hit(), miss(), hit()],
            Duration::ZERO,
        );
        w.turn();
        for m in [4.0, 8.0, 12.0, 16.0] {
            w.at(m).await;
        }
        assert_eq!(w.warms(), [4.0, 8.0]);
    }

    #[tokio::test(start_paused = true)]
    async fn oc_warming_resumes_warming_on_the_next_real_request_after_a_miss() {
        let mut w = Run::new(
            json!(true),
            json!({}),
            vec![miss(), hit(), hit()],
            Duration::ZERO,
        );
        w.turn();
        for m in [4.0, 8.0] {
            w.at(m).await;
        }
        w.turn();
        for m in [12.0, 16.0] {
            w.at(m).await;
        }
        assert_eq!(w.warms(), [4.0, 12.0, 16.0]);
    }

    #[tokio::test(start_paused = true)]
    async fn oc_warming_doesnt_count_a_ping_without_cache_usage_as_a_miss() {
        let mut w = Run::new(
            json!(true),
            json!({}),
            vec![unreported(), unreported()],
            Duration::ZERO,
        );
        w.turn();
        for m in [4.0, 8.0] {
            w.at(m).await;
        }
        assert_eq!(w.warms(), [4.0, 8.0]);
    }

    #[tokio::test(start_paused = true)]
    async fn oc_warming_keeps_warming_when_a_request_is_sent_while_a_missed_ping_responds() {
        let mut w = Run::new(
            json!(true),
            json!({"ttl": "5m"}),
            vec![miss(), hit()],
            Duration::from_secs(30),
        );
        w.turn();
        w.at(4.5).await;
        w.at(4.75).await;
        w.turn();
        for m in [5.0, 9.25] {
            w.at(m).await;
        }
        assert_eq!(w.warms(), [4.5, 9.25]);
    }

    #[tokio::test(start_paused = true)]
    async fn oc_warming_counts_the_interval_from_when_the_request_was_sent() {
        let mut w = run(json!(true), json!({"ttl": "5m"}));
        w.send();
        w.at(1.0).await;
        w.end();
        for m in [4.5, 5.5] {
            w.at(m).await;
        }
        assert_eq!(w.warms(), [4.5]);
    }

    #[tokio::test(start_paused = true)]
    async fn oc_warming_counts_the_next_interval_from_when_a_ping_was_sent() {
        let mut w = Run::new(
            json!(true),
            json!({"ttl": "5m"}),
            vec![],
            Duration::from_secs(30),
        );
        w.turn();
        for m in [4.5, 9.0, 9.5] {
            w.at(m).await;
        }
        assert_eq!(w.warms(), [4.5, 9.0]);
    }

    #[tokio::test(start_paused = true)]
    async fn oc_warming_waits_for_a_request_running_at_the_due_time() {
        let mut w = run(json!(true), json!({"ttl": "5m"}));
        w.send();
        w.at(4.5).await;
        assert!(w.warms().is_empty());
        w.at(4.75).await;
        w.end();
        for m in [9.0, 9.25] {
            w.at(m).await;
        }
        assert_eq!(w.warms(), [4.75, 9.25]);
    }

    #[tokio::test(start_paused = true)]
    async fn oc_warming_stops_warming_when_a_request_outlasts_the_cache() {
        let mut w = run(json!(true), json!({"ttl": "5m"}));
        w.send();
        w.at(6.0).await;
        w.end();
        w.at(15.0).await;
        assert!(w.warms().is_empty());
        w.turn();
        w.at(19.5).await;
        assert_eq!(w.warms(), [19.5]);
    }

    #[tokio::test(start_paused = true)]
    async fn oc_warming_keeps_one_loop_when_turned_off_and_on_again() {
        let mut w = Run::new(
            json!(false),
            json!({"warming": true}),
            vec![],
            Duration::from_secs(30),
        );
        w.turn();
        w.at(1.0).await;
        *w.cache.lock().unwrap() = json!({"warming": false});
        w.turn();
        w.at(2.0).await;
        *w.cache.lock().unwrap() = json!({"warming": true});
        w.turn();
        for m in 3..=40 {
            w.at(m as f64).await;
        }
        assert_eq!(w.warms(), [6.0, 10.0, 14.0, 18.0, 22.0, 26.0, 30.0]);
    }

    #[tokio::test(start_paused = true)]
    async fn oc_warming_doesnt_ping_during_a_running_compaction() {
        let mut w = run(json!(true), json!({"ttl": "5m"}));
        w.compact();
        w.at(4.5).await;
        assert!(w.warms().is_empty());
        w.at(4.75).await;
        w.end();
        for m in [9.0, 9.25] {
            w.at(m).await;
        }
        assert_eq!(w.warms(), [4.75, 9.25]);
    }

    #[tokio::test(start_paused = true)]
    async fn oc_warming_stops_warming_a_deleted_session() {
        let mut w = run(json!(true), json!({}));
        w.turn();
        w.at(4.0).await;
        w.warmer.stop();
        for m in [8.0, 12.0] {
            w.at(m).await;
        }
        assert_eq!(w.warms(), [4.0]);
    }

    #[tokio::test(start_paused = true)]
    async fn oc_warming_stops_every_loop_when_the_plugin_closes() {
        let mut w = run(json!(true), json!({}));
        w.turn();
        drop(std::mem::take(&mut w.warmer));
        for m in [4.0, 8.0] {
            w.at(m).await;
        }
        assert!(w.warms().is_empty());
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("2 minutes"), Some(Duration::from_secs(120)));
        assert_eq!(parse_duration("1h"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_duration("250ms"), Some(Duration::from_millis(250)));
        assert_eq!(parse_duration("soon"), None);
    }
}
