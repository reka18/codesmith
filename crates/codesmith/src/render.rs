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
}

impl Printer {
    pub fn new(json: bool, thinking: bool) -> Self {
        Printer {
            json,
            thinking,
            in_reasoning: false,
            at_line_start: true,
        }
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
            title.map(|t| format!(" \"{t}\"")).unwrap_or_default()
        );
    }
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
        print!("{delta}");
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
        eprint!("{delta}");
    }

    fn tool_start(&mut self, id: &str, name: &str, input: &Value) {
        self.end_reasoning();
        if self.json {
            self.emit(json!({"type": "tool_call", "id": id, "name": name, "input": input}));
            return;
        }
        if !self.at_line_start {
            println!();
            self.at_line_start = true;
        }
        eprintln!(
            "\x1b[36m⚙ {name}\x1b[0m \x1b[2m{}\x1b[0m",
            describe(name, input)
        );
    }

    fn tool_end(&mut self, id: &str, name: &str, output: &str, is_error: bool) {
        if self.json {
            self.emit(json!({"type": "tool_result", "id": id, "name": name, "output": output, "is_error": is_error}));
            return;
        }
        let first = output.lines().next().unwrap_or("");
        if is_error {
            eprintln!("  \x1b[31m✗ {first}\x1b[0m");
        } else {
            eprintln!("  \x1b[2m→ {first}\x1b[0m");
        }
    }

    fn step_end(&mut self, reason: &FinishReason, usage: &Usage) {
        if self.json {
            self.emit(json!({"type": "step", "reason": reason.normalized, "finish": reason, "usage": usage_json(usage)}));
        }
    }

    fn retry(&mut self, attempt: u32, error: &str, wait: Duration) {
        if self.json {
            self.emit(json!({"type": "retry", "attempt": attempt, "error": error, "wait_ms": wait.as_millis() as u64}));
        } else {
            eprintln!(
                "\x1b[33mretry {attempt} in {:.1}s: {error}\x1b[0m",
                wait.as_secs_f32()
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
            eprintln!("  {style}{mark} {}\x1b[0m", t.content);
        }
    }

    fn notice(&mut self, message: &str) {
        if self.json {
            self.emit(json!({"type": "notice", "message": message}));
        } else {
            eprintln!("\x1b[33m{message}\x1b[0m");
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
    use super::describe;
    use serde_json::json;

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
