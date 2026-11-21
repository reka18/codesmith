//! The login check (OpenCode's `probe` and `status`): start `claude`
//! without sending a prompt, so no model call is made, and read the
//! version, the login and the models it reports.

use super::process::{Guard, Handler, Process, Spawn};
use super::query::Launch;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

/// Older Claude Code ignores verbatim prompts and can read user text as
/// its own commands.
pub const MINIMUM_VERSION: &str = "2.1.248";
/// Older Claude Code cannot report a model's context window without a
/// prompt.
pub const CONTEXT_DISCOVERY_VERSION: &str = "2.1.257";

/// What a `claude` reports when it starts.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Probe {
    pub version: Option<String>,
    /// Claude Code's account info (its login, never a credential).
    pub account: Value,
    /// Its models, each with a `contextWindow` once discovered.
    pub models: Vec<Value>,
}

impl Probe {
    pub fn from_init(init: &Value) -> Probe {
        Probe {
            version: init["claude_code_version"].as_str().map(str::to_string),
            account: init["account"].clone(),
            models: init["models"].as_array().cloned().unwrap_or_default(),
        }
    }

    pub fn model(&self, value: &str) -> Option<&Value> {
        self.models.iter().find(|m| m["value"] == value)
    }
}

/// Compares release numbers like 2.1.248, ignoring a pre-release or build
/// suffix.
fn at_least(version: &str, minimum: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.split(['-', '+'])
            .next()
            .unwrap_or_default()
            .split('.')
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    let (current, required) = (parse(version), parse(minimum));
    for (i, r) in required.iter().enumerate() {
        let c = current.get(i).copied().unwrap_or(0);
        if c != *r {
            return c > *r;
        }
    }
    true
}

/// Why this Claude Code cannot run a chat, if it cannot.
pub fn version_error(version: Option<&str>) -> Option<String> {
    if version.is_some_and(|v| at_least(v, MINIMUM_VERSION)) {
        return None;
    }
    let what = match version {
        Some(v) => format!("Claude Code {v} is too old; {MINIMUM_VERSION} or later is required."),
        None => "Claude Code did not report its version, so compatibility could not be checked."
            .to_string(),
    };
    Some(format!("{what} Run `claude update`."))
}

/// Only a missing credential counts as logged out.
pub fn is_logged_in(account: &Value) -> bool {
    account["tokenSource"] != "none" || !account["apiKeySource"].is_null()
}

struct NoRequests;

impl Handler for NoRequests {
    fn handle(
        &self,
        _request: Value,
    ) -> futures_util::future::BoxFuture<'static, Option<Result<Value, String>>> {
        Box::pin(async { Some(Err("not supported".to_string())) })
    }
}

/// Run the check: start, read the report, then (on a new enough Claude
/// Code) each model's context window, and close. A stuck start fails after
/// `timeout`; a stuck context discovery keeps what it found.
pub async fn run(launch: &Launch, timeout: Duration) -> Result<Probe, String> {
    run_with(launch, timeout, None).await
}

/// [`run`], holding `guard` (a registry's process slot) while `claude`
/// runs.
pub(crate) async fn run_with(
    launch: &Launch,
    timeout: Duration,
    guard: Option<Guard>,
) -> Result<Probe, String> {
    let launch = Launch {
        chat: false,
        persist: false,
        ..launch.clone()
    };
    let process = Process::start(
        Spawn {
            executable: launch.executable.clone(),
            args: launch.args(),
            env: launch.environment(),
            cwd: launch.directory.clone(),
        },
        Arc::new(NoRequests),
        guard,
    )?;
    let result = check(&process, &launch, timeout).await;
    process.close_input().await;
    process.kill();
    let _ = tokio::time::timeout(Duration::from_secs(5), process.exited()).await;
    result
}

async fn check(process: &Process, launch: &Launch, timeout: Duration) -> Result<Probe, String> {
    let init = tokio::time::timeout(timeout, process.request(launch.initialize()))
        .await
        .map_err(|_| {
            format!(
                "claude did not start within {} seconds",
                timeout.as_secs().max(1)
            )
        })??;
    let mut probe = Probe::from_init(&init);
    // No prompt is sent, so an old Claude Code can still list its models;
    // a chat's own start tells the user to update.
    if probe
        .version
        .as_deref()
        .is_some_and(|v| at_least(v, CONTEXT_DISCOVERY_VERSION))
    {
        let _ = tokio::time::timeout(timeout, discover(process, &mut probe.models)).await;
    }
    Ok(probe)
}

/// Set each model's context window in place, so models found before a
/// timeout keep theirs. `rawMaxTokens` is the capacity; `maxTokens` can be
/// Claude Code's own compaction window.
async fn discover(process: &Process, models: &mut [Value]) {
    for m in models.iter_mut() {
        let Some(value) = m["value"].as_str().map(str::to_string) else {
            continue;
        };
        if value == "default" {
            continue;
        }
        if process
            .request(json!({"subtype": "set_model", "model": value}))
            .await
            .is_err()
        {
            continue;
        }
        let Ok(usage) = process
            .request(json!({"subtype": "get_context_usage", "detail": "summary"}))
            .await
        else {
            continue;
        };
        if let Some(n) = usage["rawMaxTokens"].as_u64().filter(|n| *n > 0) {
            m["contextWindow"] = json!(n);
        }
    }
}
