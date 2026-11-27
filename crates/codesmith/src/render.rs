//! Printing a run: streamed text on stdout, tool activity on stderr, or
//! one JSON object per event line with `--format json`.

use codesmith_ai::{FinishReason, Usage};
use codesmith_core::session::Sink;
use serde_json::{json, Value};
use std::io::Write;
use std::time::Duration;

pub struct Printer {
    json: bool,
    thinking: bool,
    in_reasoning: bool,
    at_line_start: bool,
    usage_log: Option<UsageLog>,
    /// `--output final`: hold each step's text and print only the final
    /// answer when the run ends.
    final_only: Option<FinalText>,
}

/// The text `--output final` holds: the current step's, and the last
/// step's that had any.
#[derive(Default)]
struct FinalText {
    step: String,
    last: String,
}

impl FinalText {
    /// The step ended: its text, when it has any, is the answer so far.
    fn end_step(&mut self) {
        let step = std::mem::take(&mut self.step);
        if !step.trim().is_empty() {
            self.last = step;
        }
    }
}

/// `codesmith run --usage-log PATH`: one JSON line per model step (its
/// tokens and any reported cost) and per tool call, appended as they
/// happen, so a program watching the run (a tailbus bridge's `[usage]`)
/// can count what it used while it runs. Lines are written whole, and a
/// failed write only stops the log, never the run.
pub struct UsageLog {
    file: Option<std::fs::File>,
    model: String,
    session: String,
    /// The estimated USD of a step (see [`Estimate`]).
    estimate: Option<Estimate>,
    /// What the gateway said about the step that ends next.
    served: Option<codesmith_ai::Served>,
    /// What the provider billed for the step that ends next.
    billed: Option<f64>,
}

/// Works out a step's estimated USD from its model, who served it and its
/// usage (see `Config::estimate`).
pub type Estimate = Box<dyn Fn(&str, Option<&codesmith_ai::Served>, &Usage) -> Option<f64> + Send>;

impl UsageLog {
    /// Open `path` for appending (created 0600), for a run of `session`
    /// on `model`.
    pub fn open(path: &std::path::Path, model: &str, session: &str) -> std::io::Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        Ok(UsageLog {
            file: Some(file),
            model: model.to_string(),
            session: session.to_string(),
            estimate: None,
            served: None,
            billed: None,
        })
    }

    /// Estimate each step's USD with `f`.
    pub fn with_estimate(mut self, f: Estimate) -> Self {
        self.estimate = Some(f);
        self
    }

    fn write(&mut self, v: Value) {
        let Some(f) = &mut self.file else {
            return;
        };
        let mut line = v.to_string();
        line.push('\n');
        if f.write_all(line.as_bytes())
            .and_then(|_| f.flush())
            .is_err()
        {
            self.file = None;
        }
    }

    /// A model step's line, with what was said about it since the last.
    pub fn step(&mut self, reason: &FinishReason, usage: &Usage) {
        let served = self.served.take();
        let billed = self.billed.take();
        let estimated = self
            .estimate
            .as_ref()
            .and_then(|f| f(&self.model, served.as_ref(), usage));
        let v = usage_line(
            &self.model,
            &self.session,
            reason,
            usage,
            &StepExtra {
                billed,
                estimated,
                served,
            },
        );
        self.write(v);
    }

    /// Who served the step that ends next.
    pub fn served(&mut self, s: &codesmith_ai::Served) {
        self.served = Some(s.clone());
    }

    /// What the provider billed for the step that ends next.
    pub fn billed(&mut self, b: Option<f64>) {
        self.billed = b;
    }

    /// The run's own line: what the gateway's budget for the key moved by
    /// over the run (see `run_total_line`).
    pub fn run_total(&mut self, source: &str, budget: &str, usd: f64) {
        let v = run_total_line(&self.session, &self.model, source, budget, usd);
        self.write(v);
    }

    /// A tool call's line: its name and, for file tools, the path.
    pub fn tool(&mut self, name: &str, input: &Value) {
        let v = tool_line(&self.session, name, input);
        self.write(v);
    }
}

