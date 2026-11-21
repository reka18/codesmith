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
//!   the other machine, and not shaped like a remote path or URL
//!   ([`valid_name`]).
//! - The model selected afterwards is the requested one, or the
//!   `model_id` `use` reports when it is one `list` offered; another
//!   reported id is ignored, with a warning.
//! - Only a person starts a swap: the model picker and `codesmith models
//!   swap`. It is not a tool, and neither automations nor `run`, `serve` or
//!   ACP sessions reach it. [`guard`] refuses inside a CodeSmith shell tool,
//!   and [`shell_rules`] make running `codesmith models swap` from one ask.
//!   Neither is a security boundary: the model's shell can run the
//!   commands themselves, and only `shell` rules govern that.
//! - Output lines have control characters, escape sequences and Unicode
//!   format characters removed, and are read in bounded chunks: a line is
//!   kept up to [`LINE_MAX`] characters (the rest, up to its newline, is
//!   dropped), each stream up to 8 MiB, and `list`'s stdout is parsed whole
//!   up to 1 MiB. `timeout` (default 600 s) bounds `use`; `list` gets at
//!   most 60 s.

use anyhow::{anyhow, bail, Result};
use serde::Deserialize;
use serde_json::Value;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};

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
/// Longest output line held while reading, in bytes (`LINE_MAX` characters
/// of up to 4 bytes): the rest of a longer line is read and dropped, up to
/// its newline, so a line with no end never grows in memory.
const LINE_BYTES: usize = LINE_MAX * 4;
/// Most bytes of lines passed on from one stream; the rest is read and
/// dropped, so the command never blocks on a full pipe.
const OUTPUT_MAX: usize = 8 << 20;
/// Most of `list`'s stdout kept whole for parsing (a JSON listing on one
/// long line included). More is an error.
const STDOUT_MAX: usize = 1 << 20;
/// How much a stream is read at a time.
const CHUNK: usize = 8 << 10;

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
/// with no `..` part, and not shaped like a remote path or URL, which a
/// command such as `scp`, `rsync` or `git` would reach out to: no `@`
/// together with `:` (`user@host:path`), no `:` followed by `/`
/// (`host:/path`, `scheme://`), and no one-letter head before a `:`
/// (`x:path`, a drive). Ollama-style tags (`qwen:7b`, `org/llama3:8b`)
/// stay valid.
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && !s.starts_with(['-', '.', '/'])
        && !s.split('/').any(|p| p == ".." || p.is_empty())
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-' | ':' | '@' | '/')
        })
        && !(s.contains('@') && s.contains(':'))
        && !s.contains(":/")
        && !s.split_once(':').is_some_and(|(head, _)| head.len() == 1)
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

/// `s` without escape sequences, control characters and Unicode format
/// characters ([`crate::unicode::format_char`]: bidi controls, zero-width
/// characters); tabs become spaces.
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
            c if c.is_control() || crate::unicode::format_char(c) => {}
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

/// Built-in `shell` rules for every agent: running `codesmith models swap`
/// (by name or by a path to the binary) asks. They are
/// [`crate::permission::Rule::builtin`], so a broad configured allow does
/// not lift them; a rule configured for the agent, or an "always" answer,
/// does. [`guard`] refuses inside a session's shell anyway; this makes the
/// attempt visible first. Neither is a security boundary: the model's shell
/// could run the `swap` commands (`ssh host tblink use …`) themselves, which
/// only the `shell` rules govern.
pub fn shell_rules() -> Vec<crate::permission::Rule> {
    use crate::permission::{Effect, Rule};
    ["codesmith models swap*", "*/codesmith models swap*"]
        .iter()
        .map(|p| Rule::new("shell", p, Effect::Ask).into_builtin())
        .collect()
}

/// Refuse when running inside a CodeSmith session's shell tool, so the
/// model cannot start a swap through `codesmith models swap`. Not a
/// security boundary (see [`shell_rules`]).
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
    /// All of stdout, when kept, up to [`STDOUT_MAX`].
    stdout: String,
    /// Stdout was longer than [`STDOUT_MAX`].
    stdout_over: bool,
}

