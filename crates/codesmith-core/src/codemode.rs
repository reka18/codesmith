//! Code Mode, as in OpenCode: the model writes a short JavaScript program
//! that calls tools as `await tools.<namespace>.<tool>(input)` and composes
//! their results, all in one `execute` call. Programs run in Boa, a pure
//! Rust JavaScript engine with no filesystem, network or process access of
//! its own — the only way out is through the tools offered here, and every
//! call goes through CodeSmith's permissions like a direct one.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::sync::mpsc;

/// One tool available inside Code Mode.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// The tool's name as the session knows it (e.g. `github_create_issue`).
    pub tool: String,
    pub namespace: String,
    /// The name inside its namespace (e.g. `create_issue`).
    pub name: String,
    pub description: String,
    pub schema: Value,
}

pub const TOOL: &str = "execute";
pub const MAX_TOOL_CALLS: usize = 50;
pub const TIMEOUT: Duration = Duration::from_secs(180);
const LOOP_LIMIT: u64 = 5_000_000;
const RECURSION_LIMIT: usize = 400;

fn is_ident(s: &str) -> bool {
    let mut c = s.chars();
    c.next()
        .is_some_and(|f| f.is_ascii_alphabetic() || f == '_' || f == '$')
        && c.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '$')
}

/// `tools.ns.name` or `tools.ns["odd-name"]`.
pub fn path(e: &Entry) -> String {
    let part = |s: &str| {
        if is_ident(s) {
            format!(".{s}")
        } else {
            format!("[{}]", serde_json::to_string(s).unwrap())
        }
    };
    format!("tools{}{}", part(&e.namespace), part(&e.name))
}

