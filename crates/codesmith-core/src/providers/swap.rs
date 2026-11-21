//! A provider's model swap: commands, from the user's own config, that list
//! the models a server can load and load one of them (for example a
//! two-Mac oMLX cluster that serves one model at a time).
//!
//! ```json
//! "providers": {"stable": {"swap": {
//!     "list": ["ssh", "stable", "/usr/local/bin/tblink", "models", "--json"],
//!     "use": ["ssh", "stable", "/usr/local/bin/tblink", "use", "{model}", "--json"],
//!     "timeout": 1200
//! }}}
//! ```
//!
//! Rules:
//! - Only the global config file (`<config dir>/codesmith.json[c]`) may set
//!   `swap`: [`crate::config::load_from`] drops it from project files,
//!   `.codesmith/`, `$CODESMITH_CONFIG` and `$CODESMITH_CONFIG_CONTENT`,
//!   since it runs commands.
//! - The argv runs as given, with no shell. `{model}` must be a whole argv
//!   element, and is replaced by the model name as one element. The name
//!   must be one `list` returned (or, without `list`, a configured model of
//!   the provider) and of letters, digits and `. _ + - : @ /`, so it is safe
//!   even when the command is `ssh`, which hands its arguments to a shell on
//!   the other machine.
//! - Only a person starts a swap: the model picker and `codesmith models
//!   swap`. It is not a tool, and neither automations nor `run`, `serve` or
//!   ACP sessions reach it. [`guard`] refuses inside a CodeSmith shell tool.
//! - Output lines have control characters and escape sequences removed.
//!   `timeout` (default 600 s) bounds `use`; `list` gets at most 60 s.

use anyhow::{anyhow, bail, Result};
use serde::Deserialize;
use serde_json::Value;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

/// A provider's `swap` setting.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct SwapConfig {
    /// Lists the models the server can load (JSON or one name per line).
    pub list: Vec<String>,
    /// Loads `{model}` on the server.
    #[serde(rename = "use")]
    pub use_: Vec<String>,
    /// Seconds `use` may take before it is killed (default 600).
    pub timeout: Option<u64>,
}

/// The placeholder `use` replaces with the model name.
pub const PLACEHOLDER: &str = "{model}";
pub const DEFAULT_TIMEOUT: u64 = 600;
pub const LIST_TIMEOUT: u64 = 60;
/// Longest output line kept, in characters.
const LINE_MAX: usize = 2000;

impl SwapConfig {
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout.unwrap_or(DEFAULT_TIMEOUT).max(1))
    }
}

/// The provider's swap setting, if it has a usable one.
pub fn for_provider<'a>(cfg: &'a crate::config::Config, pid: &str) -> Option<&'a SwapConfig> {
    cfg.providers.get(pid)?.swap.as_ref()
}

/// Problems with a swap setting.
pub fn validate(s: &SwapConfig) -> Result<()> {
    if s.use_.first().is_none_or(|p| p.is_empty()) {
        bail!("swap.use must be a command: an array whose first element is the program");
    }
    if let Some(a) = s
        .use_
        .iter()
        .find(|a| *a != PLACEHOLDER && a.contains(PLACEHOLDER))
    {
        bail!("swap.use: \"{PLACEHOLDER}\" must be a whole element, not part of {a:?}");
    }
    if !s.use_.iter().any(|a| a == PLACEHOLDER) {
        bail!("swap.use must have \"{PLACEHOLDER}\" as one of its elements");
    }
    if !s.list.is_empty() && s.list[0].is_empty() {
        bail!("swap.list must be a command: an array whose first element is the program");
    }
    if s.list.iter().any(|a| a.contains(PLACEHOLDER)) {
        bail!("swap.list takes no \"{PLACEHOLDER}\"");
    }
    Ok(())
}

/// A model name that is safe as an argument here and on the far side of
/// `ssh`: letters, digits and `. _ + - : @ /`, not starting with `-` or `.`,
/// with no `..` part.
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && !s.starts_with(['-', '.', '/'])
        && !s.split('/').any(|p| p == ".." || p.is_empty())
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-' | ':' | '@' | '/')
        })
}

