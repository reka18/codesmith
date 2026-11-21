//! Code Mode, as in OpenCode: the model writes a short JavaScript program
//! that calls tools as `await tools.<namespace>.<tool>(input)` and composes
//! their results, all in one `execute` call. Programs run in Boa, a pure
//! Rust JavaScript engine with no filesystem, network or process access of
//! its own — the only way out is through the tools offered here, and every
//! call goes through CodeSmith's permissions like a direct one.
//!
//! A program can still burn CPU or memory, and Boa cannot be interrupted, so
//! when a helper is registered ([`set_helper`]; the `codesmith` binary
//! registers itself) each execution runs in a child process: CPU-time and
//! (on Linux) address-space rlimits, a resident-memory watchdog, and a kill
//! on timeout bound it, and tool calls travel over the child's stdin and
//! stdout as JSON lines. Without a helper (library users who do not call
//! [`child_main_if_requested`]) programs run on a thread in this process with
//! Boa's loop and recursion limits and the prelude's caps on buffer and
//! string sizes; a runaway program there keeps its thread busy after the
//! timeout until it ends on its own.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::mpsc;

pub mod fetch;
#[cfg(test)]
pub(crate) mod fixture;
pub mod openapi;
pub mod placement;
pub mod progress;
mod search;
pub mod signature;
mod web;

/// One tool available inside Code Mode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// The tool's name as the session knows it (e.g. `github_create_issue`).
    pub tool: String,
    pub namespace: String,
    /// The name inside its namespace (e.g. `create_issue`).
    pub name: String,
    pub description: String,
    pub schema: Value,
    /// The result's schema, when the tool declares one (OpenAPI operations
    /// do); the catalog shows `Promise<unknown>` without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    /// A freeform tool (OpenCode's `freeform`): when its input is one
    /// required string field, programs may pass that string directly
    /// (`tools.apply_patch("*** Begin Patch...")`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub freeform: bool,
}

pub const TOOL: &str = "execute";
/// A tool result that starts with this is JSON to hand the program as the
/// value it encodes, whatever it is (`null`, a number, a string): OpenAPI
/// operations return their decoded responses this way. Other results are
/// parsed only when they are a JSON object or array.
pub const JSON_RESULT: &str = "\u{1}codesmith-json\u{1}";
pub const MAX_TOOL_CALLS: usize = 50;
/// The `fetch` and `Response` globals (see [`fetch`]).
const FETCH: &str = include_str!("codemode/fetch.js");
pub const TIMEOUT: Duration = Duration::from_secs(180);
const LOOP_LIMIT: u64 = 5_000_000;
const RECURSION_LIMIT: usize = 400;
/// Resident memory a helper process may reach before it is stopped.
pub const MEMORY_LIMIT: u64 = 512 * 1024 * 1024;
/// The address-space rlimit for a helper on Linux (virtual memory, so
/// larger than [`MEMORY_LIMIT`]): allocations past it fail.
#[cfg(target_os = "linux")]
const ADDRESS_LIMIT: u64 = 2 * 1024 * 1024 * 1024;
/// A helper exits with this code when it passes [`MEMORY_LIMIT`].
const MEMORY_EXIT: i32 = 86;
/// The argument that starts a program as the Code Mode helper.
pub const CHILD_ARG: &str = "__codemode";

/// `tools.ns.name`, or `tools.ns["odd-name"]`; dots in the name nest it
/// (`tools.ns.issues.list`), as in OpenCode.
pub fn path(e: &Entry) -> String {
    search::expression(&search::canonical(e))
}

/// A JSON Schema as a compact TypeScript type (see [`signature`]).
pub fn ts_type(s: &Value) -> String {
    signature::typescript(s, false)
}

/// What the catalog shows for a tool, as OpenCode does: its call
/// expression, its input with JSDoc for each field's description and
/// constraints, and its result.
///
/// Rendering a schema is costly and the catalog is rebuilt every step, so
/// signatures are kept, keyed by the call expression and the schema.
pub fn signature(e: &Entry) -> String {
    use std::collections::HashMap;
    use std::hash::{BuildHasher, Hash, Hasher, RandomState};
    use std::sync::{LazyLock, Mutex};
    const KEEP: usize = 4096;
    static HASHER: LazyLock<RandomState> = LazyLock::new(RandomState::new);
    static MEMO: LazyLock<Mutex<HashMap<u64, String>>> = LazyLock::new(Default::default);
    let path = path(e);
    let mut h = HASHER.build_hasher();
    path.hash(&mut h);
    e.schema.to_string().hash(&mut h);
    if let Some(o) = &e.output {
        o.to_string().hash(&mut h);
    }
    e.freeform.hash(&mut h);
    let key = h.finish();
    if let Some(s) = MEMO.lock().ok().and_then(|m| m.get(&key).cloned()) {
        return s;
    }
    let s = match e
        .freeform
        .then(|| signature::string_parameter(&e.schema))
        .flatten()
    {
        Some(params) => format!(
            "{path}{params}: Promise<{}>",
            e.output
                .as_ref()
                .map_or_else(|| "unknown".to_string(), |o| signature::typescript(o, true))
        ),
        None => signature::signature(&path, &e.schema, e.output.as_ref()),
    };
    if let Ok(mut m) = MEMO.lock() {
        if m.len() >= KEEP {
            m.clear();
        }
        m.insert(key, s.clone());
    }
    s
}

/// The signature of the global `search(...)` a partial catalog points at.
pub fn search_signature() -> String {
    search::search_signature()
}

/// A catalog line's description: the first line, at most 500 characters.
fn summary(description: &str) -> String {
    let first = description.split('\n').next().unwrap_or("").trim();
    if first.chars().count() > 500 {
        format!("{}...", first.chars().take(497).collect::<String>())
    } else {
        first.to_string()
    }
}

/// Tokens the inline catalog may take (four characters a token), as in
/// OpenCode; past it the catalog is partial and points at `search(...)`.
const INLINE_BUDGET: usize = 2_000;

fn cost(text: &str) -> usize {
    (text.chars().count() as f64 / 4.0).round() as usize
}

fn namespace_line(name: &str, total: usize, shown: usize) -> String {
    let count = if total == 1 {
        "1 tool".to_string()
    } else {
        format!("{total} tools")
    };
    let label = if shown == total {
        count
    } else if shown == 0 {
        format!("{count}, none shown")
    } else {
        format!("{count}, {shown} shown")
    };
    format!("- {name} ({label})")
}