/// A TypeScript-like type for a JSON Schema, kept short.
pub fn ts_type(s: &Value, depth: usize) -> String {
    if depth > 3 {
        return "any".into();
    }
    if let Some(e) = s["enum"].as_array() {
        return e
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(" | ");
    }
    if let Some(any) = s["anyOf"].as_array().or(s["oneOf"].as_array()) {
        return any
            .iter()
            .map(|v| ts_type(v, depth + 1))
            .collect::<Vec<_>>()
            .join(" | ");
    }
    match s["type"].as_str() {
        Some("string") => "string".into(),
        Some("integer") | Some("number") => "number".into(),
        Some("boolean") => "boolean".into(),
        Some("null") => "null".into(),
        Some("array") => format!("{}[]", ts_type(&s["items"], depth + 1)),
        Some("object") | None if s.get("properties").is_some() => {
            let req: Vec<&str> = s["required"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let props: Vec<String> = s["properties"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(k, v)| {
                    let key = if is_ident(k) {
                        k.clone()
                    } else {
                        serde_json::to_string(k).unwrap()
                    };
                    format!(
                        "{key}{}: {}",
                        if req.contains(&k.as_str()) { "" } else { "?" },
                        ts_type(v, depth + 1)
                    )
                })
                .collect();
            if props.is_empty() {
                "{}".into()
            } else {
                format!("{{ {} }}", props.join("; "))
            }
        }
        Some("object") => "object".into(),
        _ => "any".into(),
    }
}

/// The catalog section of the system prompt.
pub fn instructions(entries: &[Entry]) -> String {
    let mut by_ns: BTreeMap<&str, Vec<&Entry>> = BTreeMap::new();
    for e in entries {
        by_ns.entry(&e.namespace).or_default().push(e);
    }
    let mut s = String::from("# Code Mode\n\nThe tools listed below cannot be called directly: they only work as JavaScript inside the `execute` tool. Every other tool you have is a normal tool call: call it directly, never inside `execute`. The catalog is complete. Do not guess tool names.\n\n## Code Mode tools (call only inside `execute`)\n");
    for (ns, list) in by_ns {
        s.push_str(&format!("\n### {ns}\n"));
        for e in list {
            let desc = e.description.lines().next().unwrap_or("").trim();
            s.push_str(&format!(
                "- `{}(input: {})`{}\n",
                path(e),
                ts_type(&e.schema, 0),
                if desc.is_empty() {
                    String::new()
                } else {
                    format!(" — {desc}")
                }
            ));
        }
    }
    s
}

pub fn spec() -> (String, Value) {
    let description = [
        "Run JavaScript in a confined Code Mode runtime to script tool calls and compose their results.",
        "Write plain JavaScript in `code`: no code fence. Imports, direct filesystem or network access, and timers are unavailable; all external access goes through `tools`.",
        "Inside `execute`, the only callable tools are those listed in the Code Mode catalog. Call them through `tools` using the exact paths shown, preserving bracket notation such as `tools.<namespace>[\"tool-name\"](input)`.",
        "Prefer an explicit `return`; if omitted, the final top-level expression becomes the result. `console.log` output is returned too.",
        "Await every call whose result matters. Tool results are parsed as JSON when they are JSON, otherwise they are strings. A failed tool call throws an Error.",
    ]
    .join("\n");
    (
        description,
        json!({"type": "object", "properties": {"code": {"type": "string", "description": "The JavaScript program"}}, "required": ["code"]}),
    )
}

/// Make the final expression the result when there is no `return`.
pub fn program(code: &str) -> String {
    let code = code
        .trim()
        .trim_start_matches("```javascript")
        .trim_start_matches("```js")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let has_return = code
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .any(|w| w == "return");
    if has_return {
        return code.to_string();
    }
    let lines: Vec<&str> = code.lines().collect();
    let Some(i) = lines.iter().rposition(|l| !l.trim().is_empty()) else {
        return String::new();
    };
    let last = lines[i].trim().trim_end_matches(';');
    let starts_statement = [
        "const ",
        "let ",
        "var ",
        "if ",
        "if(",
        "for ",
        "for(",
        "while",
        "function ",
        "class ",
        "}",
        "//",
        "try",
    ]
    .iter()
    .any(|k| last.starts_with(k));
    if starts_statement || last.ends_with('{') || last.ends_with(',') {
        return code.to_string();
    }
    let mut out = lines[..i].join("\n");
    out.push_str(&format!("\nreturn ({last});"));
    out
}

fn prelude(entries: &[Entry]) -> String {
    let mut ns: BTreeMap<&str, Vec<&Entry>> = BTreeMap::new();
    for e in entries {
        ns.entry(&e.namespace).or_default().push(e);
    }
    let mut s = String::from("const tools = {};\n");
    for (n, list) in ns {
        let key = serde_json::to_string(n).unwrap();
        s.push_str(&format!("tools[{key}] = {{}};\n"));
        for e in list {
            s.push_str(&format!("tools[{key}][{}] = async (input) => __call({}, input === undefined ? {{}} : input);\n", serde_json::to_string(&e.name).unwrap(), serde_json::to_string(&e.tool).unwrap()));
        }
    }
    s.push_str("const __logs = [];\nconst console = { log: (...a) => { __logs.push(a.map(x => typeof x === 'string' ? x : JSON.stringify(x)).join(' ')); } };\nconsole.error = console.log; console.warn = console.log; console.info = console.log;\n");
    s
}

/// A tool call the program made, for the session to run.
pub struct Call {
    pub tool: String,
    pub input: Value,
    pub reply: std::sync::mpsc::Sender<Result<String, String>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub result: Result<Value, String>,
    pub logs: Vec<String>,
    pub calls: usize,
}

impl Outcome {
    /// What the model sees.
    pub fn render(&self) -> (String, bool) {
        let mut s = match &self.result {
            Ok(v) => format!(
                "Result:\n{}",
                serde_json::to_string_pretty(v).unwrap_or_default()
            ),
            Err(e) => format!("Error: {e}"),
        };
        if !self.logs.is_empty() {
            s.push_str(&format!("\n\nLogs:\n{}", self.logs.join("\n")));
        }
        s.push_str(&format!(
            "\n\n({} tool call{})",
            self.calls,
            if self.calls == 1 { "" } else { "s" }
        ));
        (s, self.result.is_err())
    }
}

fn run_js(
    code: String,
    entries: Vec<Entry>,
    calls: mpsc::UnboundedSender<Call>,
    max_calls: usize,
) -> Outcome {
    use boa_engine::builtins::promise::PromiseState;
    use boa_engine::object::builtins::JsPromise;
    use boa_engine::{js_string, Context, JsNativeError, JsValue, NativeFunction, Source};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let mut ctx = Context::default();
    ctx.runtime_limits_mut()
        .set_loop_iteration_limit(LOOP_LIMIT);
    ctx.runtime_limits_mut()
        .set_recursion_limit(RECURSION_LIMIT);
    let count = Arc::new(AtomicUsize::new(0));
    let c2 = count.clone();
    // SAFETY: the closure captures no garbage-collected values.
    let call = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let tool = args
                .first()
                .cloned()
                .unwrap_or_default()
                .to_string(ctx)?
                .to_std_string_escaped();
            let input = args
                .get(1)
                .cloned()
                .unwrap_or_default()
                .to_json(ctx)?
                .unwrap_or(json!({}));
            if c2.fetch_add(1, Ordering::SeqCst) >= max_calls {
                return Err(JsNativeError::error()
                    .with_message(format!(
                        "Code Mode allows at most {max_calls} tool calls per execution"
                    ))
                    .into());
            }
            let (tx, rx) = std::sync::mpsc::channel();
            if calls
                .send(Call {
                    tool: tool.clone(),
                    input,
                    reply: tx,
                })
                .is_err()
            {
                return Err(JsNativeError::error()
                    .with_message("the execution was stopped")
                    .into());
            }
            match rx.recv() {
                Ok(Ok(text)) => match serde_json::from_str::<Value>(&text) {
                    Ok(v) if v.is_object() || v.is_array() => JsValue::from_json(&v, ctx),
                    _ => Ok(JsValue::from(js_string!(text.as_str()))),
                },
                Ok(Err(e)) => Err(JsNativeError::error()
                    .with_message(format!("{tool}: {e}"))
                    .into()),
                Err(_) => Err(JsNativeError::error()
                    .with_message("the execution was stopped")
                    .into()),
            }
        })
    };
    let fail = |e: String, count: &Arc<AtomicUsize>| Outcome {
        result: Err(e),
        logs: vec![],
        calls: count.load(Ordering::SeqCst),
    };
    if let Err(e) = ctx.register_global_callable(js_string!("__call"), 2, call) {
        return fail(e.to_string(), &count);
    }
    if let Err(e) = ctx.eval(Source::from_bytes(prelude(&entries).as_bytes())) {
        return fail(format!("prelude: {e}"), &count);
    }
    let wrapped = format!("(async () => {{\n{}\n}})()", program(&code));
    let promise = match ctx.eval(Source::from_bytes(wrapped.as_bytes())) {
        Ok(v) => v,
        Err(e) => return fail(e.to_string(), &count),
    };
    if let Err(e) = ctx.run_jobs() {
        return fail(e.to_string(), &count);
    }
    let logs: Vec<String> = ctx
        .eval(Source::from_bytes("__logs"))
        .ok()
        .and_then(|v| v.to_json(&mut ctx).ok().flatten())
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    let result = match promise
        .as_object()
        .and_then(|o| JsPromise::from_object(o.clone()).ok())
        .map(|p| p.state())
    {
        Some(PromiseState::Fulfilled(v)) => v
            .to_json(&mut ctx)
            .map(|j| j.unwrap_or(Value::Null))
            .map_err(|e| e.to_string()),
        Some(PromiseState::Rejected(e)) => Err(e
            .to_string(&mut ctx)
            .map(|s| s.to_std_string_escaped())
            .unwrap_or_else(|_| "the program threw".into())),
        Some(PromiseState::Pending) => {
            Err("the program did not finish (a promise never settled)".into())
        }
        None => Err("the program did not produce a result".into()),
    };
    Outcome {
        result,
        logs,
        calls: count.load(Ordering::SeqCst),
    }
}