/// What [`pump`] read from one stream.
#[derive(Debug, Default)]
struct Pumped {
    /// Bytes read in all.
    read: u64,
    /// The most bytes of a line it held at once.
    held_max: usize,
    /// The stream's first [`STDOUT_MAX`] bytes, when asked for.
    raw: Vec<u8>,
    /// The stream was longer than that.
    raw_over: bool,
}

/// Read `r` to its end in [`CHUNK`]s, sending each line (without its
/// newline, cut to [`LINE_BYTES`], the rest of it dropped) to `tx` with
/// `out`, until [`OUTPUT_MAX`] bytes have gone; then it keeps reading and
/// drops what it reads. With `keep_raw`, the first [`STDOUT_MAX`] bytes are
/// kept whole too.
async fn pump(
    mut r: impl AsyncRead + Unpin,
    out: bool,
    keep_raw: bool,
    tx: tokio::sync::mpsc::Sender<(bool, String)>,
) -> Pumped {
    let mut p = Pumped::default();
    let mut buf = vec![0u8; CHUNK];
    let mut line: Vec<u8> = Vec::with_capacity(256);
    let mut sent = 0usize;
    let mut open = true;
    let emit = |line: &mut Vec<u8>, sent: &mut usize| {
        let text = (*sent < OUTPUT_MAX).then(|| {
            *sent += line.len() + 1;
            String::from_utf8_lossy(line)
                .chars()
                .take(LINE_MAX)
                .collect::<String>()
        });
        line.clear();
        text
    };
    loop {
        let n = match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        p.read += n as u64;
        if keep_raw && !p.raw_over {
            let room = STDOUT_MAX - p.raw.len();
            p.raw.extend_from_slice(&buf[..n.min(room)]);
            p.raw_over = n > room;
        }
        for piece in buf[..n].split_inclusive(|b| *b == b'\n') {
            let (body, ends) = match piece.strip_suffix(b"\n") {
                Some(b) => (b, true),
                None => (piece, false),
            };
            let room = LINE_BYTES.saturating_sub(line.len());
            line.extend_from_slice(&body[..body.len().min(room)]);
            p.held_max = p.held_max.max(line.capacity());
            if ends {
                if let Some(t) = emit(&mut line, &mut sent) {
                    if open && tx.send((out, t)).await.is_err() {
                        open = false;
                    }
                }
            }
        }
    }
    if !line.is_empty() {
        if let Some(t) = emit(&mut line, &mut sent) {
            if open {
                let _ = tx.send((out, t)).await;
            }
        }
    }
    p
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
    let (tx, mut rx) = tokio::sync::mpsc::channel::<(bool, String)>(64);
    let stdout = tokio::spawn(pump(
        child.stdout.take().unwrap(),
        true,
        keep_stdout,
        tx.clone(),
    ));
    let stderr = tokio::spawn(pump(child.stderr.take().unwrap(), false, false, tx));
    let mut ran = Ran {
        ok: false,
        code: None,
        json: None,
        tail: vec![],
        stdout: String::new(),
        stdout_over: false,
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
            }
            if !line.trim().is_empty() {
                on_line(&line);
                ran.tail.push(line);
                if ran.tail.len() > 20 {
                    ran.tail.remove(0);
                }
            }
        }
        let status = child.wait().await;
        if let Ok(p) = stdout.await {
            ran.stdout = String::from_utf8_lossy(&p.raw).into_owned();
            ran.stdout_over = p.raw_over;
        }
        let _ = stderr.await;
        status
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
    if ran.stdout_over {
        bail!(
            "{} printed more than {} MiB; a listing must be smaller",
            s.list[0],
            STDOUT_MAX >> 20
        );
    }
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