/// The catalog section of the system prompt, in OpenCode's form: each
/// namespace with its tool count, then each tool's signature and summary.
/// Every namespace is listed; tools are picked one per namespace per round,
/// in path order, while they fit [`INLINE_BUDGET`], and a partial catalog
/// tells the model to find the rest with `search(...)`.
pub fn instructions(entries: &[Entry]) -> String {
    if entries.is_empty() {
        return "No Code Mode tools are currently available. Later Code Mode catalog updates may add or remove tools. Do not call `execute` unless there is at least one available Code Mode tool.".into();
    }
    let mut by_ns: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for e in entries {
        let canonical = search::canonical(e);
        let top = canonical.split('.').next().unwrap_or("").to_string();
        let desc = summary(&e.description);
        let mut line = format!("  - {}", signature(e));
        if !desc.is_empty() {
            line.push_str(&format!(" // {desc}"));
        }
        by_ns.entry(top).or_default().push((canonical, line));
    }
    for list in by_ns.values_mut() {
        list.sort();
    }
    let mut remaining = INLINE_BUDGET as i64
        - by_ns
            .iter()
            .map(|(n, l)| cost(&namespace_line(n, l.len(), 0)) as i64)
            .sum::<i64>();
    let mut shown: Vec<usize> = vec![0; by_ns.len()];
    let lists: Vec<&Vec<(String, String)>> = by_ns.values().collect();
    let mut active: Vec<usize> = (0..lists.len()).collect();
    while !active.is_empty() {
        active.retain(|&i| {
            let Some((_, line)) = lists[i].get(shown[i]) else {
                return false;
            };
            if cost(line) as i64 > remaining {
                return false;
            }
            remaining -= cost(line) as i64;
            shown[i] += 1;
            shown[i] < lists[i].len()
        });
    }
    let complete = lists.iter().zip(&shown).all(|(l, n)| l.len() == *n);
    let mut s = String::from("# Code Mode\n\n");
    if complete {
        s.push_str("The tools listed below cannot be called directly: they only work as JavaScript inside the `execute` tool. Every other tool you have is a normal tool call: call it directly, never inside `execute`.\n\nThe catalog is complete. Do not guess tool names.");
    } else {
        s.push_str(&format!("The tools listed below, and `search`, cannot be called directly: they only work as JavaScript inside the `execute` tool. Every other tool you have is a normal tool call: call it directly, never inside `execute`.\n\nThe catalog is partial. Before calling a tool that is not listed below, find it with the global `search(...)` function. Do not guess tool names. Each result's `path` is the complete call expression: write it out as code, for example `await tools.ns.tool_name({{ ... }})`. Do not use it as a key such as `tools[path]`.\n\n- {}", search::search_signature()));
    }
    s.push_str("\n\n## Code Mode tools (call only inside `execute`)\n");
    for (i, (name, list)) in by_ns.iter().enumerate() {
        s.push_str(&format!("\n{}", namespace_line(name, list.len(), shown[i])));
        for (_, line) in &list[..shown[i]] {
            s.push_str(&format!("\n{line}"));
        }
    }
    s
}

pub fn spec() -> (String, Value) {
    let description = [
        "Run JavaScript in a confined Code Mode runtime to script tool calls and HTTP requests and compose their results.",
        "Write plain JavaScript in `code`: no code fence.",
        "`fetch` is available for HTTP requests. Imports, direct filesystem access, and timers are unavailable: there is no `setTimeout` or sleep. All other external access goes through `tools`.",
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
        "/*",
        "try",
        "async function",
        "throw ",
        "switch",
        "do ",
        "do{",
        "break",
        "continue",
        "else",
        "case ",
        "default:",
        "import ",
        "export ",
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

/// Size caps on what a program can allocate in one step, so the obvious
/// memory bombs fail with a RangeError instead of taking the process down
/// (the helper's memory watchdog covers the rest).
const CAPS: &str = r#"(() => {
  const MAX_BYTES = 64 * 1024 * 1024, MAX_CHARS = 16 * 1024 * 1024;
  const bytes = (n) => { if (Number(n) > MAX_BYTES) throw new RangeError('Code Mode limits buffers to 64 MiB'); };
  const chars = (n) => { if (Number(n) > MAX_CHARS) throw new RangeError('Code Mode limits strings to 16M characters'); };
  const guard = (C, check) => {
    const P = new Proxy(C, {
      construct(t, a, nt) { check(a); return Reflect.construct(t, a, nt === P ? t : nt); },
      apply(t, self, a) { check(a); return Reflect.apply(t, self, a); },
    });
    Object.defineProperty(C.prototype, 'constructor', { value: P, writable: true, configurable: true });
    return P;
  };
  const buffer = (a) => { bytes(a[0]); if (a[1] && typeof a[1] === 'object') bytes(a[1].maxByteLength); };
  globalThis.ArrayBuffer = guard(ArrayBuffer, buffer);
  if (typeof SharedArrayBuffer === 'function') globalThis.SharedArrayBuffer = guard(SharedArrayBuffer, buffer);
  for (const name of ['Int8Array', 'Uint8Array', 'Uint8ClampedArray', 'Int16Array', 'Uint16Array', 'Int32Array', 'Uint32Array', 'Float32Array', 'Float64Array', 'BigInt64Array', 'BigUint64Array', 'Float16Array']) {
    const T = globalThis[name];
    if (typeof T !== 'function') continue;
    globalThis[name] = guard(T, (a) => {
      const x = a[0];
      if (x === null || typeof x !== 'object') bytes(Number(x) * T.BYTES_PER_ELEMENT);
      else if (typeof x.length === 'number') bytes(x.length * T.BYTES_PER_ELEMENT);
    });
  }
  const repeat = String.prototype.repeat, padStart = String.prototype.padStart, padEnd = String.prototype.padEnd;
  String.prototype.repeat = function (n) { chars(String(this).length * Number(n)); return repeat.call(this, n); };
  String.prototype.padStart = function (n, f) { chars(n); return padStart.call(this, n, f); };
  String.prototype.padEnd = function (n, f) { chars(n); return padEnd.call(this, n, f); };
  const ArrayCtor = Array;
  const array = (a) => { if (a.length === 1 && typeof a[0] === 'number') bytes(a[0] * 8); };
  globalThis.Array = guard(ArrayCtor, array);
})();
"#;

/// `console`, formatting values as OpenCode's Code Mode does.
const CONSOLE: &str = include_str!("codemode/console.js");

/// What kind of object `console` is showing, for the kinds a program
/// cannot tell apart without running its own code: `"proxy"` (its traps
/// are not called), `"function"` (a function the program wrote, as
/// opposed to a built-in), `"error"` (an object made by an `Error`
/// constructor), `"promise"`, or `""`.
fn value_kind(
    _: &boa_engine::JsValue,
    args: &[boa_engine::JsValue],
    _: &mut boa_engine::Context,
) -> boa_engine::JsResult<boa_engine::JsValue> {
    use boa_engine::builtins::{
        error::Error, function::OrdinaryFunction, promise::Promise, proxy::Proxy,
    };
    let kind = match args.first().and_then(|v| v.as_object()) {
        Some(o) if o.is::<Proxy>() => "proxy",
        Some(o) if o.is::<OrdinaryFunction>() => "function",
        Some(o) if o.is::<Error>() => "error",
        Some(o) if o.is::<Promise>() => "promise",
        _ => "",
    };
    Ok(boa_engine::js_string!(kind).into())
}