/// What a step's line says beyond its usage.
#[derive(Debug, Clone, Default)]
pub struct StepExtra {
    /// The USD the provider reported it billed, as it reported it.
    pub billed: Option<f64>,
    /// The USD worked out from the token counts and the model's rates.
    pub estimated: Option<f64>,
    /// What the gateway said about the request.
    pub served: Option<codesmith_ai::Served>,
}

/// The usage log's line for one step. `usd` is only ever what the
/// provider billed (more than 0); `usd_estimated` is CodeSmith's estimate
/// from token counts and rates, and the two are never mixed.
pub fn usage_line(
    model: &str,
    session: &str,
    reason: &FinishReason,
    u: &Usage,
    extra: &StepExtra,
) -> Value {
    let mut v = json!({
        "type": "step",
        "session": session,
        "model": model,
        "reason": reason.normalized,
        "input": u.input,
        "output": u.output,
        "cache_read": u.cache_read,
        "cache_write": u.cache_write,
        "reasoning": u.reasoning,
        "time": chrono::Utc::now().to_rfc3339(),
    });
    if let Some(c) = extra.billed.filter(|c| c.is_finite() && *c > 0.0) {
        v["usd"] = json!(c);
    }
    if let Some(c) = extra.estimated.filter(|c| c.is_finite() && *c >= 0.0) {
        v["usd_estimated"] = json!(c);
    }
    if let Some(s) = &extra.served {
        if let Some(id) = &s.request_id {
            v["request_id"] = json!(id);
        }
        if let (Some(p), Some(m)) = (&s.provider, &s.model) {
            v["served_model"] = json!(format!("{p}/{m}"));
        }
    }
    v
}

/// The run's line from a gateway's budget: `usd_quota` is what the key's
/// budget moved by between the run's start and end, as the gateway
/// counts it. It is the whole key's spend over that time (concurrent
/// calls on the key included) and may lag, so it is never a step's cost.
pub fn run_total_line(session: &str, model: &str, source: &str, budget: &str, usd: f64) -> Value {
    json!({
        "type": "run",
        "session": session,
        "model": model,
        "source": source,
        "budget": budget,
        "usd_quota": usd,
        "time": chrono::Utc::now().to_rfc3339(),
    })
}

/// The usage log's line for one tool call. Only the path of a file tool's
/// input is kept, never its content or a command.
pub fn tool_line(session: &str, name: &str, input: &Value) -> Value {
    let mut v = json!({"type": "tool", "session": session, "name": name});
    if let Some(p) = input
        .get("filePath")
        .or_else(|| input.get("path"))
        .and_then(Value::as_str)
    {
        v["path"] = json!(p);
    }
    v
}

impl Printer {
    pub fn new(json: bool, thinking: bool) -> Self {
        Printer {
            json,
            thinking,
            in_reasoning: false,
            at_line_start: true,
            usage_log: None,
            final_only: None,
        }
    }

    /// Print only the final answer (`--output final`). It has no effect on
    /// `--format json`.
    pub fn final_only(mut self) -> Self {
        if !self.json {
            self.final_only = Some(FinalText::default());
        }
        self
    }

    /// Also write a [`UsageLog`].
    pub fn with_usage_log(mut self, log: UsageLog) -> Self {
        self.usage_log = Some(log);
        self
    }

    /// The usage log, if the run keeps one.
    pub fn usage_log(&mut self) -> Option<&mut UsageLog> {
        self.usage_log.as_mut()
    }

    fn emit(&self, v: Value) {
        println!("{v}");
    }

    fn end_reasoning(&mut self) {
        if self.in_reasoning {
            eprintln!("\x1b[0m");
            self.in_reasoning = false;
        }
    }

    pub fn done(
        &mut self,
        usage: &Usage,
        error: Option<String>,
        session: &str,
        title: Option<&str>,
    ) {
        self.end_reasoning();
        if self.json {
            self.emit(json!({"type": "done", "session": session, "title": title, "usage": usage_json(usage), "error": error}));
            return;
        }
        if let Some(mut f) = self.final_only.take() {
            f.end_step();
            if !f.last.is_empty() {
                print!("{}", safe(&f.last));
                self.at_line_start = f.last.ends_with('\n');
            }
        }
        if !self.at_line_start {
            println!();
        }
        eprintln!(
            "\x1b[2m{} in · {} out{} · {}{}\x1b[0m",
            usage.input,
            usage.output,
            if usage.cache_read > 0 {
                format!(" · {} cached", usage.cache_read)
            } else {
                String::new()
            },
            session,
            title
                .map(|t| format!(" \"{}\"", safe(t)))
                .unwrap_or_default()
        );
    }
}

