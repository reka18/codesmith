//! Replays OpenCode's recorded provider traffic (`tests/fixtures/opencode`,
//! see the NOTICE there) through CodeSmith's protocol parsers.
//!
//! Every interaction is checked twice:
//! - against an *oracle* that reads the raw stream independently of the
//!   parsers, following OpenCode's semantics (non-cached input tokens, final
//!   usage wins, normalized finish reasons);
//! - against what OpenCode's own recorded tests assert for that cassette
//!   (`tests/fixtures/opencode-expectations.json` plus the golden scenarios).
//!
//! Mismatches CodeSmith accepts on purpose are listed in `GAPS`; a gap that
//! no longer triggers fails the run so the list stays honest.

use crate::openai::Utf8;
use crate::{anthropic, gemini, openai, responses, sse, Decoder, Error, Event, Usage};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/opencode");
const EXPECTATIONS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/opencode-expectations.json"
);

/// `(cassette, check, reason)`: known, deliberate differences from OpenCode.
/// `cassette` is the path under the fixtures dir without `.json`; a trailing
/// `*` matches a prefix. `check` is one of the check names used below.
const GAPS: &[(&str, &str, &str)] = &[
    // CodeSmith only declares JSON-schema function tools, never freeform
    // (`custom`) tools, so `custom_tool_call` items cannot occur for it.
    (
        "digitalocean-responses/calls-a-freeform-text-tool",
        "tools",
        FREEFORM,
    ),
    (
        "digitalocean-responses/calls-a-freeform-text-tool",
        "finish",
        FREEFORM,
    ),
    (
        "openrouter-responses/calls-a-freeform-text-tool",
        "tools",
        FREEFORM,
    ),
    (
        "openrouter-responses/calls-a-freeform-text-tool",
        "finish",
        FREEFORM,
    ),
    (
        "venice-responses/calls-a-freeform-text-tool",
        "tools",
        FREEFORM,
    ),
    (
        "venice-responses/calls-a-freeform-text-tool",
        "finish",
        FREEFORM,
    ),
    (
        "digitalocean-responses/calls-a-freeform-text-tool",
        "expect",
        FREEFORM,
    ),
    (
        "openrouter-responses/calls-a-freeform-text-tool",
        "expect",
        FREEFORM,
    ),
    (
        "venice-responses/calls-a-freeform-text-tool",
        "expect",
        FREEFORM,
    ),
    // Provider-executed (hosted) tools: CodeSmith never enables them and does
    // not surface their calls/results as events; only the text is replayed.
    (
        "alibaba-responses/qwen-3-8-max-uses-hosted-code-interpreter",
        "expect",
        HOSTED,
    ),
    (
        "alibaba-responses/qwen-3-8-max-uses-hosted-web-search-and-extraction",
        "expect",
        HOSTED,
    ),
    (
        "meta-search/searches-and-replays-grounded-messages",
        "expect",
        HOSTED,
    ),
    (
        "meta-search/searches-and-replays-grounded-responses",
        "expect",
        HOSTED,
    ),
    // OpenCode reports Anthropic `end_turn` as `stop` even when the turn holds
    // a tool call; CodeSmith (and this harness) count any tool call as a
    // tool-calls finish. The reason is informational only in CodeSmith.
    (
        "alibaba-messages/qwen-3-8-max-obeys-named-tool-choice",
        "expect",
        "end_turn with a tool call normalizes to tool-calls, not stop",
    ),
];

const HOSTED: &str = "hosted (provider-executed) tool calls are not surfaced as events";

const FREEFORM: &str = "freeform custom_tool_call items: CodeSmith declares no freeform tools";

#[derive(Debug, Clone, Copy, PartialEq)]
enum Proto {
    Chat,
    Anthropic,
    Responses,
    Gemini,
}

fn proto_of(url: &str) -> Option<Proto> {
    let path = url.split('?').next().unwrap_or("");
    if path.ends_with("/chat/completions") {
        Some(Proto::Chat)
    } else if path.ends_with("/messages") {
        Some(Proto::Anthropic)
    } else if path.ends_with("/responses") {
        Some(Proto::Responses)
    } else if path.to_lowercase().contains(":streamgeneratecontent") {
        Some(Proto::Gemini)
    } else {
        None
    }
}

fn decode_body(resp: &Value) -> String {
    let body = resp["body"].as_str().unwrap_or("");
    match resp["bodyEncoding"].as_str() {
        Some("base64") => String::from_utf8_lossy(&base64(body)).into_owned(),
        _ => body.to_string(),
    }
}