fn prelude(entries: &[Entry]) -> String {
    let mut s = search::prelude(entries);
    s.push_str(CAPS);
    s.push_str(CONSOLE);
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
    /// What went wrong without failing the program: a promise rejected and
    /// never awaited or handled (OpenCode's unhandled rejection warning).
    pub warnings: Vec<String>,
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
        if !self.warnings.is_empty() {
            s.push_str("\n\nWarnings:");
            for w in &self.warnings {
                s.push_str("\n- ");
                s.push_str(w);
            }
        }
        s.push_str(&format!(
            "\n\n({} tool call{})",
            self.calls,
            if self.calls == 1 { "" } else { "s" }
        ));
        (s, self.result.is_err())
    }
}

/// Runs one tool call for the program: `None` when the execution was
/// stopped, otherwise the tool's text or error.
type Caller = Arc<dyn Fn(String, Value) -> Option<Result<String, String>> + Send + Sync>;

fn run_js(code: String, entries: Vec<Entry>, caller: Caller, max_calls: usize) -> Outcome {
    use boa_engine::builtins::promise::PromiseState;
    use boa_engine::object::builtins::JsPromise;
    use boa_engine::{js_string, JsNativeError, JsObject, JsValue, NativeFunction, Source};
    use std::rc::Rc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let rejections = Rc::new(Rejections::default());
    let mut ctx = web::context(rejections.clone());
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
                    .with_message(too_many_calls(max_calls))
                    .into());
            }
            match caller(tool.clone(), input) {
                Some(Ok(text)) if text.starts_with(JSON_RESULT) => {
                    match serde_json::from_str::<Value>(&text[JSON_RESULT.len()..]) {
                        Ok(v) => JsValue::from_json(&v, ctx),
                        Err(_) => Ok(JsValue::from(js_string!(text.as_str()))),
                    }
                }
                Some(Ok(text)) => match serde_json::from_str::<Value>(&text) {
                    Ok(v) if v.is_object() || v.is_array() => JsValue::from_json(&v, ctx),
                    _ => Ok(JsValue::from(js_string!(text.as_str()))),
                },
                Some(Err(e)) => Err(JsNativeError::error()
                    .with_message(format!("{tool}: {e}"))
                    .into()),
                None => Err(JsNativeError::error()
                    .with_message("the execution was stopped")
                    .into()),
            }
        })
    };
    let fail = |e: String, count: &Arc<AtomicUsize>| Outcome {
        result: Err(e),
        logs: vec![],
        warnings: vec![],
        calls: count.load(Ordering::SeqCst),
    };
    if let Err(e) = ctx.register_global_callable(js_string!("__call"), 2, call) {
        return fail(e.to_string(), &count);
    }
    if let Err(e) = ctx.register_global_callable(
        js_string!("__kind"),
        1,
        NativeFunction::from_fn_ptr(value_kind),
    ) {
        return fail(e.to_string(), &count);
    }
    if let Err(e) = search::install(&mut ctx, &entries) {
        return fail(format!("prelude: {e}"), &count);
    }
    if let Err(e) = ctx.eval(Source::from_bytes(prelude(&entries).as_bytes())) {
        return fail(format!("prelude: {e}"), &count);
    }
    if let Err(e) = web::install(&mut ctx) {
        return fail(format!("prelude: {e}"), &count);
    }
    if let Err(e) = ctx.eval(Source::from_bytes(FETCH.as_bytes())) {
        return fail(format!("prelude: {e}"), &count);
    }
    // Strict mode, as OpenCode's interpreter applies its checks (writes to
    // frozen or read-only properties throw, plain calls get `this`
    // undefined, assigning an undeclared name throws). OpenCode still
    // accepts repeated parameter names, which strict mode rejects as it
    // parses, before anything runs: such a program runs in sloppy mode, as
    // before.
    let body = program(&code);
    // A function rather than an arrow, so `this` at the top is undefined as
    // in OpenCode; the directive shares the first line so error positions
    // keep their line numbers.
    let strict = format!("(async function () {{ \"use strict\";\n{body}\n}})()");
    let promise = match ctx.eval(Source::from_bytes(strict.as_bytes())) {
        Ok(v) => v,
        Err(e) if e.to_string().contains("Duplicate parameter name") => {
            let sloppy = format!("(async () => {{\n{body}\n}})()");
            match ctx.eval(Source::from_bytes(sloppy.as_bytes())) {
                Ok(v) => v,
                Err(e) => return fail(e.to_string(), &count),
            }
        }
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
    // Promises rejected and still unhandled once every job ran, the
    // program's own promise aside (its rejection is the result).
    let main = promise.as_object();
    let unhandled: Vec<JsValue> = rejections
        .take()
        .into_iter()
        .filter(|p| !main.as_ref().is_some_and(|m| JsObject::equals(m, p)))
        .filter_map(|p| match JsPromise::from_object(p).ok()?.state() {
            PromiseState::Rejected(e) => Some(e),
            _ => None,
        })
        .collect();
    let warnings = bound_warnings(unhandled.into_iter().map(|e| {
        let reason = e
            .to_string(&mut ctx)
            .map(|s| s.to_std_string_escaped())
            .unwrap_or_else(|_| "an unprintable value".into());
        format!("Unhandled rejection from an un-awaited promise: {reason}")
    }));
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
        warnings,
        calls: count.load(Ordering::SeqCst),
    }
}

/// How many unhandled rejections a result lists, and how long each may be.
const MAX_WARNINGS: usize = 10;
const MAX_WARNING_CHARS: usize = 500;

/// At most [`MAX_WARNINGS`] warnings of at most [`MAX_WARNING_CHARS`]
/// characters each, control characters (but newlines) shown escaped, and
/// a count of the ones left out.
fn bound_warnings(all: impl Iterator<Item = String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut more = 0usize;
    for w in all {
        if out.len() == MAX_WARNINGS {
            more += 1;
            continue;
        }
        let mut t: String = w
            .chars()
            .flat_map(|c| match c {
                '\n' => vec!['\n'],
                c if c.is_control() => c.escape_default().collect(),
                c => vec![c],
            })
            .take(MAX_WARNING_CHARS + 1)
            .collect();
        if t.chars().count() > MAX_WARNING_CHARS {
            t = t.chars().take(MAX_WARNING_CHARS).collect();
            t.push('…');
        }
        out.push(t);
    }
    if more > 0 {
        out.push(format!(
            "… and {more} more unhandled rejection{}",
            if more == 1 { "" } else { "s" }
        ));
    }
    out
}