/// `template` with each `{model}` element replaced by `model`.
pub fn argv(template: &[String], model: &str) -> Vec<String> {
    template
        .iter()
        .map(|a| {
            if a == PLACEHOLDER {
                model.to_string()
            } else {
                a.clone()
            }
        })
        .collect()
}

/// `s` without escape sequences and control characters (tabs become spaces).
pub fn strip_controls(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']' | 'P' | 'X' | '^' | '_') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\t' => out.push(' '),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// A model the server can load.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SwapModel {
    pub name: String,
    /// Loaded on the server now.
    pub live: bool,
    /// The one the server is configured for.
    pub configured: bool,
    pub size_bytes: Option<u64>,
}

/// The models in `list` output: the last JSON line (an array of names or of
/// `{"name"|"id", "live", "configured", "size_bytes"}`, or an object with
/// such a `models` array and optionally `live` and `configured` names),
/// else one name per line. Names that are not [`valid_name`] are dropped.
pub fn parse_list(out: &str) -> Vec<SwapModel> {
    let json = out
        .lines()
        .rev()
        .map(str::trim)
        .filter(|l| l.starts_with('{') || l.starts_with('['))
        .find_map(|l| serde_json::from_str::<Value>(l).ok());
    let mut v: Vec<SwapModel> = match json {
        Some(j) => {
            let models = if j.is_array() {
                j.clone()
            } else {
                j["models"].clone()
            };
            let names = |k: &str| -> Vec<String> {
                match &j[k] {
                    Value::String(s) => vec![s.clone()],
                    Value::Array(a) => a
                        .iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect(),
                    _ => vec![],
                }
            };
            let (live, configured) = (names("live"), names("configured"));
            models
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|m| {
                    let name = m
                        .as_str()
                        .or_else(|| m["name"].as_str())
                        .or_else(|| m["id"].as_str())?
                        .to_string();
                    Some(SwapModel {
                        live: m["live"].as_bool().unwrap_or(false) || live.contains(&name),
                        configured: m["configured"].as_bool().unwrap_or(false)
                            || configured.contains(&name),
                        size_bytes: m["size_bytes"].as_u64(),
                        name,
                    })
                })
                .collect()
        }
        None => out
            .lines()
            .map(|l| strip_controls(l).trim().to_string())
            .filter(|l| !l.is_empty())
            .map(|name| SwapModel {
                name,
                ..SwapModel::default()
            })
            .collect(),
    };
    v.retain(|m| valid_name(&m.name));
    let mut seen = std::collections::HashSet::new();
    v.retain(|m| seen.insert(m.name.clone()));
    v
}

/// Refuse when running inside a CodeSmith session's shell tool, so the
/// model cannot start a swap through `codesmith models swap`.
pub fn guard() -> Result<()> {
    guard_with(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()))
}

pub fn guard_with(set: impl Fn(&str) -> bool) -> Result<()> {
    if set("CODESMITH_SESSION_ID") || set("CODESMITH") {
        bail!("a model swap cannot be started from inside a CodeSmith session's shell; run it in your own terminal");
    }
    Ok(())
}

/// What a command printed and how it ended.
struct Ran {
    ok: bool,
    code: Option<i32>,
    /// The last JSON line on stdout.
    json: Option<Value>,
    /// The last few non-empty lines, for errors.
    tail: Vec<String>,
    stdout: String,
}