/// How a `use` ended: the model id to select, with a warning when the
/// command reported another one that was not taken, or why it failed.
///
/// A reported `model_id` is taken only when it is the requested name or
/// one of `known` (the names `list` offered): `use` output is not trusted,
/// and must not pick a model the user never chose from.
pub fn outcome(
    ok: bool,
    exit: Option<i32>,
    json: Option<&Value>,
    tail: &[String],
    requested: &str,
    known: &[String],
) -> Result<(String, Option<String>)> {
    let reported_fail = json.is_some_and(|j| j["ok"] == Value::Bool(false));
    if ok && !reported_fail {
        let reported = json.and_then(|j| j["model_id"].as_str());
        return Ok(match reported {
            Some(id) if id == requested => (id.to_string(), None),
            Some(id) if valid_name(id) && known.iter().any(|k| k == id) => (id.to_string(), None),
            Some(id) => (
                requested.to_string(),
                Some(format!(
                    "warning: the swap reported model {:?}, which the server did not list; selecting {requested}",
                    strip_controls(id).chars().take(200).collect::<String>()
                )),
            ),
            None => (requested.to_string(), None),
        });
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
    let (id, warning) = outcome(ran.ok, ran.code, ran.json.as_ref(), &ran.tail, model, known)?;
    if let Some(w) = warning {
        on_line(&w);
    }
    Ok(id)
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
            "qwen:7b",
            "qwen2.5-coder:32b-instruct-q4_K_M",
            "org/llama3:8b",
            "hf.co/org/model:Q4_K_M",
            "ab:c:d",
            "org/model",
            "m@v2",
            "org/m@main",
            "a+b_c",
            "A",
            "a.",
        ] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in [
            "", "-rf", ".hidden", "/abs", "a/../b", "a//b", "a/", "a b", "a;b", "$(x)", "`x`",
            "a'b", "a\"b", "a|b", "a&b", "a\nb", "é", "a*",
        ] {
            assert!(!valid_name(bad), "{bad:?}");
        }
        // Security review 8, finding 7: names shaped like remote paths or
        // URLs, which scp, rsync or git would reach out to.
        for bad in [
            "u@h:/etc/passwd",
            "u@h:x",
            "h@u:7b",
            "git@github.com:o/r",
            "a:b@c",
            "host:/etc/passwd",
            "h:/x",
            "org/m:/x",
            "https://evil",
            "file:/etc/passwd",
            "x:y",
            "a:b",
            "c:model",
            "x:",
            "Z:9",
            "ｑwen:7b",
            "qwen：7b",
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

    /// Security review 8, finding 8: bidi controls and zero-width
    /// characters survived; a progress line could read backwards. The
    /// probe's case first.
    #[test]
    fn format_characters_are_stripped() {
        assert_eq!(
            strip_controls("ok\u{202e}evil\u{200b}\u{9b}31m"),
            "okevil31m"
        );
        assert_eq!(
            strip_controls(
                "\u{2066}a\u{2067}b\u{2068}c\u{2069}d\u{202a}e\u{202b}f\u{202c}g\u{202d}h"
            ),
            "abcdefgh"
        );
        assert_eq!(
            strip_controls("z\u{200c}w\u{200d}j\u{2060}\u{feff}!"),
            "zwj!"
        );
        // Ordinary text, accents, CJK and emoji stay.
        assert_eq!(
            strip_controls("modèle ホ 😀\u{fe0f} ok"),
            "modèle ホ 😀\u{fe0f} ok"
        );
        // A name listed one per line with one hidden inside is listed by
        // what it shows.
        assert_eq!(
            parse_list("qwen\u{202e}:7b\nllama3:8b\n")
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>(),
            ["qwen:7b", "llama3:8b"]
        );
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
        let known = ["m".to_string(), "org/m".to_string()];
        let ok = |json: Option<serde_json::Value>| {
            outcome(true, Some(0), json.as_ref(), &[], "m", &known).unwrap()
        };
        assert_eq!(ok(None), ("m".into(), None));
        assert_eq!(
            ok(Some(json!({"ok": true, "model_id": "org/m"}))),
            ("org/m".into(), None)
        );
        assert_eq!(
            ok(Some(json!({"ok": true, "model_id": "m"}))),
            ("m".into(), None)
        );
        let (id, warn) = ok(Some(json!({"ok": true, "model_id": "bad id"})));
        assert_eq!(id, "m");
        assert!(warn.unwrap().contains("did not list"));
        let e = outcome(false, Some(1), Some(&json!({"ok": false, "error": "m does not fit", "shortfall_bytes": 3_460_000_000u64, "rolled_back": true})), &[], "m", &known)
            .unwrap_err()
            .to_string();
        assert_eq!(e, "m does not fit; 3460000000 bytes (3.46 GB) short; the server kept its previous model (exit code 1)");
        assert!(
            outcome(
                true,
                Some(0),
                Some(&json!({"ok": false, "error": "x"})),
                &[],
                "m",
                &known
            )
            .is_err(),
            "the report wins"
        );
        assert_eq!(
            outcome(false, Some(2), None, &["boom".into()], "m", &known)
                .unwrap_err()
                .to_string(),
            "boom (exit code 2)"
        );
    }

    /// Security review 8, finding 5: `use` reporting a model `list` never
    /// offered no longer selects it; the requested one is kept, with a
    /// warning. The probe's case first.
    #[test]
    fn use_cannot_choose_a_model_list_never_offered() {
        let (id, warn) = outcome(
            true,
            Some(0),
            Some(&json!({"model_id": "other-model"})),
            &[],
            "big",
            &[],
        )
        .unwrap();
        assert_eq!(id, "big");
        let warn = warn.unwrap();
        assert!(
            warn.contains("\"other-model\"") && warn.contains("selecting big"),
            "{warn}"
        );
        let known = ["big".to_string(), "small".to_string()];
        for (reported, want, warned) in [
            ("small", "small", false),
            ("big", "big", false),
            ("other-model", "big", true),
            ("Small", "big", true),
            ("smäll", "big", true),
            ("small\u{202e}", "big", true),
            ("", "big", true),
        ] {
            let (id, warn) = outcome(
                true,
                Some(0),
                Some(&json!({"model_id": reported})),
                &[],
                "big",
                &known,
            )
            .unwrap();
            assert_eq!(id, want, "{reported:?}");
            assert_eq!(warn.is_some(), warned, "{reported:?}");
            // The warning never carries control or format characters.
            if let Some(w) = warn {
                assert!(!w.contains('\u{202e}'), "{w:?}");
            }
        }
        // A model id that is not a string is no report at all.
        let (id, warn) = outcome(
            true,
            Some(0),
            Some(&json!({"model_id": 7})),
            &[],
            "big",
            &known,
        )
        .unwrap();
        assert_eq!((id.as_str(), warn), ("big", None));
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

    /// `n` bytes of `fill`, read in whatever sizes the reader asks for,
    /// never all held.
    struct Endless {
        left: u64,
        fill: &'static [u8],
        at: usize,
    }

    impl AsyncRead for Endless {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            let n = (buf.remaining() as u64).min(self.left) as usize;
            let mut put = 0;
            while put < n {
                let from = self.at % self.fill.len();
                let take = (self.fill.len() - from).min(n - put);
                buf.put_slice(&self.fill[from..from + take]);
                self.at += take;
                put += take;
            }
            self.left -= n as u64;
            std::task::Poll::Ready(Ok(()))
        }
    }

    fn endless(left: u64, fill: &'static [u8]) -> Endless {
        Endless { left, fill, at: 0 }
    }

    async fn pumped(r: Endless, keep_raw: bool) -> (Pumped, Vec<String>) {
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let got = tokio::spawn(async move {
            let mut v = Vec::new();
            while let Some((_, l)) = rx.recv().await {
                v.push(l);
            }
            v
        });
        let p = pump(r, true, keep_raw, tx).await;
        (p, got.await.unwrap())
    }

    /// Security review 8, finding 4: 300 MB with no newline used to be
    /// held whole as one line. Now at most one cut line is held.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_line_with_no_end_stays_bounded() {
        static A: [u8; 4096] = [b'a'; 4096];
        let (p, lines) = pumped(endless(300_000_000, &A), true).await;
        assert_eq!(p.read, 300_000_000, "read to the end");
        assert!(p.held_max <= 2 * LINE_BYTES, "held {}", p.held_max);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].len(), LINE_MAX);
        assert_eq!(p.raw.len(), STDOUT_MAX);
        assert!(p.raw_over);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_rest_of_a_long_line_is_dropped_up_to_its_newline() {
        static TEXT: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
        let text = TEXT.get_or_init(|| {
            let mut t = vec![b'x'; 50_000];
            t.extend_from_slice(b"\nnext line\n");
            // Three-byte characters, cut inside one at the byte limit.
            t.extend_from_slice("ホ".repeat(4000).as_bytes());
            t.extend_from_slice("\n\u{1F600}end".as_bytes());
            t
        });
        let (p, lines) = pumped(endless(text.len() as u64, text), false).await;
        assert_eq!(p.read, text.len() as u64);
        assert!(p.raw.is_empty() && !p.raw_over, "raw only when asked");
        assert_eq!(
            lines.len(),
            4,
            "{:?}",
            lines.iter().map(|l| l.len()).collect::<Vec<_>>()
        );
        assert_eq!(lines[0], "x".repeat(LINE_MAX));
        assert_eq!(lines[1], "next line");
        assert_eq!(lines[2], "ホ".repeat(LINE_MAX));
        assert_eq!(lines[3], "\u{1F600}end", "a last line without a newline");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn lines_past_the_output_cap_are_read_and_dropped() {
        static LINE: [u8; 1000] = {
            let mut l = [b'y'; 1000];
            l[999] = b'\n';
            l
        };
        let total = 3 * OUTPUT_MAX as u64;
        let (p, lines) = pumped(endless(total, &LINE), false).await;
        assert_eq!(p.read, total);
        let passed: usize = lines.iter().map(|l| l.len() + 1).sum();
        assert!(passed <= OUTPUT_MAX + 1000, "{passed}");
        assert!(passed >= OUTPUT_MAX - 1000, "{passed}");
    }

    fn list_of(script: &str) -> SwapConfig {
        SwapConfig {
            list: vec!["/bin/sh".into(), "-c".into(), script.into()],
            use_: vec!["/bin/echo".into(), PLACEHOLDER.into()],
            timeout: Some(30),
        }
    }

    /// The side bug: a JSON listing longer than a line's limit was cut and
    /// then read as model names. It is parsed from the whole stdout now.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_long_json_listing_parses() {
        let names: Vec<String> = (0..1500).map(|i| format!("org/model-{i:04}-ünï")).collect();
        let ascii: Vec<String> = (0..1500).map(|i| format!("org/model-{i:04}")).collect();
        let json = serde_json::to_string(&json!({
            "models": ascii.iter().map(|n| json!({"name": n, "live": n.ends_with("0007")})).collect::<Vec<_>>(),
            "configured": "org/model-0003",
            "note": names,
        }))
        .unwrap();
        assert!(json.len() > 4 * LINE_BYTES);
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("list.json");
        std::fs::write(&f, format!("Warning: banner\n{json}\n")).unwrap();
        let models = list(&list_of(&format!("cat '{}'", f.display())))
            .await
            .unwrap();
        assert_eq!(models.len(), 1500);
        assert_eq!(models[5].name, "org/model-0005");
        assert!(models[7].live && !models[6].live);
        assert!(models[3].configured);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_listing_over_a_mebibyte_is_refused() {
        let e = list(&list_of("head -c 2000000 /dev/zero | tr '\\0' a"))
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("more than 1 MiB"), "{e}");
        // Just under the limit, as names, still lists.
        let ok = list(&list_of(
            "i=0; while [ $i -lt 3000 ]; do echo model-$i; i=$((i+1)); done",
        ))
        .await
        .unwrap();
        assert_eq!(ok.len(), 3000);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_swap_reporting_an_unlisted_model_keeps_the_requested_one() {
        let s = SwapConfig {
            list: vec![],
            use_: vec![
                "/bin/sh".into(),
                "-c".into(),
                r#"echo "loading $1"; echo '{"ok":true,"model_id":"evil/model"}'"#.into(),
                "sh".into(),
                PLACEHOLDER.into(),
            ],
            timeout: Some(30),
        };
        let mut lines = Vec::new();
        let known = vec!["big".to_string(), "small".to_string()];
        let id = swap(&s, "big", &known, &mut |l| lines.push(l.to_string()))
            .await
            .unwrap();
        assert_eq!(id, "big");
        assert!(lines.iter().any(|l| l == "loading big"), "{lines:?}");
        assert!(
            lines.last().unwrap().starts_with("warning:")
                && lines.last().unwrap().contains("evil/model"),
            "{lines:?}"
        );
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