/// Promises rejected with no handler, as Boa reports them
/// (HostPromiseRejectionTracker): one rejected is added, and dropped again
/// once a handler is attached.
#[derive(Default)]
pub(crate) struct Rejections(std::cell::RefCell<Vec<boa_engine::JsObject>>);

impl Rejections {
    fn take(&self) -> Vec<boa_engine::JsObject> {
        std::mem::take(&mut *self.0.borrow_mut())
    }
}

impl boa_engine::context::HostHooks for Rejections {
    fn promise_rejection_tracker(
        &self,
        promise: &boa_engine::JsObject<boa_engine::builtins::promise::Promise>,
        operation: boa_engine::builtins::promise::OperationType,
        _context: &mut boa_engine::Context,
    ) {
        use boa_engine::builtins::promise::OperationType;
        let promise = promise.clone().upcast();
        let mut list = self.0.borrow_mut();
        match operation {
            OperationType::Reject => list.push(promise),
            OperationType::Handle => list.retain(|p| !boa_engine::JsObject::equals(p, &promise)),
        }
    }
}

fn timed_out(timeout: Duration, calls: usize) -> Outcome {
    Outcome {
        result: Err(format!("timed out after {}s", timeout.as_secs_f64())),
        logs: vec![],
        warnings: vec![],
        calls,
    }
}

fn too_many_calls(max_calls: usize) -> String {
    format!("Code Mode allows at most {max_calls} tool calls per execution")
}

fn failed(e: String) -> Outcome {
    Outcome {
        result: Err(e),
        logs: vec![],
        warnings: vec![],
        calls: 0,
    }
}

// ----- the helper process -----------------------------------------------------

/// How to start the Code Mode helper: a program that calls [`child_main`]
/// when started with these arguments and environment.
#[derive(Debug, Clone, PartialEq)]
pub struct Helper {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// The helper's whole environment (it starts with an empty one).
    pub env: Vec<(String, String)>,
}

static HELPER: RwLock<Option<Helper>> = RwLock::new(None);

/// Run programs in this helper process from now on (`None`: in-process).
pub fn set_helper(h: Option<Helper>) {
    *HELPER.write().unwrap() = h;
}

/// The registered helper, if any.
pub fn helper() -> Option<Helper> {
    HELPER.read().unwrap().clone()
}

/// For a binary's `main`: serve as the helper if started as one (this does
/// not return then), otherwise register this program as the helper.
pub fn child_main_if_requested() {
    if std::env::args_os().nth(1).is_some_and(|a| a == CHILD_ARG) {
        child_main();
    }
    if let Ok(program) = std::env::current_exe() {
        set_helper(Some(Helper {
            program,
            args: vec![CHILD_ARG.into()],
            env: vec![],
        }));
    }
}

/// The first line a helper reads: what to run and its limits.
#[derive(Serialize, Deserialize)]
struct Job {
    code: String,
    entries: Vec<Entry>,
    max_calls: usize,
    cpu_seconds: u64,
    memory: u64,
}

fn outcome_json(o: &Outcome) -> Value {
    json!({
        "done": true,
        "result": match &o.result { Ok(v) => json!({"ok": v}), Err(e) => json!({"err": e}) },
        "logs": o.logs,
        "warnings": o.warnings,
        "calls": o.calls,
    })
}

fn outcome_from(v: &Value) -> Outcome {
    let r = &v["result"];
    Outcome {
        result: match r.get("err").and_then(Value::as_str) {
            Some(e) => Err(e.to_string()),
            None => Ok(r.get("ok").cloned().unwrap_or(Value::Null)),
        },
        logs: serde_json::from_value(v["logs"].clone()).unwrap_or_default(),
        warnings: serde_json::from_value::<Vec<String>>(v["warnings"].clone())
            .map(|w| bound_warnings(w.into_iter()))
            .unwrap_or_default(),
        calls: v["calls"].as_u64().unwrap_or(0) as usize,
    }
}

#[cfg(unix)]
fn limit(resource: libc::c_int, soft: u64, hard: u64) {
    let r = libc::rlimit {
        rlim_cur: soft as libc::rlim_t,
        rlim_max: hard as libc::rlim_t,
    };
    // SAFETY: setrlimit only reads the struct.
    unsafe {
        libc::setrlimit(resource as _, &r);
    }
}

/// The most resident memory this process has used, in bytes.
#[cfg(unix)]
fn peak_rss() -> u64 {
    // SAFETY: getrusage fills the zeroed struct.
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe {
        libc::getrusage(libc::RUSAGE_SELF, &mut u);
    }
    let v = u.ru_maxrss as u64;
    if cfg!(target_os = "macos") {
        v
    } else {
        v * 1024
    }
}

/// Bound this helper: CPU time (the kernel stops it past the limit), the
/// address space on Linux, and a watchdog that exits once resident memory
/// passes `memory`.
fn apply_limits(cpu_seconds: u64, memory: u64) {
    #[cfg(unix)]
    {
        // SIGXCPU at the soft limit, SIGKILL a second later.
        limit(
            libc::RLIMIT_CPU as _,
            cpu_seconds.max(1),
            cpu_seconds.max(1) + 1,
        );
        #[cfg(target_os = "linux")]
        {
            let address = ADDRESS_LIMIT.max(memory * 2);
            limit(libc::RLIMIT_AS as _, address, address);
        }
        let _ = std::thread::Builder::new()
            .name("codemode-watchdog".into())
            .stack_size(64 * 1024)
            .spawn(move || loop {
                if peak_rss() > memory {
                    std::process::exit(MEMORY_EXIT);
                }
                std::thread::sleep(Duration::from_millis(5));
            });
    }
    #[cfg(not(unix))]
    let _ = (cpu_seconds, memory);
}

/// Serve one execution over stdin and stdout, then exit. A binary calls
/// this (usually through [`child_main_if_requested`]) when started as the
/// helper; it reads a job line, answers each tool call it writes with the
/// reply line it reads back, and ends with the outcome line.
pub fn child_main() -> ! {
    use std::io::{BufRead, Write};
    let stdin = std::io::stdin();
    let mut first = String::new();
    if stdin.lock().read_line(&mut first).is_err() {
        std::process::exit(2);
    }
    let Ok(job) = serde_json::from_str::<Job>(&first) else {
        std::process::exit(2);
    };
    apply_limits(job.cpu_seconds, job.memory);
    let io = Arc::new(std::sync::Mutex::new((stdin, std::io::stdout())));
    let line = |out: &mut std::io::Stdout, v: &Value| {
        let mut out = out.lock();
        writeln!(out, "{v}").and_then(|_| out.flush()).is_ok()
    };
    let io2 = io.clone();
    let caller: Caller = Arc::new(move |tool, input| {
        let mut g = io2.lock().ok()?;
        let (stdin, stdout) = &mut *g;
        if !line(stdout, &json!({"call": {"tool": tool, "input": input}})) {
            return None;
        }
        let mut reply = String::new();
        stdin.lock().read_line(&mut reply).ok().filter(|n| *n > 0)?;
        let v: Value = serde_json::from_str(&reply).ok()?;
        match (v["ok"].as_str(), v["err"].as_str()) {
            (Some(t), _) => Some(Ok(t.to_string())),
            (_, Some(e)) => Some(Err(e.to_string())),
            _ => None,
        }
    });
    let (code, entries, max_calls) = (job.code, job.entries, job.max_calls);
    let out = std::thread::Builder::new()
        .name("codemode".into())
        .stack_size(32 * 1024 * 1024)
        .spawn(move || run_js(code, entries, caller, max_calls))
        .map(|t| {
            t.join()
                .unwrap_or_else(|_| failed("Code Mode stopped unexpectedly".into()))
        })
        .unwrap_or_else(|e| failed(format!("could not start Code Mode: {e}")));
    let mut g = io.lock().unwrap_or_else(|e| e.into_inner());
    line(&mut g.1, &outcome_json(&out));
    std::process::exit(0)
}