/// Text from the model, a tool, a provider or an MCP server made safe to
/// print to a terminal: control characters (ESC, so OSC 52 clipboard
/// writes, OSC 8 links and title changes; C1 controls; `\r`) and the
/// characters that reorder or hide text are written as visible escapes.
/// Line breaks and tabs stay.
pub fn safe(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\n' | '\t' => out.push(c),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32))
            }
            c if c.is_control()
                || matches!(c as u32, 0x200B..=0x200F | 0x2028..=0x202E | 0x2060..=0x206F | 0xFEFF) =>
            {
                out.push_str(&format!("\\u{{{:x}}}", c as u32))
            }
            c => out.push(c),
        }
    }
    out
}

fn usage_json(u: &Usage) -> Value {
    json!({"input": u.input, "output": u.output, "cache_read": u.cache_read, "cache_write": u.cache_write})
}

/// A one-line summary of a tool call's input for the activity log.
pub fn describe(name: &str, input: &Value) -> String {
    let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("");
    // Tools inside a subagent arrive as "agent › tool".
    let name = name.rsplit(" › ").next().unwrap_or(name);
    match name {
        "subagent" => format!("{}: {}", s("agent"), s("description")),
        "webfetch" => s("url").to_string(),
        "websearch" => s("query").to_string(),
        "skill" => s("id").to_string(),
        "todowrite" => String::new(),
        "read" | "write" | "edit" => s("path").to_string(),
        "patch" | "apply_patch" => {
            let files = codesmith_core::tool::apply_patch::touched(
                std::path::Path::new(""),
                s("patchText"),
            );
            files
                .iter()
                .map(|(p, _)| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        }
        "glob" | "grep" => {
            let p = s("path");
            if p.is_empty() {
                s("pattern").to_string()
            } else {
                format!("{} in {p}", s("pattern"))
            }
        }
        "shell" => s("command").lines().next().unwrap_or("").to_string(),
        // Tools the provider runs itself.
        "web_search" | "web_search_preview" => s("query").to_string(),
        "web_fetch" => s("url").to_string(),
        "code_execution" | "code_interpreter" | "bash_code_execution" => {
            let code = [s("code"), s("command")]
                .into_iter()
                .find(|c| !c.is_empty());
            code.unwrap_or("").lines().next().unwrap_or("").to_string()
        }
        "google_search" => input["queries"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(", "),
        _ => input.to_string(),
    }
}

impl Sink for Printer {
    fn text(&mut self, delta: &str) {
        self.end_reasoning();
        if self.json {
            self.emit(json!({"type": "text", "text": delta}));
            return;
        }
        if let Some(f) = &mut self.final_only {
            f.step.push_str(delta);
            return;
        }
        print!("{}", safe(delta));
        let _ = std::io::stdout().flush();
        self.at_line_start = delta.ends_with('\n');
    }

    fn reasoning(&mut self, delta: &str) {
        if self.json {
            self.emit(json!({"type": "reasoning", "text": delta}));
            return;
        }
        if !self.thinking {
            return;
        }
        if !self.in_reasoning {
            eprint!("\x1b[2;3m");
            self.in_reasoning = true;
        }
        eprint!("{}", safe(delta));
    }

    fn tool_start(&mut self, id: &str, name: &str, input: &Value) {
        self.end_reasoning();
        if let Some(log) = &mut self.usage_log {
            log.tool(name, input);
        }
        if self.json {
            self.emit(json!({"type": "tool_call", "id": id, "name": name, "input": input}));
            return;
        }
        if !self.at_line_start {
            println!();
            self.at_line_start = true;
        }
        eprintln!(
            "\x1b[36m⚙ {}\x1b[0m \x1b[2m{}\x1b[0m",
            safe(name),
            safe(&describe(name, input))
        );
    }

    fn tool_end(&mut self, id: &str, name: &str, output: &str, is_error: bool) {
        if self.json {
            self.emit(json!({"type": "tool_result", "id": id, "name": name, "output": output, "is_error": is_error}));
            return;
        }
        let first = safe(output.lines().next().unwrap_or(""));
        if is_error {
            eprintln!("  \x1b[31m✗ {first}\x1b[0m");
        } else {
            eprintln!("  \x1b[2m→ {first}\x1b[0m");
        }
    }

    fn served(&mut self, s: &codesmith_ai::Served) {
        if let Some(log) = &mut self.usage_log {
            log.served(s);
        }
    }

    fn step_billed(&mut self, billed: Option<f64>) {
        if let Some(log) = &mut self.usage_log {
            log.billed(billed);
        }
    }

    fn step_end(&mut self, reason: &FinishReason, usage: &Usage) {
        if let Some(log) = &mut self.usage_log {
            log.step(reason, usage);
        }
        if let Some(f) = &mut self.final_only {
            f.end_step();
        }
        if self.json {
            self.emit(json!({"type": "step", "reason": reason.normalized, "finish": reason, "usage": usage_json(usage)}));
        }
    }

    fn retry(&mut self, attempt: u32, error: &str, wait: Duration) {
        // A retried step starts again: what it said so far is dropped.
        if let Some(f) = &mut self.final_only {
            f.step.clear();
        }
        if self.json {
            self.emit(json!({"type": "retry", "attempt": attempt, "error": error, "wait_ms": wait.as_millis() as u64}));
        } else {
            eprintln!(
                "\x1b[33mretry {attempt} in {:.1}s: {}\x1b[0m",
                wait.as_secs_f32(),
                safe(error)
            );
        }
    }

    fn todos(&mut self, todos: &[codesmith_core::tool::todo::Todo]) {
        use codesmith_core::tool::todo::Status;
        if self.json {
            self.emit(json!({"type": "todos", "todos": todos}));
            return;
        }
        for t in todos {
            let (mark, style) = match t.status {
                Status::Completed => ("✓", "\x1b[2m"),
                Status::InProgress => ("▶", "\x1b[1m"),
                Status::Pending => ("○", ""),
                Status::Cancelled => ("✗", "\x1b[2;9m"),
            };
            eprintln!("  {style}{mark} {}\x1b[0m", safe(&t.content));
        }
    }

    fn notice(&mut self, message: &str) {
        if self.json {
            self.emit(json!({"type": "notice", "message": message}));
        } else {
            eprintln!("\x1b[33m{}\x1b[0m", safe(message));
        }
    }

    fn compacted(&mut self, before: u64, after: u64) {
        if self.json {
            self.emit(json!({"type": "compacted", "before_tokens": before, "after_tokens": after}));
        } else {
            eprintln!("\x1b[2mcompacted context: ~{before} → ~{after} tokens\x1b[0m");
        }
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn final_text_keeps_the_last_step_with_text() {
        let mut f = super::FinalText::default();
        f.step.push_str("Let me look.");
        f.end_step();
        f.step.push_str("The answer.");
        f.end_step();
        assert_eq!(f.last, "The answer.");
        // A step with only whitespace leaves the answer as it was.
        f.step.push_str("  \n");
        f.end_step();
        assert_eq!(f.last, "The answer.");
    }

    #[test]
    fn a_retried_step_drops_what_it_said() {
        let mut p = super::Printer::new(false, false).final_only();
        use codesmith_core::session::Sink;
        p.text("half an ans");
        p.retry(1, "connection reset", std::time::Duration::from_millis(1));
        p.text("A whole answer.");
        let f = p.final_only.as_mut().unwrap();
        f.end_step();
        assert_eq!(f.last, "A whole answer.");
    }

    #[test]
    fn final_only_does_nothing_with_json() {
        assert!(super::Printer::new(true, false)
            .final_only()
            .final_only
            .is_none());
    }

    use super::{describe, safe, usage_line, StepExtra};
    use serde_json::json;

    fn step(u: &codesmith_ai::Usage, extra: StepExtra) -> serde_json::Value {
        let r = codesmith_ai::FinishReason::new(codesmith_ai::Reason::Stop, "stop");
        usage_line("bifrost/openai/m", "ses_1", &r, u, &extra)
    }

    // `usd` is only ever a billed cost above 0; a price CodeSmith worked
    // out (a context tier's, carried in the usage) never shows as `usd`.
    #[test]
    fn usd_is_billed_cost_only_and_the_estimate_is_its_own_field() {
        let priced = codesmith_ai::Usage {
            input: 10,
            cost: Some(1.5),
            ..Default::default()
        };
        let v = step(&priced, StepExtra::default());
        assert!(
            v.get("usd").is_none() && v.get("usd_estimated").is_none(),
            "{v}"
        );
        for billed in [Some(0.0), Some(-1.0), Some(f64::NAN), None] {
            let v = step(
                &priced,
                StepExtra {
                    billed,
                    ..Default::default()
                },
            );
            assert!(v.get("usd").is_none(), "{billed:?}: {v}");
        }
        let v = step(
            &priced,
            StepExtra {
                billed: Some(0.25),
                estimated: Some(0.2),
                served: Some(codesmith_ai::Served {
                    request_id: Some("req-1".into()),
                    provider: Some("openai".into()),
                    model: Some("m-mini".into()),
                }),
            },
        );
        assert_eq!(v["usd"], 0.25);
        assert_eq!(v["usd_estimated"], 0.2);
        assert_eq!(v["request_id"], "req-1");
        assert_eq!(v["served_model"], "openai/m-mini");
        // An estimate of 0 is still written; a broken one is not.
        let v = step(
            &priced,
            StepExtra {
                estimated: Some(0.0),
                ..Default::default()
            },
        );
        assert_eq!(v["usd_estimated"], 0.0);
        let v = step(
            &priced,
            StepExtra {
                estimated: Some(f64::INFINITY),
                ..Default::default()
            },
        );
        assert!(v.get("usd_estimated").is_none());
    }

    #[test]
    fn the_run_line_is_never_a_step() {
        let v = super::run_total_line("ses_1", "bifrost/openai/m", "bifrost-quota", "b1", 0.5);
        assert_eq!(v["type"], "run");
        assert_eq!(v["usd_quota"], 0.5);
        assert!(v.get("usd").is_none());
    }

    // OSC 52 (clipboard), OSC 8 (links), title changes, C1 controls and
    // bidi overrides in untrusted text are shown, not sent; line breaks
    // and tabs are kept.
    #[test]
    fn untrusted_text_cannot_drive_the_terminal() {
        assert_eq!(
            safe("a\x1b]52;c;ZXZpbA==\x07b\tc\nd"),
            "a\\x1b]52;c;ZXZpbA==\\x07b\tc\nd"
        );
        assert_eq!(
            safe("\x1b]8;;https://evil\x1b\\x\x1b]8;;\x1b\\"),
            "\\x1b]8;;https://evil\\x1b\\x\\x1b]8;;\\x1b\\"
        );
        assert_eq!(safe("\u{9b}31m\r\u{202e}"), "\\u{9b}31m\\r\\u{202e}");
        assert_eq!(safe("plain — ü 日本"), "plain — ü 日本");
    }

    #[test]
    fn tool_descriptions() {
        assert_eq!(describe("read", &json!({"path": "src/a.rs"})), "src/a.rs");
        assert_eq!(describe("grep", &json!({"pattern": "fn main"})), "fn main");
        assert_eq!(
            describe("glob", &json!({"pattern": "*.rs", "path": "src"})),
            "*.rs in src"
        );
        assert_eq!(
            describe("shell", &json!({"command": "cargo test\necho done"})),
            "cargo test"
        );
        assert_eq!(describe("other", &json!({"a": 1})), "{\"a\":1}");
        assert_eq!(
            describe("explore › glob", &json!({"pattern": "*.py"})),
            "*.py"
        );
        assert_eq!(
            describe(
                "subagent",
                &json!({"agent": "explore", "description": "Find files", "prompt": "long"})
            ),
            "explore: Find files"
        );
        assert_eq!(
            describe("webfetch", &json!({"url": "https://x.dev"})),
            "https://x.dev"
        );
    }
}