fn base64(s: &str) -> Vec<u8> {
    let val = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    };
    let (mut out, mut acc, mut bits) = (Vec::new(), 0u32, 0);
    for v in s.bytes().filter_map(val) {
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

/// SSE frames as the streaming loops see them: bytes arrive in small chunks,
/// go through the UTF-8 decoder and the frame parser, and a trailing frame
/// without a blank line is flushed at the end.
fn frames(body: &str) -> Vec<sse::Frame> {
    let mut parser = sse::Parser::default();
    let mut utf8 = Utf8::default();
    let mut out = Vec::new();
    for chunk in body.as_bytes().chunks(61) {
        out.extend(parser.push(&utf8.push(chunk)));
    }
    out.extend(parser.finish());
    out
}

/// Runs a body through the protocol's real decoder, frame by frame.
fn run(proto: Proto, body: &str) -> Result<Vec<Event>, Error> {
    fn go<D: Decoder>(mut d: D, frames: &[sse::Frame]) -> Result<Vec<Event>, Error> {
        let mut out = Vec::new();
        for f in frames {
            if d.done() {
                break;
            }
            out.extend(d.frame(f)?);
        }
        out.extend(d.end()?);
        Ok(out)
    }
    let frames = frames(body);
    match proto {
        Proto::Chat => go(openai::Decode::default(), &frames),
        Proto::Anthropic => go(anthropic::Decode::default(), &frames),
        Proto::Responses => go(responses::Decode::default(), &frames),
        Proto::Gemini => go(gemini::Decode::default(), &frames),
    }
}

/// What one model call produced, in comparable form.
#[derive(Debug, Default, Clone, PartialEq)]
struct Summary {
    text: String,
    reasoning: String,
    /// `(name, input)`; ids are checked separately (non-empty, unique).
    tools: Vec<(String, Value)>,
    finish: String,
    usage: Usage,
}

fn summarize(events: &[Event]) -> Result<Summary, String> {
    let mut s = Summary::default();
    let mut ids = BTreeSet::new();
    let mut finishes = 0;
    for (i, e) in events.iter().enumerate() {
        match e {
            Event::TextDelta(t) => s.text.push_str(t),
            Event::ReasoningDelta(t) => s.reasoning.push_str(t),
            Event::ToolCall { id, name, input } => {
                if id.is_empty() || !ids.insert(id.clone()) {
                    return Err(format!("tool call id {id:?} empty or repeated"));
                }
                s.tools.push((name.clone(), input.clone()));
            }
            Event::Finish { reason, usage } => {
                finishes += 1;
                if i + 1 != events.len() {
                    return Err("events after Finish".into());
                }
                s.finish = normalize(reason);
                s.usage = *usage;
            }
            _ => {}
        }
    }
    if finishes != 1 {
        return Err(format!("{finishes} Finish events"));
    }
    if s.finish == "stop" && !s.tools.is_empty() {
        s.finish = "tool-calls".into();
    }
    Ok(s)
}

/// OpenCode's normalized finish reasons.
fn normalize(raw: &str) -> String {
    match raw {
        "stop" | "end_turn" | "stop_sequence" | "pause_turn" | "STOP" => "stop",
        "tool_calls" | "tool_use" | "function_call" => "tool-calls",
        "length" | "max_tokens" | "max_output_tokens" | "MAX_TOKENS" => "length",
        "content_filter" | "refusal" | "safety" | "SAFETY" => "content-filter",
        other => other,
    }
    .to_string()
}

fn args(s: &str) -> Value {
    if s.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(s).unwrap_or_else(|_| json!({"__invalid_json": s}))
    }
}

fn n(v: &Value) -> Option<u64> {
    v.as_u64()
}

fn datas(body: &str) -> Vec<(Option<String>, Value)> {
    frames(body)
        .into_iter()
        .filter(|f| f.data.trim() != "[DONE]")
        .filter_map(|f| Some((f.event.clone(), serde_json::from_str(&f.data).ok()?)))
        .collect()
}