/// Run a program. `exec` runs each tool call (with permissions) and
/// returns its text; it is called on the current task, one call at a time.
pub async fn execute<F, Fut>(
    code: &str,
    entries: &[Entry],
    max_calls: usize,
    timeout: Duration,
    mut exec: F,
) -> Outcome
where
    F: FnMut(String, Value) -> Fut,
    Fut: std::future::Future<Output = Result<String, String>>,
{
    let (tx, mut rx) = mpsc::unbounded_channel::<Call>();
    let (done_tx, mut done_rx) = tokio::sync::oneshot::channel::<Outcome>();
    let (code, entries) = (code.to_string(), entries.to_vec());
    let spawned = std::thread::Builder::new()
        .name("codemode".into())
        .stack_size(32 * 1024 * 1024)
        .spawn(move || {
            let _ = done_tx.send(run_js(code, entries, tx, max_calls));
        });
    if let Err(e) = spawned {
        return Outcome {
            result: Err(format!("could not start Code Mode: {e}")),
            logs: vec![],
            calls: 0,
        };
    }
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            Some(c) = rx.recv() => {
                let r = exec(c.tool, c.input).await;
                let _ = c.reply.send(r);
            }
            out = &mut done_rx => {
                return out.unwrap_or(Outcome { result: Err("Code Mode stopped unexpectedly".into()), logs: vec![], calls: 0 });
            }
            _ = &mut deadline => {
                // Pending and later calls fail, so the program unwinds on its own.
                drop(rx);
                return Outcome { result: Err(format!("timed out after {}s", timeout.as_secs())), logs: vec![], calls: 0 };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries() -> Vec<Entry> {
        vec![
            Entry {
                tool: "gh_issues".into(),
                namespace: "gh".into(),
                name: "issues".into(),
                description: "List issues\nmore".into(),
                schema: json!({"type": "object", "properties": {"repo": {"type": "string"}, "state": {"enum": ["open", "closed"]}}, "required": ["repo"]}),
            },
            Entry {
                tool: "gh_get-issue".into(),
                namespace: "gh".into(),
                name: "get-issue".into(),
                description: String::new(),
                schema: json!({"type": "object", "properties": {"n": {"type": "integer"}}}),
            },
        ]
    }

    #[test]
    fn catalog_lines() {
        let s = instructions(&entries());
        assert!(s.contains("- `tools.gh.issues(input: { repo: string; state?: \"open\" | \"closed\" })` — List issues"), "{s}");
        assert!(
            s.contains("tools.gh[\"get-issue\"](input: { n?: number })"),
            "{s}"
        );
        assert_eq!(
            ts_type(&json!({"type": "array", "items": {"type": "string"}}), 0),
            "string[]"
        );
        assert_eq!(
            ts_type(&json!({"type": "object", "properties": {}}), 0),
            "{}"
        );
    }

    #[test]
    fn programs_get_a_result() {
        assert_eq!(
            program("const a = 1\na + 1"),
            "const a = 1\nreturn (a + 1);"
        );
        assert_eq!(program("return 5"), "return 5");
        assert_eq!(program("```js\n42\n```"), "\nreturn (42);");
        assert_eq!(
            program("const x = 1;"),
            "const x = 1;",
            "a declaration is not an expression"
        );
        assert_eq!(
            program("for (const x of y) {\n f(x)\n}"),
            "for (const x of y) {\n f(x)\n}"
        );
    }

    async fn run(code: &str) -> (Outcome, Vec<(String, Value)>) {
        let mut seen = Vec::new();
        let out = execute(
            code,
            &entries(),
            3,
            Duration::from_secs(20),
            |tool, input| {
                seen.push((tool.clone(), input.clone()));
                async move {
                    match tool.as_str() {
                        "gh_issues" => Ok(
                            json!([{"n": 1, "title": "bug"}, {"n": 2, "title": "feature"}])
                                .to_string(),
                        ),
                        "gh_get-issue" if input["n"] == 404 => Err("not found".into()),
                        _ => Ok(format!("issue {}", input["n"])),
                    }
                }
            },
        )
        .await;
        (out, seen)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tools_compose() {
        let (out, seen) = run("const list = await tools.gh.issues({repo: 'o/r'})\nconsole.log('got', list.length)\nconst titles = list.map(i => i.title)\nconst one = await tools.gh['get-issue']({n: list[0].n})\n({ titles, one })").await;
        assert_eq!(
            out.result,
            Ok(json!({"titles": ["bug", "feature"], "one": "issue 1"}))
        );
        assert_eq!(out.logs, ["got 2"]);
        assert_eq!(out.calls, 2);
        assert_eq!(seen[0], ("gh_issues".to_string(), json!({"repo": "o/r"})));
        let (text, err) = out.render();
        assert!(
            !err && text.contains("\"bug\"")
                && text.contains("Logs:\ngot 2")
                && text.ends_with("(2 tool calls)")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn errors_limits_and_sandbox() {
        let (out, _) = run("try { await tools.gh['get-issue']({n: 404}) } catch (e) { return 'caught: ' + e.message }").await;
        assert_eq!(out.result, Ok(json!("caught: gh_get-issue: not found")));
        let (out, _) = run("await tools.gh['get-issue']({n: 404})").await;
        assert!(out.result.unwrap_err().contains("not found"));
        let (out, _) =
            run("for (let i = 0; i < 10; i++) { await tools.gh.issues({repo: 'x'}) }").await;
        assert!(out.result.unwrap_err().contains("at most 3 tool calls"));
        let (out, _) = run("while (true) {}").await;
        assert!(out
            .result
            .unwrap_err()
            .contains("maximum number of iteration loops"));
        for code in [
            "require('fs')",
            "process.exit(1)",
            "fetch('http://x')",
            "setTimeout(() => 1, 10)",
        ] {
            let (out, seen) = run(code).await;
            assert!(
                out.result.is_err() && seen.is_empty(),
                "{code} must not work: {:?}",
                out.result
            );
        }
        let (out, _) = run("let x = (").await;
        assert!(out.result.unwrap_err().contains("SyntaxError"));
        let (out, _) = run("const r = await Promise.allSettled([tools.gh.issues({repo: 'a'}), tools.gh['get-issue']({n: 404})])\nr.map(x => x.status)").await;
        assert_eq!(out.result, Ok(json!(["fulfilled", "rejected"])));
    }
}