/// Helper processes started, with their programs (for tests to find theirs).
#[cfg(test)]
static CHILDREN: std::sync::Mutex<Vec<(String, u32)>> = std::sync::Mutex::new(Vec::new());

/// Why a helper ended without an outcome.
fn exit_reason(status: std::io::Result<std::process::ExitStatus>, timeout: Duration) -> String {
    let Ok(status) = status else {
        return "Code Mode stopped unexpectedly".into();
    };
    if status.code() == Some(MEMORY_EXIT) {
        return format!(
            "the program used more than {} MiB of memory and was stopped",
            MEMORY_LIMIT / (1024 * 1024)
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        match status.signal() {
            Some(libc::SIGXCPU) | Some(libc::SIGKILL) => {
                return format!(
                    "the program used more than {}s of CPU time and was stopped",
                    timeout.as_secs().max(1)
                )
            }
            Some(libc::SIGABRT) | Some(libc::SIGSEGV) | Some(libc::SIGBUS) => {
                return "the program ran out of memory and was stopped".into()
            }
            _ => {}
        }
    }
    let _ = timeout;
    format!("Code Mode stopped unexpectedly ({status})")
}

async fn execute_in_child<F, Fut>(
    h: &Helper,
    code: &str,
    entries: &[Entry],
    max_calls: usize,
    timeout: Duration,
    exec: &mut F,
) -> Outcome
where
    F: FnMut(String, Value) -> Fut,
    Fut: std::future::Future<Output = Result<String, String>>,
{
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let mut child = match tokio::process::Command::new(&h.program)
        .args(&h.args)
        .env_clear()
        .envs(h.env.iter().cloned())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return failed(format!("could not start Code Mode: {e}")),
    };
    #[cfg(test)]
    {
        if let Some(pid) = child.id() {
            CHILDREN.lock().unwrap().push((code.to_string(), pid));
        }
    }
    let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return failed("could not start Code Mode: no pipes".into());
    };
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let job = Job {
        code: code.into(),
        entries: entries.to_vec(),
        max_calls,
        cpu_seconds: timeout.as_secs() + 1,
        memory: MEMORY_LIMIT,
    };
    let job = format!("{}\n", serde_json::to_string(&job).unwrap_or_default());
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    let mut calls = 0;
    if stdin.write_all(job.as_bytes()).await.is_err() {
        return failed(exit_reason(child.wait().await, timeout));
    }
    loop {
        let line = tokio::select! {
            l = lines.next_line() => l,
            _ = &mut deadline => {
                let _ = child.kill().await;
                return timed_out(timeout, calls);
            }
        };
        let Ok(Some(line)) = line else {
            // It ended without an outcome: a limit stopped it, or it crashed.
            let status = child.wait().await;
            return Outcome {
                calls,
                ..failed(exit_reason(status, timeout))
            };
        };
        // Anything else on stdout (a test harness's chatter) is not ours.
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if v["done"] == true {
            let _ = child.wait().await;
            return outcome_from(&v);
        }
        let Some(call) = v.get("call") else { continue };
        // The helper counts too, but it runs the program: enforce the
        // limit here as well, so a helper that doesn't gets stopped.
        if calls >= max_calls {
            let _ = child.kill().await;
            return Outcome {
                calls,
                ..failed(too_many_calls(max_calls))
            };
        }
        calls += 1;
        let (tool, input) = (
            call["tool"].as_str().unwrap_or_default().to_string(),
            call["input"].clone(),
        );
        let r = tokio::select! {
            r = exec(tool, input) => r,
            _ = &mut deadline => {
                let _ = child.kill().await;
                return timed_out(timeout, calls);
            }
        };
        let reply = match r {
            Ok(t) => json!({"ok": t}),
            Err(e) => json!({"err": e}),
        };
        if stdin
            .write_all(format!("{reply}\n").as_bytes())
            .await
            .is_err()
        {
            let status = child.wait().await;
            return Outcome {
                calls,
                ..failed(exit_reason(status, timeout))
            };
        }
    }
}

// ----- in this process -------------------------------------------------------

async fn execute_in_thread<F, Fut>(
    code: &str,
    entries: &[Entry],
    max_calls: usize,
    timeout: Duration,
    exec: &mut F,
) -> Outcome
where
    F: FnMut(String, Value) -> Fut,
    Fut: std::future::Future<Output = Result<String, String>>,
{
    let (tx, mut rx) = mpsc::unbounded_channel::<Call>();
    let (done_tx, mut done_rx) = tokio::sync::oneshot::channel::<Outcome>();
    let (code, entries) = (code.to_string(), entries.to_vec());
    let caller: Caller = Arc::new(move |tool, input| {
        let (reply, rx) = std::sync::mpsc::channel();
        tx.send(Call { tool, input, reply }).ok()?;
        rx.recv().ok()
    });
    let spawned = std::thread::Builder::new()
        .name("codemode".into())
        .stack_size(32 * 1024 * 1024)
        .spawn(move || {
            let _ = done_tx.send(run_js(code, entries, caller, max_calls));
        });
    if let Err(e) = spawned {
        return failed(format!("could not start Code Mode: {e}"));
    }
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    let mut calls = 0;
    loop {
        tokio::select! {
            Some(c) = rx.recv() => {
                // The program's thread counts too; this keeps the limit
                // here whatever runs the program.
                if calls >= max_calls {
                    let _ = c.reply.send(Err(too_many_calls(max_calls)));
                    continue;
                }
                calls += 1;
                let r = exec(c.tool, c.input).await;
                let _ = c.reply.send(r);
            }
            out = &mut done_rx => {
                return out.unwrap_or_else(|_| failed("Code Mode stopped unexpectedly".into()));
            }
            _ = &mut deadline => {
                // Pending and later calls fail, so the program unwinds on its
                // own if it makes one; a busy loop keeps its thread until the
                // loop limit ends it (the helper process has no such gap).
                drop(rx);
                return timed_out(timeout, calls);
            }
        }
    }
}