async fn run(
    argv: &[String],
    timeout: Duration,
    keep_stdout: bool,
    on_line: &mut (dyn FnMut(&str) + Send),
) -> Result<Ran> {
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("CODESMITH_SERVER_TOKEN")
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow!("cannot run {}: {e}", argv[0]))?;
    let pid = child.id();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(bool, String)>();
    fn pump(
        r: impl AsyncRead + Unpin + Send + 'static,
        out: bool,
        tx: tokio::sync::mpsc::UnboundedSender<(bool, String)>,
    ) {
        tokio::spawn(async move {
            let mut r = BufReader::new(r);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match r.read_until(b'\n', &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let line: String = String::from_utf8_lossy(&buf)
                            .chars()
                            .take(LINE_MAX)
                            .collect();
                        if tx.send((out, line)).is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }
    pump(child.stdout.take().unwrap(), true, tx.clone());
    pump(child.stderr.take().unwrap(), false, tx);
    let mut ran = Ran {
        ok: false,
        code: None,
        json: None,
        tail: vec![],
        stdout: String::new(),
    };
    let work = async {
        while let Some((is_out, raw)) = rx.recv().await {
            let line = strip_controls(&raw).trim_end().to_string();
            if is_out {
                let t = raw.trim();
                if t.starts_with('{') {
                    if let Ok(v) = serde_json::from_str::<Value>(t) {
                        ran.json = Some(v);
                    }
                }
                if keep_stdout && ran.stdout.len() < 1 << 20 {
                    ran.stdout.push_str(&line);
                    ran.stdout.push('\n');
                }
            }
            if !line.trim().is_empty() {
                on_line(&line);
                ran.tail.push(line);
                if ran.tail.len() > 20 {
                    ran.tail.remove(0);
                }
            }
        }
        child.wait().await
    };
    match tokio::time::timeout(timeout, work).await {
        Ok(Ok(status)) => {
            ran.ok = status.success();
            ran.code = status.code();
            Ok(ran)
        }
        Ok(Err(e)) => Err(anyhow!("waiting for {}: {e}", argv[0])),
        Err(_) => {
            #[cfg(unix)]
            if let Some(p) = pid {
                unsafe { libc::killpg(p as i32, libc::SIGKILL) };
            }
            #[cfg(not(unix))]
            let _ = pid;
            bail!(
                "{} did not finish within {} s and was stopped",
                argv[0],
                timeout.as_secs()
            )
        }
    }
}

/// Run `list`.
pub async fn list(s: &SwapConfig) -> Result<Vec<SwapModel>> {
    validate(s)?;
    if s.list.is_empty() {
        bail!("swap.list is not set");
    }
    let timeout = s.timeout().min(Duration::from_secs(LIST_TIMEOUT));
    let ran = run(&s.list, timeout, true, &mut |_| {}).await?;
    if !ran.ok {
        bail!(
            "{} failed{}: {}",
            s.list[0],
            code(ran.code),
            ran.tail.last().cloned().unwrap_or_default()
        );
    }
    Ok(parse_list(&ran.stdout))
}

/// The models a provider can swap to: `list`'s, else its configured models.
pub async fn swappable(cfg: &crate::config::Config, pid: &str) -> Result<Vec<SwapModel>> {
    let s = for_provider(cfg, pid).ok_or_else(|| anyhow!("provider {pid} has no swap setting"))?;
    validate(s)?;
    if s.list.is_empty() {
        let models = cfg
            .providers
            .get(pid)
            .map(|p| p.models.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        return Ok(models
            .into_iter()
            .filter(|m| valid_name(m))
            .map(|name| SwapModel {
                name,
                ..SwapModel::default()
            })
            .collect());
    }
    list(s).await
}

fn code(c: Option<i32>) -> String {
    c.map(|c| format!(" (exit code {c})"))
        .unwrap_or_else(|| " (killed)".into())
}

/// How a `use` ended: the model id to select, or why it failed.
pub fn outcome(
    ok: bool,
    exit: Option<i32>,
    json: Option<&Value>,
    tail: &[String],
    requested: &str,
) -> Result<String> {
    let reported_fail = json.is_some_and(|j| j["ok"] == Value::Bool(false));
    if ok && !reported_fail {
        let id = json
            .and_then(|j| j["model_id"].as_str())
            .filter(|m| valid_name(m))
            .unwrap_or(requested);
        return Ok(id.to_string());
    }
    let mut why = json
        .and_then(|j| j["error"].as_str())
        .map(strip_controls)
        .or_else(|| tail.last().cloned())
        .unwrap_or_else(|| "no output".into());
    if let Some(b) = json.and_then(|j| j["shortfall_bytes"].as_u64()) {
        why.push_str(&format!("; {b} bytes ({:.2} GB) short", b as f64 / 1e9));
    }
    if json.is_some_and(|j| j["rolled_back"] == Value::Bool(true)) {
        why.push_str("; the server kept its previous model");
    }
    if !ok {
        why.push_str(&code(exit));
    }
    Err(anyhow!(why))
}

/// Run `use` for `model`, which must be in `known`. Each output line goes to
/// `on_line`. Returns the model id to select.
pub async fn swap(
    s: &SwapConfig,
    model: &str,
    known: &[String],
    on_line: &mut (dyn FnMut(&str) + Send),
) -> Result<String> {
    validate(s)?;
    if !valid_name(model) {
        bail!("{model:?} is not a model name a swap can take (letters, digits and . _ + - : @ /)");
    }
    if !known.iter().any(|k| k == model) {
        bail!("{model} is not one of the models the server lists");
    }
    let ran = run(&argv(&s.use_, model), s.timeout(), false, on_line).await?;
    outcome(ran.ok, ran.code, ran.json.as_ref(), &ran.tail, model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> String {
        format!(
            "{}/tests/fixtures/swap/fake-swap.sh",
            env!("CARGO_MANIFEST_DIR")
        )
    }

    fn cfg(timeout: Option<u64>) -> SwapConfig {
        SwapConfig {
            list: vec!["/bin/sh".into(), fixture(), "list".into()],
            use_: vec![
                "/bin/sh".into(),
                fixture(),
                "use".into(),
                PLACEHOLDER.into(),
            ],
            timeout,
        }
    }

    #[test]
    fn names() {
        for ok in [
            "Qwen3.5-122B-A10B-oQ3.5e-mtp",
            "llama3:8b",
            "org/model",
            "m@v2",
            "a+b_c",
        ] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in [
            "", "-rf", ".hidden", "/abs", "a/../b", "a//b", "a/", "a b", "a;b", "$(x)", "`x`",
            "a'b", "a\"b", "a|b", "a&b", "a\nb", "é", "a*",
        ] {
            assert!(!valid_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn settings_are_checked() {
        assert!(validate(&cfg(None)).is_ok());
        let mut s = cfg(None);
        s.use_ = vec![];
        assert!(validate(&s).is_err());
        s.use_ = vec!["x".into()];
        assert!(validate(&s)
            .unwrap_err()
            .to_string()
            .contains("as one of its elements"));
        s.use_ = vec!["x".into(), "--model={model}".into()];
        assert!(validate(&s)
            .unwrap_err()
            .to_string()
            .contains("whole element"));
        s.use_ = vec!["x".into(), PLACEHOLDER.into()];
        s.list = vec!["x".into(), PLACEHOLDER.into()];
        assert!(validate(&s).is_err());
        s.list = vec![];
        assert!(validate(&s).is_ok(), "list is optional");
        assert_eq!(cfg(None).timeout(), Duration::from_secs(600));
        assert_eq!(cfg(Some(0)).timeout(), Duration::from_secs(1));
    }

    #[test]
    fn the_model_is_one_element() {
        let t: Vec<String> = ["ssh", "stable", "tblink", "use", "{model}", "--json"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            argv(&t, "m x"),
            ["ssh", "stable", "tblink", "use", "m x", "--json"]
        );
    }

    #[test]
    fn controls_are_stripped() {
        assert_eq!(
            strip_controls("\x1b[31mred\x1b[0m\tok\r\x07\x1b]0;title\x07!"),
            "red ok!"
        );
        assert_eq!(strip_controls("a\u{9b}b\u{0}c"), "abc");
    }

    #[test]
    fn list_formats() {
        let tblink = r#"{"models_dir":"/m","models":[{"name":"a","size_bytes":5,"configured":true,"live":true},{"name":"b","configured":false,"live":false}],"configured":"a","live":["a"]}"#;
        assert_eq!(
            parse_list(&format!("Warning: added host key\n{tblink}\n")),
            vec![
                SwapModel {
                    name: "a".into(),
                    live: true,
                    configured: true,
                    size_bytes: Some(5)
                },
                SwapModel {
                    name: "b".into(),
                    ..SwapModel::default()
                },
            ]
        );
        assert_eq!(
            parse_list(r#"["x","y","x","bad name"]"#)
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>(),
            ["x", "y"]
        );
        assert_eq!(
            parse_list(r#"{"models":[{"id":"x"}],"live":"x"}"#)[0],
            SwapModel {
                name: "x".into(),
                live: true,
                ..SwapModel::default()
            }
        );
        assert_eq!(
            parse_list("one\n\x1b[1mtwo\x1b[0m\n\n")
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>(),
            ["one", "two"]
        );
    }

    #[test]
    fn outcomes() {
        assert_eq!(outcome(true, Some(0), None, &[], "m").unwrap(), "m");
        assert_eq!(
            outcome(
                true,
                Some(0),
                Some(&json!({"ok": true, "model_id": "org/m"})),
                &[],
                "m"
            )
            .unwrap(),
            "org/m"
        );
        assert_eq!(
            outcome(
                true,
                Some(0),
                Some(&json!({"ok": true, "model_id": "bad id"})),
                &[],
                "m"
            )
            .unwrap(),
            "m"
        );
        let e = outcome(false, Some(1), Some(&json!({"ok": false, "error": "m does not fit", "shortfall_bytes": 3_460_000_000u64, "rolled_back": true})), &[], "m")
            .unwrap_err()
            .to_string();
        assert_eq!(e, "m does not fit; 3460000000 bytes (3.46 GB) short; the server kept its previous model (exit code 1)");
        assert!(
            outcome(
                true,
                Some(0),
                Some(&json!({"ok": false, "error": "x"})),
                &[],
                "m"
            )
            .is_err(),
            "the report wins"
        );
        assert_eq!(
            outcome(false, Some(2), None, &["boom".into()], "m")
                .unwrap_err()
                .to_string(),
            "boom (exit code 2)"
        );
    }

    #[test]
    fn the_shell_tool_cannot_swap() {
        assert!(guard_with(|k| k == "CODESMITH").is_err());
        assert!(guard_with(|k| k == "CODESMITH_SESSION_ID").is_err());
        assert!(guard_with(|_| false).is_ok());
    }

    #[tokio::test]
    async fn listing_runs_the_command() {
        let models = list(&cfg(None)).await.unwrap();
        assert_eq!(
            models.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            ["small", "big", "huge"]
        );
        assert!(models[0].live && models[0].configured);
    }

    #[tokio::test]
    async fn a_swap_that_works_streams_clean_lines() {
        let mut lines = Vec::new();
        let id = swap(&cfg(None), "big", &["big".into()], &mut |l| {
            lines.push(l.to_string())
        })
        .await
        .unwrap();
        assert_eq!(id, "big");
        assert!(lines.iter().any(|l| l == "loading big"), "{lines:?}");
        assert!(
            lines.iter().all(|l| !l.chars().any(char::is_control)),
            "{lines:?}"
        );
        assert!(lines.iter().any(|l| l.contains("progress on stderr")));
    }

    #[tokio::test]
    async fn a_swap_that_does_not_fit_reports_why() {
        let e = swap(&cfg(None), "huge", &["huge".into()], &mut |_| {})
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("huge does not fit"), "{e}");
        assert!(e.contains("GB) short"), "{e}");
        assert!(e.contains("kept its previous model"), "{e}");
    }

    #[tokio::test]
    async fn unknown_or_unsafe_names_never_run() {
        let known = vec!["big".to_string()];
        let e = swap(&cfg(None), "other", &known, &mut |_| panic!("ran"))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("not one of the models"));
        let e = swap(
            &cfg(None),
            "big;reboot",
            &["big;reboot".into()],
            &mut |_| panic!("ran"),
        )
        .await
        .unwrap_err();
        assert!(e.to_string().contains("not a model name"));
    }

    #[tokio::test]
    async fn a_slow_swap_is_stopped_at_the_timeout() {
        let t = std::time::Instant::now();
        let e = swap(&cfg(Some(1)), "slow", &["slow".into()], &mut |_| {})
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("did not finish within 1 s"), "{e}");
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn a_missing_program_is_an_error() {
        let mut s = cfg(None);
        s.use_[0] = "/nonexistent/swap".into();
        assert!(swap(&s, "big", &["big".into()], &mut |_| {})
            .await
            .unwrap_err()
            .to_string()
            .contains("cannot run"));
    }
}