/// Independent reading of a raw stream, following OpenCode's semantics.
fn oracle(proto: Proto, body: &str) -> Summary {
    let mut s = Summary::default();
    let events = datas(body);
    match proto {
        Proto::Chat => {
            let mut calls: Vec<(String, String)> = Vec::new();
            let mut finish = None;
            let mut usage = None;
            for (_, v) in &events {
                for u in [
                    &v["usage"],
                    &v["choices"][0]["usage"],
                    &v["x_groq"]["usage"],
                ] {
                    if u.is_object() {
                        usage = Some(u.clone());
                    }
                }
                let Some(c) = v["choices"].get(0) else {
                    continue;
                };
                let d = &c["delta"];
                if let Some(t) = d["content"].as_str() {
                    s.text.push_str(t);
                }
                for p in d["content"].as_array().into_iter().flatten() {
                    match p["type"].as_str() {
                        Some("text") => s.text.push_str(p["text"].as_str().unwrap_or("")),
                        Some("thinking") => {
                            for t in p["thinking"].as_array().into_iter().flatten() {
                                s.reasoning.push_str(t["text"].as_str().unwrap_or(""));
                            }
                        }
                        _ => {}
                    }
                }
                for k in ["reasoning_content", "reasoning"] {
                    if let Some(t) = d[k].as_str() {
                        s.reasoning.push_str(t);
                    }
                }
                for tc in d["tool_calls"].as_array().into_iter().flatten() {
                    let i = n(&tc["index"]).unwrap_or(calls.len() as u64) as usize;
                    if calls.len() <= i {
                        calls.resize(i + 1, Default::default());
                    }
                    calls[i]
                        .0
                        .push_str(tc["function"]["name"].as_str().unwrap_or(""));
                    calls[i]
                        .1
                        .push_str(tc["function"]["arguments"].as_str().unwrap_or(""));
                }
                if let Some(f) = c["finish_reason"].as_str() {
                    finish = Some(f.to_string());
                }
            }
            s.tools = calls
                .into_iter()
                .filter(|c| !c.0.is_empty())
                .map(|(name, a)| (name, args(&a)))
                .collect();
            if let Some(u) = usage {
                let prompt = n(&u["prompt_tokens"]).unwrap_or(0);
                let cached = n(&u["prompt_tokens_details"]["cached_tokens"])
                    .or(n(&u["prompt_cache_hit_tokens"]))
                    .or(n(&u["cached_tokens"]))
                    .or(n(&u["cache_read_input_tokens"]))
                    .unwrap_or(0);
                let write = n(&u["prompt_tokens_details"]["cache_write_tokens"])
                    .or(n(&u["cache_created_input_tokens"]))
                    .unwrap_or(0);
                s.usage = Usage {
                    input: prompt.saturating_sub(cached + write),
                    output: n(&u["completion_tokens"]).unwrap_or(0),
                    cache_read: cached,
                    cache_write: write,
                };
            }
            s.finish = normalize(finish.as_deref().unwrap_or("stop"));
        }
        Proto::Anthropic => {
            let mut blocks: Vec<(String, String, String)> = Vec::new();
            let mut usage = serde_json::Map::new();
            let mut merge = |u: &Value| {
                for (k, v) in u.as_object().into_iter().flatten() {
                    if !v.is_null() {
                        usage.insert(k.clone(), v.clone());
                    }
                }
            };
            let mut stop = None;
            for (_, v) in &events {
                let i = n(&v["index"]).unwrap_or(0) as usize;
                match v["type"].as_str().unwrap_or("") {
                    "message_start" => merge(&v["message"]["usage"]),
                    "message_delta" => {
                        merge(&v["usage"]);
                        if let Some(r) = v["delta"]["stop_reason"].as_str() {
                            stop = Some(r.to_string());
                        }
                    }
                    "content_block_start" => {
                        if blocks.len() <= i {
                            blocks.resize(i + 1, Default::default());
                        }
                        let b = &v["content_block"];
                        blocks[i] = (
                            b["type"].as_str().unwrap_or("").into(),
                            b["name"].as_str().unwrap_or("").into(),
                            String::new(),
                        );
                        if let Some(t) = b["text"].as_str() {
                            s.text.push_str(t);
                        }
                    }
                    "content_block_delta" => {
                        let d = &v["delta"];
                        match d["type"].as_str().unwrap_or("") {
                            "text_delta" => s.text.push_str(d["text"].as_str().unwrap_or("")),
                            "thinking_delta" => {
                                s.reasoning.push_str(d["thinking"].as_str().unwrap_or(""))
                            }
                            "input_json_delta" => {
                                if let Some(b) = blocks.get_mut(i) {
                                    b.2.push_str(d["partial_json"].as_str().unwrap_or(""))
                                }
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
            s.tools = blocks
                .into_iter()
                .filter(|b| b.0 == "tool_use")
                .map(|(_, name, a)| (name, args(&a)))
                .collect();
            let u = Value::Object(usage);
            s.usage = Usage {
                input: n(&u["input_tokens"]).unwrap_or(0),
                output: n(&u["output_tokens"]).unwrap_or(0),
                cache_read: n(&u["cache_read_input_tokens"]).unwrap_or(0),
                cache_write: n(&u["cache_creation_input_tokens"]).unwrap_or(0),
            };
            s.finish = normalize(stop.as_deref().unwrap_or("end_turn"));
        }
        Proto::Responses => {
            let fin = events
                .iter()
                .map(|(_, v)| v)
                .find(|v| v["type"] == "response.completed" || v["type"] == "response.incomplete")
                .cloned()
                .unwrap_or(Value::Null);
            let r = &fin["response"];
            // Items as streamed; the final `output` echoes them when present.
            let mut items: Vec<Value> = events
                .iter()
                .filter(|(_, v)| v["type"] == "response.output_item.done")
                .map(|(_, v)| v["item"].clone())
                .collect();
            if let Some(out) = r["output"].as_array().filter(|o| !o.is_empty()) {
                items = out.clone();
            }
            for it in &items {
                match it["type"].as_str().unwrap_or("") {
                    "message" => {
                        for c in it["content"].as_array().into_iter().flatten() {
                            s.text.push_str(c["text"].as_str().unwrap_or(""));
                        }
                    }
                    "reasoning" => {
                        for k in ["summary", "content"] {
                            for c in it[k].as_array().into_iter().flatten() {
                                s.reasoning.push_str(c["text"].as_str().unwrap_or(""));
                            }
                        }
                    }
                    "function_call" => s.tools.push((
                        it["name"].as_str().unwrap_or("").into(),
                        args(it["arguments"].as_str().unwrap_or("")),
                    )),
                    "custom_tool_call" => s.tools.push((
                        it["name"].as_str().unwrap_or("").into(),
                        json!(it["input"].as_str().unwrap_or("")),
                    )),
                    _ => {}
                }
            }
            if items.iter().all(|i| i["type"] != "message") {
                for (_, v) in &events {
                    if v["type"] == "response.output_text.delta" {
                        s.text.push_str(v["delta"].as_str().unwrap_or(""));
                    }
                }
            }
            let u = &r["usage"];
            // OpenCode: non-cached input excludes cache reads and writes
            // (openai-responses.test.ts "parses text and usage stream fixtures").
            let cached = n(&u["input_tokens_details"]["cached_tokens"]).unwrap_or(0);
            let written = n(&u["input_tokens_details"]["cache_write_tokens"]).unwrap_or(0);
            s.usage = Usage {
                input: n(&u["input_tokens"])
                    .unwrap_or(0)
                    .saturating_sub(cached + written),
                output: n(&u["output_tokens"]).unwrap_or(0),
                cache_read: cached,
                cache_write: written,
            };
            s.finish = if !s.tools.is_empty() {
                "tool-calls".into()
            } else {
                normalize(r["incomplete_details"]["reason"].as_str().unwrap_or("stop"))
            };
        }
        Proto::Gemini => {
            let mut finish = "STOP".to_string();
            for (_, v) in &events {
                if let Some(u) = v.get("usageMetadata") {
                    let cached = n(&u["cachedContentTokenCount"]).unwrap_or(0);
                    s.usage = Usage {
                        input: n(&u["promptTokenCount"])
                            .unwrap_or(0)
                            .saturating_sub(cached),
                        output: n(&u["candidatesTokenCount"]).unwrap_or(0)
                            + n(&u["thoughtsTokenCount"]).unwrap_or(0),
                        cache_read: cached,
                        cache_write: 0,
                    };
                }
                let c = &v["candidates"][0];
                for p in c["content"]["parts"].as_array().into_iter().flatten() {
                    if let Some(f) = p.get("functionCall") {
                        s.tools.push((
                            f["name"].as_str().unwrap_or("").into(),
                            f.get("args").cloned().unwrap_or(json!({})),
                        ));
                    } else if let Some(t) = p["text"].as_str() {
                        if p["thought"] == true {
                            s.reasoning.push_str(t);
                        } else {
                            s.text.push_str(t);
                        }
                    }
                }
                if let Some(f) = c["finishReason"].as_str() {
                    finish = f.to_string();
                }
            }
            s.finish = normalize(&finish);
        }
    }
    if s.finish == "stop" && !s.tools.is_empty() {
        s.finish = "tool-calls".into();
    }
    s
}

struct Cassette {
    name: String,
    tags: Vec<String>,
    interactions: Vec<Value>,
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            walk(&p, out);
        } else if p.extension().is_some_and(|x| x == "json") {
            out.push(p);
        }
    }
}

fn cassettes() -> Vec<Cassette> {
    let mut paths = Vec::new();
    walk(Path::new(FIXTURES), &mut paths);
    paths.sort();
    paths
        .iter()
        .map(|p| {
            let v: Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
            let name = p
                .strip_prefix(FIXTURES)
                .unwrap()
                .with_extension("")
                .to_string_lossy()
                .trim_start_matches('/')
                .to_string();
            Cassette {
                name,
                tags: v["metadata"]["tags"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|t| t.as_str().map(str::to_string))
                    .collect(),
                interactions: v["interactions"].as_array().cloned().unwrap_or_default(),
            }
        })
        .collect()
}

/// Collects failures, swallowing the ones listed in `GAPS`.
struct Report {
    failures: Vec<String>,
    used: BTreeSet<usize>,
}

impl Report {
    fn check(&mut self, cassette: &str, call: usize, check: &str, ok: bool, detail: String) {
        if ok {
            return;
        }
        let gap = GAPS.iter().position(|(c, k, _)| {
            *k == check
                && match c.strip_suffix('*') {
                    Some(prefix) => cassette.starts_with(prefix),
                    None => *c == cassette,
                }
        });
        match gap {
            Some(i) => {
                self.used.insert(i);
            }
            None => self
                .failures
                .push(format!("{cassette} #{call} [{check}] {detail}")),
        }
    }
}

/// OpenCode's tests compare trimmed, lowercased text with thousands
/// separators removed; an `equals` that is JSON compares as JSON.
fn text_matches(actual: &str, spec: &Value) -> bool {
    let norm = |s: &str| s.trim().to_lowercase().replace(',', "");
    let a = norm(actual);
    if let Some(e) = spec["equals"].as_str() {
        if let (Ok(x), Ok(y)) = (
            serde_json::from_str::<Value>(actual.trim()),
            serde_json::from_str::<Value>(e),
        ) {
            if x.is_object() || x.is_array() {
                return x == y;
            }
        }
        return a == norm(e);
    }
    if let Some(c) = spec["contains"].as_str() {
        return a.contains(&norm(c));
    }
    if let Some(re) = spec["matches"].as_str() {
        return simple_regex(&a, &re.to_lowercase());
    }
    true
}

/// The regex shapes the OpenCode tests use: optional `^`/`$` anchors around
/// literals, `\x` escapes, `(?:a|b)` groups, `[ab]` classes, a `?` after any
/// of those, and `.`/`.+` (anything non-empty). Matching expands every
/// alternative literal.
fn simple_regex(actual: &str, re: &str) -> bool {
    if matches!(re, "." | ".+") {
        return !actual.is_empty();
    }
    let start = re.starts_with('^');
    let end = re.ends_with('$') && !re.ends_with("\\$");
    let body = re.trim_start_matches('^');
    let body = if end { &body[..body.len() - 1] } else { body };
    let chars: Vec<char> = body.chars().collect();
    let mut alts = vec![String::new()];
    let mut i = 0;
    while i < chars.len() {
        let options: Vec<String> = match chars[i] {
            '\\' => {
                i += 1;
                vec![chars[i].to_string()]
            }
            '(' => {
                let close = (i..chars.len()).find(|&j| chars[j] == ')').unwrap();
                let inner: String = chars[i + 1..close].iter().collect();
                i = close;
                inner
                    .trim_start_matches("?:")
                    .split('|')
                    .map(str::to_string)
                    .collect()
            }
            '[' => {
                let close = (i..chars.len()).find(|&j| chars[j] == ']').unwrap();
                let set = chars[i + 1..close].iter().map(|c| c.to_string()).collect();
                i = close;
                set
            }
            c => vec![c.to_string()],
        };
        i += 1;
        let mut options = options;
        if chars.get(i) == Some(&'?') {
            options.push(String::new());
            i += 1;
        }
        alts = alts
            .iter()
            .flat_map(|a| options.iter().map(move |o| format!("{a}{o}")))
            .collect();
    }
    alts.iter().any(|lit| match (start, end) {
        (true, true) => actual == lit,
        (true, false) => actual.starts_with(lit.as_str()),
        (false, true) => actual.ends_with(lit.as_str()),
        (false, false) => actual.contains(lit.as_str()),
    })
}

fn normalize_words(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// What OpenCode's golden scenarios assert, by call index.
fn golden(c: &Cassette, call: usize, s: &Summary) -> Option<Result<(), String>> {
    if !c.tags.iter().any(|t| t == "golden") {
        return None;
    }
    let has = |t: &str| c.tags.iter().any(|x| x == t);
    let weather = |s: &Summary| {
        s.tools == [("get_weather".to_string(), json!({"city": "Paris"}))]
            && s.finish == "tool-calls"
    };
    let text = |s: &Summary, re: &str| simple_regex(s.text.trim(), re) && s.finish == "stop";
    let ok = if has("tool-loop") {
        if call == 0 {
            weather(s)
        } else {
            text(s, r"^Paris is sunny\.?$")
        }
    } else if has("tool-call") {
        weather(s)
    } else if has("tool-result") {
        normalize_words(&s.text) == "jiggling restroom prison" && s.finish == "stop"
    } else if has("image") {
        normalize_words(&s.text) == "jiggling restroom prison"
    } else if has("continuation") {
        text(s, if call == 0 { "^Hello!?$" } else { r"^Done\.?$" })
    } else if has("text") || has("reasoning") {
        text(s, "^Hello!?$")
    } else {
        return None;
    };
    Some(if ok {
        Ok(())
    } else {
        Err(format!("golden scenario not met: {s:?}"))
    })
}

/// `reasoningTokens` assertions are skipped: CodeSmith's `Usage` has no
/// reasoning breakdown (reasoning is counted inside `output`).
fn expected_usage(spec: &Value, s: &Summary) -> Result<(), String> {
    let fields = [
        ("outputTokens", s.usage.output),
        ("cacheReadInputTokens", s.usage.cache_read),
        ("cacheWriteInputTokens", s.usage.cache_write),
        (
            "inputTokens",
            s.usage.input + s.usage.cache_read + s.usage.cache_write,
        ),
        ("nonCachedInputTokens", s.usage.input),
    ];
    for (k, actual) in fields {
        if let Some(want) = spec[k].as_u64() {
            if want != actual {
                return Err(format!("usage.{k}: want {want}, got {actual}"));
            }
        }
    }
    for k in spec[">0"].as_array().into_iter().flatten() {
        if let Some((_, actual)) = fields.iter().find(|(f, _)| Some(*f) == k.as_str()) {
            if *actual == 0 {
                return Err(format!("usage.{k} should be > 0"));
            }
        }
    }
    Ok(())
}

/// What OpenCode's per-cassette tests assert (from the expectations file).
fn expected(spec: &Value, s: &Summary) -> Result<(), String> {
    if spec.is_null() {
        return Ok(());
    }
    if !spec["text"].is_null() && !text_matches(&s.text, &spec["text"]) {
        return Err(format!("text {:?} vs {}", s.text, spec["text"]));
    }
    match spec["reasoning"].as_str() {
        Some("present") if s.reasoning.trim().is_empty() => return Err("no reasoning".into()),
        Some("absent") if !s.reasoning.is_empty() => {
            return Err(format!("unexpected reasoning {:?}", s.reasoning))
        }
        _ => {}
    }
    if let Some(want) = spec["tool_calls"].as_array() {
        let names: Vec<&str> = want.iter().filter_map(|t| t["name"].as_str()).collect();
        let got: Vec<&str> = s.tools.iter().map(|t| t.0.as_str()).collect();
        if names != got {
            return Err(format!("tool calls {got:?}, want {names:?}"));
        }
        for (w, (_, input)) in want.iter().zip(&s.tools) {
            if !w["input"].is_null() && &w["input"] != input {
                return Err(format!("tool input {input} != {}", w["input"]));
            }
        }
    }
    if let Some(f) = spec["finish"].as_str() {
        if f != s.finish {
            return Err(format!("finish {:?}, want {f:?}", s.finish));
        }
    }
    if spec["usage"].is_object() {
        expected_usage(&spec["usage"], s)?;
    }
    Ok(())
}

#[test]
fn opencode_recordings_replay() {
    let expectations: Value = std::fs::read_to_string(EXPECTATIONS)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);
    let mut report = Report {
        failures: Vec::new(),
        used: BTreeSet::new(),
    };
    let all = cassettes();
    assert!(all.len() > 200, "fixtures missing: {}", all.len());
    let mut calls = 0;
    for c in &all {
        let spec = &expectations[format!("{}.json", c.name)];
        for (i, it) in c.interactions.iter().enumerate() {
            calls += 1;
            let name = c.name.as_str();
            let proto = proto_of(it["request"]["url"].as_str().unwrap_or(""))
                .unwrap_or_else(|| panic!("{name}: unsupported URL"));
            let resp = &it["response"];
            let body = decode_body(resp);
            let status = resp["status"].as_u64().unwrap_or(0) as u16;
            let call_spec = &spec["calls"][i];
            if !(200..300).contains(&status) {
                // CodeSmith surfaces these as `Error::Status` before parsing.
                let e = Error::Status {
                    status,
                    body: body.clone(),
                };
                report.check(
                    name,
                    i,
                    "error",
                    !e.retryable()
                        && !body.is_empty()
                        && (call_spec.is_null() || !call_spec["error"].is_null()),
                    format!("status {status} should be a non-retryable error"),
                );
                continue;
            }
            let events = match run(proto, &body) {
                Ok(ev) => ev,
                Err(e) => {
                    report.check(name, i, "parse", false, e.to_string());
                    continue;
                }
            };
            let got = match summarize(&events) {
                Ok(s) => s,
                Err(e) => {
                    report.check(name, i, "events", false, e);
                    continue;
                }
            };
            let want = oracle(proto, &body);
            report.check(
                name,
                i,
                "text",
                got.text == want.text,
                format!("{:?} != {:?}", got.text, want.text),
            );
            report.check(
                name,
                i,
                "reasoning",
                got.reasoning.trim().is_empty() == want.reasoning.trim().is_empty(),
                format!("{:?} vs oracle {:?}", got.reasoning, want.reasoning),
            );
            report.check(
                name,
                i,
                "tools",
                got.tools == want.tools,
                format!("{:?} != {:?}", got.tools, want.tools),
            );
            report.check(
                name,
                i,
                "finish",
                got.finish == want.finish,
                format!("{:?} != {:?}", got.finish, want.finish),
            );
            report.check(
                name,
                i,
                "usage",
                got.usage == want.usage,
                format!("{:?} != {:?}", got.usage, want.usage),
            );
            if let Some(Err(e)) = golden(c, i, &got) {
                report.check(name, i, "golden", false, e);
            }
            if let Err(e) = expected(call_spec, &got) {
                report.check(name, i, "expect", false, e);
            }
        }
    }
    let stale: Vec<_> = GAPS
        .iter()
        .enumerate()
        .filter(|(i, _)| !report.used.contains(i))
        .map(|(_, g)| format!("{} [{}]", g.0, g.1))
        .collect();
    assert!(
        report.failures.is_empty() && stale.is_empty(),
        "{} of {calls} calls in {} cassettes failed:\n{}\nstale gaps:\n{}",
        report.failures.len(),
        all.len(),
        report.failures.join("\n"),
        stale.join("\n")
    );
}

#[test]
fn harness_helpers() {
    assert_eq!(base64("aGk="), b"hi");
    assert!(simple_regex("Hello", "^Hello!?$"));
    assert!(simple_regex("Hello!", "^Hello!?$"));
    assert!(!simple_regex("Hello there", "^Hello!?$"));
    assert!(simple_regex("Paris is sunny", r"^Paris is sunny\.?$"));
    assert!(simple_regex("hi", "^(?:hello|hi)[!.]?$"));
    assert!(simple_regex("hello.", "^(?:hello|hi)[!.]?$"));
    assert!(!simple_regex("hey", "^(?:hello|hi)[!.]?$"));
    assert!(text_matches(
        " {\"city\": \"Paris\"}\n",
        &json!({"equals": "{\"city\":\"Paris\"}"})
    ));
    assert!(text_matches("37,887", &json!({"equals": "37887"})));
    assert_eq!(
        normalize_words("Jiggling, restroom\nPrison."),
        "jiggling restroom prison"
    );
    assert_eq!(
        proto_of("https://x/v1beta/models/m:streamGenerateContent?alt=sse"),
        Some(Proto::Gemini)
    );
}