/// Run a program, in the helper process when one is registered (see
/// [`set_helper`]). `exec` runs each tool call (with permissions) and
/// returns its text; it is called on the current task, one call at a time.
pub async fn execute<F, Fut>(
    code: &str,
    entries: &[Entry],
    max_calls: usize,
    timeout: Duration,
    exec: F,
) -> Outcome
where
    F: FnMut(String, Value) -> Fut,
    Fut: std::future::Future<Output = Result<String, String>>,
{
    execute_with(helper().as_ref(), code, entries, max_calls, timeout, exec).await
}

/// [`execute`] with an explicit helper (`None`: in this process).
pub async fn execute_with<F, Fut>(
    helper: Option<&Helper>,
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
    // A freeform tool called with a string gets it as its one field.
    let fields: std::collections::HashMap<String, String> = entries
        .iter()
        .filter(|e| e.freeform)
        .filter_map(|e| Some((e.tool.clone(), codesmith_ai::freeform_field(&e.schema)?)))
        .collect();
    let mut exec = |tool: String, input: Value| {
        let input = match (input, fields.get(&tool)) {
            (Value::String(s), Some(f)) => {
                Value::Object(serde_json::Map::from_iter([(f.clone(), Value::String(s))]))
            }
            (input, _) => input,
        };
        exec(tool, input)
    };
    match helper {
        Some(h) => execute_in_child(h, code, entries, max_calls, timeout, &mut exec).await,
        None => execute_in_thread(code, entries, max_calls, timeout, &mut exec).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A helper's warnings survive the trip to the session, bounded again
    /// on the way in (a helper is not trusted to have bounded them), and
    /// an outcome from a helper that sends none has none.
    #[test]
    fn warnings_cross_from_the_helper_bounded() {
        let o = Outcome {
            result: Ok(Value::Null),
            logs: vec![],
            warnings: vec!["Unhandled rejection from an un-awaited promise: 1".into()],
            calls: 0,
        };
        assert_eq!(outcome_from(&outcome_json(&o)), o);
        let flood: Vec<String> = (0..50).map(|_| "y".repeat(10_000)).collect();
        let v = json!({"done": true, "result": {"ok": null}, "warnings": flood, "calls": 0});
        let back = outcome_from(&v).warnings;
        assert_eq!(back.len(), MAX_WARNINGS + 1);
        assert!(back[0].chars().count() <= MAX_WARNING_CHARS + 1);
        let old = json!({"done": true, "result": {"ok": 1}, "logs": [], "calls": 0});
        assert!(outcome_from(&old).warnings.is_empty());
        let bad = json!({"done": true, "result": {"ok": 1}, "warnings": [1, 2], "calls": 0});
        assert!(outcome_from(&bad).warnings.is_empty());
    }

    fn entries() -> Vec<Entry> {
        vec![
            Entry {
                tool: "gh_issues".into(),
                namespace: "gh".into(),
                name: "issues".into(),
                description: "List issues\nmore".into(),
                schema: json!({"type": "object", "properties": {"repo": {"type": "string"}, "state": {"enum": ["open", "closed"]}}, "required": ["repo"]}),
                output: None,
                freeform: false,
            },
            Entry {
                tool: "gh_get-issue".into(),
                namespace: "gh".into(),
                name: "get-issue".into(),
                description: String::new(),
                schema: json!({"type": "object", "properties": {"n": {"type": "integer"}}}),
                output: None,
                freeform: false,
            },
        ]
    }

    #[test]
    fn catalog_lines() {
        let s = instructions(&entries());
        assert!(
            s.ends_with(
                "## Code Mode tools (call only inside `execute`)\n\n- gh (2 tools)\n  - tools.gh[\"get-issue\"]({\n  /** @integer */\n  n?: number,\n}): Promise<unknown>\n  - tools.gh.issues({\n  repo: string,\n  state?: \"open\" | \"closed\",\n}): Promise<unknown> // List issues"
            ),
            "{s}"
        );
        assert!(s.contains("The catalog is complete."), "{s}");
        assert!(instructions(&[]).starts_with("No Code Mode tools are currently available."));
    }

    #[test]
    fn a_large_catalog_lists_every_namespace_and_points_at_search() {
        let many: Vec<Entry> = (0..200)
            .map(|i| Entry {
                tool: format!("t{i}"),
                namespace: if i < 100 { "big" } else { "other" }.into(),
                name: format!("tool_{i:03}"),
                description: "Does a thing with a fairly long description".into(),
                schema: json!({"type": "object", "properties": {"q": {"type": "string", "description": "What to look for"}}}),
                output: None,
                freeform: false,
            })
            .collect();
        let s = instructions(&many);
        assert!(s.contains("The catalog is partial."), "{s}");
        assert!(s.contains("\n- search(query: string | {\n  /** @integer @exclusiveMinimum 0 */\n  limit?: number,"), "{s}");
        assert!(s.contains("\n- big (100 tools, "), "{s}");
        assert!(s.contains("\n- other (100 tools, "), "{s}");
        assert!(
            s.contains("tools.big.tool_000({") && s.contains("tools.other.tool_100({"),
            "{s}"
        );
        assert!(!s.contains("tool_099"), "{s}");
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
        assert_eq!(
            program("throw new Error('x')"),
            "throw new Error('x')",
            "statements are not wrapped"
        );
        assert_eq!(program("if (a) b()\nelse c()"), "if (a) b()\nelse c()");
    }

    const CHILD_ENV: &str = "CODESMITH_CODEMODE_TEST_CHILD";

    /// The helper for tests: this test binary, running only `helper_process`.
    fn child() -> Helper {
        Helper {
            program: std::env::current_exe().unwrap(),
            args: [
                "codemode::tests::helper_process",
                "--exact",
                "--nocapture",
                "--test-threads=1",
                "-q",
            ]
            .map(String::from)
            .to_vec(),
            env: vec![(CHILD_ENV.into(), "1".into())],
        }
    }

    #[test]
    fn helper_process() {
        if std::env::var_os(CHILD_ENV).is_some() {
            child_main();
        }
    }

    /// Both ways of running: in a helper process and on a thread.
    fn modes() -> [Option<Helper>; 2] {
        [Some(child()), None]
    }

    async fn run(code: &str) -> (Outcome, Vec<(String, Value)>) {
        run_in(Some(&child()), code).await
    }

    async fn run_in(helper: Option<&Helper>, code: &str) -> (Outcome, Vec<(String, Value)>) {
        let mut seen = Vec::new();
        let out = execute_with(
            helper,
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
                        t if t == fetch::TOOL => Ok(fetch::result_text(&json!({
                            "status": 200, "statusText": "OK", "url": input["url"], "redirected": false,
                            "headers": [["content-type", "text/plain"]], "body": "aGk=",
                        }))),
                        _ => Ok(format!("issue {}", input["n"])),
                    }
                }
            },
        )
        .await;
        (out, seen)
    }

    /// A helper that ignores the call limit is stopped by the parent.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn the_parent_enforces_the_call_limit() {
        // Reads the job, then asks for a tool call after every reply, forever.
        let rogue = Helper {
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                r#"read job; while :; do echo '{"call":{"tool":"gh_issues","input":{}}}'; read reply || exit 0; done"#.into(),
            ],
            env: vec![],
        };
        let mut n = 0;
        let out = execute_with(
            Some(&rogue),
            "1",
            &entries(),
            3,
            Duration::from_secs(10),
            |_, _| {
                n += 1;
                async { Ok("[]".to_string()) }
            },
        )
        .await;
        assert_eq!(n, 3, "only the allowed calls ran");
        assert_eq!(out.calls, 3);
        assert_eq!(
            out.result,
            Err("Code Mode allows at most 3 tool calls per execution".into())
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tools_compose() {
        for h in modes() {
            tools_compose_in(h.as_ref()).await;
        }
    }

    async fn tools_compose_in(h: Option<&Helper>) {
        let (out, seen) = run_in(h, "const list = await tools.gh.issues({repo: 'o/r'})\nconsole.log('got', list.length)\nconst titles = list.map(i => i.title)\nconst one = await tools.gh['get-issue']({n: list[0].n})\n({ titles, one })").await;
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
        for h in modes() {
            errors_limits_and_sandbox_in(h.as_ref()).await;
        }
    }

    async fn errors_limits_and_sandbox_in(h: Option<&Helper>) {
        let run = |code: &'static str| run_in(h, code);
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
            "setTimeout(() => 1, 10)",
        ] {
            let (out, seen) = run(code).await;
            assert!(
                out.result.is_err() && seen.is_empty(),
                "{code} must not work: {:?}",
                out.result
            );
        }
        // `fetch` reaches the host as its own call, with the request encoded.
        let (out, seen) = run("const r = await fetch('http://x/a#top', {method: 'post', body: 'hi'})\nreturn [r.status, r.ok, r.headers.get('content-type'), await r.text()]").await;
        assert_eq!(out.result, Ok(json!([200, true, "text/plain", "hi"])));
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, fetch::TOOL);
        assert_eq!(seen[0].1["method"], "POST");
        assert_eq!(seen[0].1["url"], "http://x/a#top");
        assert_eq!(seen[0].1["body"], "aGk=");
        assert_eq!(
            seen[0].1["headers"],
            json!([["content-type", "text/plain;charset=UTF-8"]])
        );
        let (out, seen) = run("await fetch('http://x', {body: 'no'})").await;
        assert!(out.result.unwrap_err().contains("GET/HEAD"));
        assert!(seen.is_empty());
        let (out, _) = run("let x = (").await;
        assert!(out.result.unwrap_err().contains("SyntaxError"));
        let (out, _) = run("const r = await Promise.allSettled([tools.gh.issues({repo: 'a'}), tools.gh['get-issue']({n: 404})])\nr.map(x => x.status)").await;
        assert_eq!(out.result, Ok(json!(["fulfilled", "rejected"])));
    }

    /// Programs run in strict mode, as OpenCode applies its checks; one
    /// repeating a parameter name (which OpenCode accepts) runs sloppy.
    #[tokio::test(flavor = "multi_thread")]
    async fn programs_run_in_strict_mode() {
        let (out, _) = run_in(None, "const o = Object.freeze({a: 1})\ntry { o.a = 2; return 'wrote' } catch (e) { return e.name }").await;
        assert_eq!(out.result, Ok(json!("TypeError")));
        let (out, _) = run_in(
            None,
            "function f() { return typeof this }\nreturn [typeof this, f()]",
        )
        .await;
        assert_eq!(out.result, Ok(json!(["undefined", "undefined"])));
        let (out, _) = run_in(
            None,
            "try { undeclared = 1; return 'global' } catch (e) { return e.name }",
        )
        .await;
        assert_eq!(out.result, Ok(json!("ReferenceError")));
        let (out, _) = run_in(
            None,
            "function f(a, b, a) { return [a, b] }\nreturn f(1, 2, 3)",
        )
        .await;
        assert_eq!(out.result, Ok(json!([3, 2])));
        let (out, _) = run_in(None, "let x = (").await;
        assert!(out.result.unwrap_err().contains("SyntaxError"));
    }

    #[cfg(unix)]
    fn alive(pid: u32) -> bool {
        // SAFETY: signal 0 only checks that the process exists.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn runaway_programs_stop_at_the_timeout() {
        // Exponential recursion stays under the loop and recursion limits.
        let code =
            "function f(n) { return n < 2 ? n : f(n - 1) + f(n - 2) }\nreturn f(60) // runaway";
        let start = std::time::Instant::now();
        let out = execute_with(
            Some(&child()),
            code,
            &entries(),
            3,
            Duration::from_secs(1),
            |_, _| async { Ok(String::new()) },
        )
        .await;
        assert!(out.result.unwrap_err().contains("timed out"));
        assert!(start.elapsed() < Duration::from_secs(10));
        let pid = CHILDREN
            .lock()
            .unwrap()
            .iter()
            .find(|(c, _)| c == code)
            .map(|(_, p)| *p)
            .unwrap();
        assert!(!alive(pid), "the helper is gone after the timeout");
        // A timeout while a tool call is pending stops the helper too.
        let code = "// pending\nawait tools.gh.issues({repo: 'slow'})";
        let out = execute_with(
            Some(&child()),
            code,
            &entries(),
            3,
            Duration::from_secs(3),
            |_, _| async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(String::new())
            },
        )
        .await;
        assert_eq!(out.calls, 1);
        assert!(out.result.unwrap_err().contains("timed out"));
        let pid = CHILDREN
            .lock()
            .unwrap()
            .iter()
            .find(|(c, _)| c == code)
            .map(|(_, p)| *p)
            .unwrap();
        assert!(!alive(pid));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn big_allocations_fail_cleanly() {
        for h in modes() {
            for code in [
                "new ArrayBuffer(2 ** 30)",
                "new ArrayBuffer(8, { maxByteLength: 2 ** 31 })",
                "new Uint8Array(2 ** 30)",
                "new Float64Array(2 ** 25)",
                "new (new Uint8Array(1).constructor)(2 ** 30)",
                "'x'.repeat(2 ** 30)",
                "'x'.padEnd(2 ** 30)",
                "new Array(2 ** 30)",
            ] {
                let (out, _) = run_in(
                    h.as_ref(),
                    &format!("try {{ {code}; return 'allocated' }} catch (e) {{ return e.name + ': ' + e.message }}"),
                )
                .await;
                let r = out.result.unwrap();
                assert!(
                    r.as_str()
                        .unwrap()
                        .starts_with("RangeError: Code Mode limits"),
                    "{code}: {r}"
                );
            }
            // Ordinary use is untouched.
            let (out, _) = run_in(
                h.as_ref(),
                "class A extends Array {}\nconst b = new Uint8Array([1, 2, 3])\n[new ArrayBuffer(16).byteLength, b.length, new Uint8Array(1024).length, [1].constructor === Array, new A(3).length, Array.isArray(new A()), b.buffer instanceof ArrayBuffer, 'ab'.repeat(3), '7'.padStart(3, '0'), new Array(3).length, Array.from('ab')]",
            )
            .await;
            assert_eq!(
                out.result,
                Ok(json!([
                    16,
                    3,
                    1024,
                    true,
                    3,
                    true,
                    true,
                    "ababab",
                    "007",
                    3,
                    ["a", "b"]
                ]))
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn the_helper_is_stopped_past_its_memory_limit() {
        let (out, _) =
            run("const a = []\nlet s = 'x'.repeat(1 << 20)\nwhile (true) { s = s + s; a.push(s) }")
                .await;
        let e = out.result.unwrap_err();
        assert!(e.contains("memory"), "{e}");
    }

    async fn logs(code: &str) -> Vec<String> {
        let (out, _) = run_in(None, code).await;
        assert!(out.result.is_ok(), "{:?}", out.result);
        out.logs
    }

    /// codemode.test.ts: "captures console output as bounded result logs"
    /// (adapted: the logs; CodeSmith's result is its own).
    #[tokio::test(flavor = "multi_thread")]
    async fn oc_captures_console_output_with_method_prefixes() {
        let got = logs(
            r#"const returned = console.log("Thread info:", { name: "Demo", count: 2 })
            console.warn("careful")
            return returned"#,
        )
        .await;
        assert_eq!(
            got,
            [
                "Thread info: {\"name\":\"Demo\",\"count\":2}",
                "[warn] careful"
            ]
        );
        let got = logs(r#"console.error("bad", 1); console.debug("d"); console.info("i"); console.log(); return 0"#).await;
        assert_eq!(got, ["[error] bad 1", "[debug] d", "i", ""]);
    }

    /// codemode.test.ts: "captures console.dir and console.table output".
    #[tokio::test(flavor = "multi_thread")]
    async fn oc_captures_console_dir_and_console_table_output() {
        let got = logs(
            r#"console.dir({ nested: { ok: true } })
            console.table([
              { name: "Kit", count: 1, hidden: "x" },
              { name: "Olive", count: 2, hidden: "y" }
            ], ["name", "count"])
            return "done""#,
        )
        .await;
        assert_eq!(
            got,
            [
                "{\"nested\":{\"ok\":true}}",
                "(index)\tname\tcount\n0\tKit\t1\n1\tOlive\t2"
            ]
        );
    }

    /// codemode.test.ts: "console.table renders CodeMode value cells".
    #[tokio::test(flavor = "multi_thread")]
    async fn oc_console_table_renders_code_mode_value_cells() {
        let got = logs("console.table([{ when: new Date(0), n: NaN }])\nreturn null").await;
        assert_eq!(got, ["(index)\twhen\tn\n0\t1970-01-01T00:00:00.000Z\tNaN"]);
    }

    /// Tables of primitives, of an object's rows, and of no columns.
    #[tokio::test(flavor = "multi_thread")]
    async fn console_table_shapes() {
        let got = logs(
            r#"console.table([1, "two"]); console.table({ a: { x: 1 }, b: { y: 2 } }); console.table(5); console.table()
            return 0"#,
        )
        .await;
        assert_eq!(
            got,
            [
                "(index)\tValue\n0\t1\n1\ttwo",
                "(index)\tx\ty\na\t1\t\nb\t\t2",
                "(index)\tValue\n0\t5",
                "undefined",
            ]
        );
    }

    /// Showing a value runs none of the program's code: no getter, proxy
    /// trap or `toJSON` is called, and built-ins show what they are.
    #[tokio::test(flavor = "multi_thread")]
    async fn console_shows_values_without_running_program_code() {
        let got = logs(
            r#"const boom = () => { throw new Error("program code ran") }
            const p = new Proxy({}, { ownKeys: boom, getOwnPropertyDescriptor: boom, getPrototypeOf: boom })
            const a = { self: null, toJSON: boom }; a.self = a
            const arr = [1, , undefined]; Object.defineProperty(arr, 3, { get: boom, enumerable: true })
            console.log(p, a, arr, Object.create(p), { get x() { boom() } })
            console.log(new Set([1, "a"]), new WeakMap(), new WeakSet(), new Date(NaN), /a+/gi, Promise.resolve(1))
            console.log(new Uint8Array([1, 2]), new ArrayBuffer(4), Object.create(null), Symbol("s"), 10n, () => 1)
            console.log(Buffer.from("hi"), new URL("https://x.dev/a?b=1"), new URLSearchParams("a=1&b=2"))
            console.log(new (class M extends Map {})([[1, 2]]), new (class B { #x = 1; y = 2 })())
            class E2 extends TypeError {}
            console.log(new E2("x"), [new Error("in")], new Error(""))
            return 0"#,
        )
        .await;
        assert_eq!(
            got,
            [
                "[Proxy] {\"self\":[Circular],\"toJSON\":[opaque reference]} [1,null,null,[Getter]] {} {\"x\":[Getter]}",
                "Set(2) [1,\"a\"] WeakMap { <items unknown> } WeakSet { <items unknown> } Invalid Date /a+/gi [Promise (await it to get its value)]",
                "Uint8Array(2) [1,2] ArrayBuffer(4) {} Symbol(s) 10 [opaque reference]",
                "<Buffer 68 69> https://x.dev/a?b=1 a=1&b=2",
                "Map(1) [[1,2]] B {\"y\":2}",
                "TypeError: x [Error: in] Error",
            ]
        );
    }

    /// Nesting past 32 levels shows as `...`, and a line stops at its cap,
    /// so a value that fans out through shared references stays bounded.
    #[tokio::test(flavor = "multi_thread")]
    async fn console_output_is_bounded() {
        let got = logs(
            r#"let d = {}, x = d
            for (let i = 0; i < 40; i++) { x.n = {}; x = x.n }
            console.log(d)
            let big = ["x".repeat(100)]
            for (let i = 0; i < 30; i++) big = [big, big]
            console.log(big)
            return 0"#,
        )
        .await;
        assert_eq!(got[0].matches("{\"n\":").count(), 33);
        assert!(got[0].contains("{\"n\":...}"), "{}", got[0]);
        assert!(got[1].ends_with("... (cut)"));
        assert!(got[1].chars().count() <= 100_000 + 9, "{}", got[1].len());
    }
}
