//! Tool input validation against the tool's JSON Schema, as OpenCode
//! decodes a call's input before the tool runs (`tool/runtime.ts`). It runs
//! after input repair ([`super::input_repair`]): what is still wrong goes
//! back to the model as the call's error, in OpenCode's words, and the tool
//! never sees it.
//!
//! A focused subset of JSON Schema (drafts 07 to 2020-12), the part tool
//! schemas use: `type` (with `nullable`), `enum`, `const`, numeric bounds
//! and `multipleOf`, string lengths and `pattern`, array lengths, `items`
//! and `prefixItems`, `properties`, `required`, `additionalProperties`,
//! `allOf`, `anyOf`, `oneOf`, and local `$ref`s (`#`, `#/$defs/…`,
//! `#/definitions/…`, any JSON pointer into the schema). Other keywords
//! (`format`, `not`, `if`, `patternProperties`, …) are not checked, so they
//! never refuse input. A schema that cannot be compiled (a `$ref` that does
//! not resolve, a remote one) validates nothing: the input passes through,
//! as in OpenCode. Properties a closed object (`additionalProperties:
//! false`) does not name are dropped, as OpenCode's decode drops them.
//!
//! A schema is compiled once and kept by tool name while it stays the same
//! ([`Cache`]).

use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// How deep a value is followed (a recursive `$ref` chain that consumes no
/// input stops here too); deeper input is not checked.
const MAX_DEPTH: usize = 128;
/// Issues listed in the error; the rest are counted.
const MAX_ISSUES: usize = 5;
/// The longest echo of the arguments in the error, in bytes.
const MAX_ECHO: usize = 8 * 1024;

/// One thing wrong with the input, at `path` (property names and indexes).
#[derive(Debug, Clone, PartialEq)]
pub struct Issue {
    pub path: Vec<Seg>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Seg {
    Key(String),
    Index(usize),
}

/// A numeric bound, its words in the issue, and the test it sets.
type Bound<'a> = (&'a Option<(f64, Value)>, &'static str, fn(f64, f64) -> bool);

/// A compiled schema.
#[derive(Debug)]
pub struct Validator {
    nodes: Vec<Node>,
}

#[derive(Debug, Default)]
enum Node {
    /// `true`, `{}` or a reference not yet filled.
    #[default]
    Any,
    /// `false`.
    Never,
    Schema(Box<Rules>),
}

#[derive(Debug, Default)]
struct Rules {
    types: Option<Vec<Ty>>,
    nullable: bool,
    one_of_values: Option<Vec<Value>>,
    constant: Option<Value>,
    minimum: Option<(f64, Value)>,
    maximum: Option<(f64, Value)>,
    exclusive_minimum: Option<(f64, Value)>,
    exclusive_maximum: Option<(f64, Value)>,
    multiple_of: Option<(f64, Value)>,
    min_length: Option<u64>,
    max_length: Option<u64>,
    pattern: Option<(regex::Regex, String)>,
    min_items: Option<u64>,
    max_items: Option<u64>,
    /// `prefixItems` (or draft-07 array `items`).
    prefix: Vec<usize>,
    /// The schema of items after the prefix.
    items: Option<usize>,
    properties: Vec<(String, usize)>,
    required: Vec<String>,
    /// `additionalProperties`: `None` allows any; `Some(None)` is `false`.
    additional: Option<Option<usize>>,
    all_of: Vec<usize>,
    any_of: Vec<usize>,
    one_of: Vec<usize>,
    reference: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Ty {
    Null,
    Boolean,
    Object,
    Array,
    Number,
    Integer,
    String,
}

impl Ty {
    fn parse(s: &str) -> Option<Ty> {
        Some(match s {
            "null" => Ty::Null,
            "boolean" => Ty::Boolean,
            "object" => Ty::Object,
            "array" => Ty::Array,
            "number" => Ty::Number,
            "integer" => Ty::Integer,
            "string" => Ty::String,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Ty::Null => "null",
            Ty::Boolean => "boolean",
            Ty::Object => "object",
            Ty::Array => "array",
            Ty::Number => "number",
            Ty::Integer => "an integer",
            Ty::String => "string",
        }
    }

    fn matches(self, v: &Value) -> bool {
        match self {
            Ty::Null => v.is_null(),
            Ty::Boolean => v.is_boolean(),
            Ty::Object => v.is_object(),
            Ty::Array => v.is_array(),
            Ty::Number => v.is_number(),
            Ty::Integer => is_integer(v),
            Ty::String => v.is_string(),
        }
    }
}

fn is_integer(v: &Value) -> bool {
    match v {
        Value::Number(n) => {
            n.is_i64()
                || n.is_u64()
                || n.as_f64()
                    .is_some_and(|f| f.is_finite() && f.fract() == 0.0)
        }
        _ => false,
    }
}

/// Compile `schema`; `None` when it cannot be (a `$ref` that does not
/// resolve within it).
pub fn compile(schema: &Value) -> Option<Validator> {
    let mut c = Compiler {
        root: schema,
        nodes: Vec::new(),
        refs: HashMap::new(),
    };
    c.node(schema, 0)?;
    Some(Validator { nodes: c.nodes })
}

struct Compiler<'a> {
    root: &'a Value,
    nodes: Vec<Node>,
    /// Node of each `$ref` target compiled so far, by pointer.
    refs: HashMap<String, usize>,
}

impl Compiler<'_> {
    fn node(&mut self, schema: &Value, depth: usize) -> Option<usize> {
        if depth > MAX_DEPTH {
            return None;
        }
        let id = self.nodes.len();
        self.nodes.push(Node::Any);
        let node = match schema {
            Value::Bool(true) => Node::Any,
            Value::Bool(false) => Node::Never,
            Value::Object(s) => Node::Schema(Box::new(self.rules(s, depth)?)),
            // Not a schema: nothing to check.
            _ => Node::Any,
        };
        self.nodes[id] = node;
        Some(id)
    }

    fn reference(&mut self, r: &str, depth: usize) -> Option<usize> {
        let pointer = r.strip_prefix('#')?;
        if let Some(&id) = self.refs.get(pointer) {
            return Some(id);
        }
        let target = resolve(self.root, pointer)?;
        // Reserved first, so a recursive reference finds it.
        let id = self.nodes.len();
        self.nodes.push(Node::Any);
        self.refs.insert(pointer.to_string(), id);
        let inner = self.node(target, depth + 1)?;
        self.nodes[id] = Node::Schema(Box::new(Rules {
            reference: Some(inner),
            ..Default::default()
        }));
        Some(id)
    }

    fn list(&mut self, v: Option<&Value>, depth: usize) -> Option<Vec<usize>> {
        match v {
            Some(Value::Array(items)) => items.iter().map(|s| self.node(s, depth + 1)).collect(),
            _ => Some(Vec::new()),
        }
    }

    fn rules(&mut self, s: &Map<String, Value>, depth: usize) -> Option<Rules> {
        let num = |k: &str| {
            s.get(k)
                .filter(|v| v.is_number())
                .and_then(|v| Some((v.as_f64()?, v.clone())))
        };
        let count = |k: &str| s.get(k).and_then(Value::as_u64);
        let mut r = Rules {
            types: match s.get("type") {
                Some(Value::String(t)) => Ty::parse(t).map(|t| vec![t]),
                Some(Value::Array(ts)) => {
                    let ts: Vec<Ty> = ts.iter().filter_map(|t| Ty::parse(t.as_str()?)).collect();
                    (!ts.is_empty()).then_some(ts)
                }
                _ => None,
            },
            nullable: s.get("nullable") == Some(&Value::Bool(true)),
            one_of_values: s.get("enum").and_then(Value::as_array).cloned(),
            constant: s.get("const").cloned(),
            minimum: num("minimum"),
            maximum: num("maximum"),
            multiple_of: num("multipleOf").filter(|(f, _)| *f > 0.0),
            min_length: count("minLength"),
            max_length: count("maxLength"),
            pattern: s
                .get("pattern")
                .and_then(Value::as_str)
                .and_then(|p| Some((regex::Regex::new(p).ok()?, p.to_string()))),
            min_items: count("minItems"),
            max_items: count("maxItems"),
            required: s
                .get("required")
                .and_then(Value::as_array)
                .map(|r| {
                    r.iter()
                        .filter_map(|k| k.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            ..Default::default()
        };
        // Draft 4's boolean exclusive bounds, and later drafts' numeric ones.
        match s.get("exclusiveMinimum") {
            Some(Value::Bool(true)) => r.exclusive_minimum = r.minimum.take(),
            Some(v) if v.is_number() => r.exclusive_minimum = num("exclusiveMinimum"),
            _ => {}
        }
        match s.get("exclusiveMaximum") {
            Some(Value::Bool(true)) => r.exclusive_maximum = r.maximum.take(),
            Some(v) if v.is_number() => r.exclusive_maximum = num("exclusiveMaximum"),
            _ => {}
        }
        if let Some(Value::String(reference)) = s.get("$ref") {
            r.reference = Some(self.reference(reference, depth)?);
        }
        match (s.get("prefixItems"), s.get("items")) {
            (Some(Value::Array(prefix)), items) => {
                r.prefix = prefix
                    .iter()
                    .map(|p| self.node(p, depth + 1))
                    .collect::<Option<_>>()?;
                if let Some(items) = items {
                    r.items = Some(self.node(items, depth + 1)?);
                }
            }
            (_, Some(Value::Array(tuple))) => {
                r.prefix = tuple
                    .iter()
                    .map(|p| self.node(p, depth + 1))
                    .collect::<Option<_>>()?;
                if let Some(extra) = s.get("additionalItems") {
                    r.items = Some(self.node(extra, depth + 1)?);
                }
            }
            (_, Some(items)) => r.items = Some(self.node(items, depth + 1)?),
            _ => {}
        }
        if let Some(Value::Object(props)) = s.get("properties") {
            for (k, p) in props {
                r.properties.push((k.clone(), self.node(p, depth + 1)?));
            }
        }
        match s.get("additionalProperties") {
            Some(Value::Bool(false)) => r.additional = Some(None),
            Some(Value::Bool(true)) | None => {}
            Some(a) => r.additional = Some(Some(self.node(a, depth + 1)?)),
        }
        // Extras matched by a pattern are not "additional"; not checked
        // here, so they are kept.
        if s.get("patternProperties")
            .is_some_and(|p| p.as_object().is_some_and(|p| !p.is_empty()))
        {
            r.additional = None;
        }
        r.all_of = self.list(s.get("allOf"), depth)?;
        r.any_of = self.list(s.get("anyOf"), depth)?;
        r.one_of = self.list(s.get("oneOf"), depth)?;
        // Extras may belong to a composed schema (as repair keeps them).
        if !(r.all_of.is_empty() && r.any_of.is_empty() && r.one_of.is_empty()) {
            r.additional = None;
        }
        Some(r)
    }
}

/// The value a JSON pointer (`""`, `/a/b`, URI-escaped or not) names.
fn resolve<'a>(root: &'a Value, pointer: &str) -> Option<&'a Value> {
    let pointer = percent_decode(pointer)?;
    if pointer.is_empty() {
        return Some(root);
    }
    let mut at = root;
    for raw in pointer.strip_prefix('/')?.split('/') {
        let key = raw.replace("~1", "/").replace("~0", "~");
        at = match at {
            Value::Object(m) => m.get(&key)?,
            Value::Array(a) => a.get(key.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(at)
}

fn percent_decode(s: &str) -> Option<String> {
    if !s.contains('%') {
        return Some(s.to_string());
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// A number as JSON writes it (`1`, not `1.0`).
fn show(v: &Value) -> String {
    match v.as_f64() {
        Some(f) if f.fract() == 0.0 && f.abs() < 1e15 && !(v.is_i64() || v.is_u64()) => {
            format!("{}", f as i64)
        }
        _ => v.to_string(),
    }
}

impl Validator {
    /// Check `value`, dropping the properties closed objects do not name;
    /// the issues, in the order found (empty: valid).
    pub fn check(&self, value: &mut Value) -> Vec<Issue> {
        let mut issues = Vec::new();
        let mut path = Vec::new();
        self.at(0, value, &mut path, &mut issues, 0);
        issues
    }

    fn at(
        &self,
        id: usize,
        v: &mut Value,
        path: &mut Vec<Seg>,
        out: &mut Vec<Issue>,
        depth: usize,
    ) {
        if depth > MAX_DEPTH {
            return;
        }
        let r = match &self.nodes[id] {
            Node::Any => return,
            Node::Never => {
                out.push(issue(path, "Unexpected value".into()));
                return;
            }
            Node::Schema(r) => r,
        };
        let issue_at = |out: &mut Vec<Issue>, path: &[Seg], m: String| out.push(issue(path, m));
        if let Some(target) = r.reference {
            self.at(target, v, path, out, depth + 1);
        }
        if r.nullable && v.is_null() {
            return;
        }
        if let Some(types) = &r.types {
            if !types.iter().any(|t| t.matches(v)) {
                let names: Vec<&str> = types.iter().map(|t| t.name()).collect();
                issue_at(out, path, format!("Expected {}", names.join(" | ")));
                return;
            }
        }
        if let Some(values) = &r.one_of_values {
            if !values.iter().any(|e| same(e, v)) {
                let names: Vec<String> = values.iter().map(Value::to_string).collect();
                issue_at(out, path, format!("Expected {}", names.join(" | ")));
                return;
            }
        }
        if let Some(c) = &r.constant {
            if !same(c, v) {
                issue_at(out, path, format!("Expected {c}"));
                return;
            }
        }
        if let Some(n) = v.as_f64().filter(|_| v.is_number()) {
            let bounds: [Bound; 4] = [
                (&r.minimum, "greater than or equal to", |n, b| n >= b),
                (&r.exclusive_minimum, "greater than", |n, b| n > b),
                (&r.maximum, "less than or equal to", |n, b| n <= b),
                (&r.exclusive_maximum, "less than", |n, b| n < b),
            ];
            for (bound, words, ok) in bounds {
                if let Some((b, raw)) = bound {
                    if !ok(n, *b) {
                        issue_at(out, path, format!("Expected a value {words} {}", show(raw)));
                    }
                }
            }
            if let Some((m, raw)) = &r.multiple_of {
                let q = n / m;
                if !q.is_finite() || (q - q.round()).abs() > 1e-9 {
                    issue_at(
                        out,
                        path,
                        format!("Expected a value that is a multiple of {}", show(raw)),
                    );
                }
            }
        }
        if let Value::String(s) = &*v {
            let len = s.chars().count() as u64;
            if let Some(min) = r.min_length.filter(|m| len < *m) {
                issue_at(
                    out,
                    path,
                    format!("Expected a value with a length of at least {min}"),
                );
            }
            if let Some(max) = r.max_length.filter(|m| len > *m) {
                issue_at(
                    out,
                    path,
                    format!("Expected a value with a length of at most {max}"),
                );
            }
            if let Some((re, p)) = &r.pattern {
                if !re.is_match(s) {
                    issue_at(
                        out,
                        path,
                        format!("Expected a string matching the RegExp {p}"),
                    );
                }
            }
        }
        if let Value::Array(items) = v {
            let len = items.len() as u64;
            if let Some(min) = r.min_items.filter(|m| len < *m) {
                issue_at(
                    out,
                    path,
                    format!("Expected a value with a length of at least {min}"),
                );
            }
            if let Some(max) = r.max_items.filter(|m| len > *m) {
                issue_at(
                    out,
                    path,
                    format!("Expected a value with a length of at most {max}"),
                );
            }
            for (i, item) in items.iter_mut().enumerate() {
                let schema = r.prefix.get(i).copied().or(r.items);
                if let Some(s) = schema {
                    path.push(Seg::Index(i));
                    self.at(s, item, path, out, depth + 1);
                    path.pop();
                }
            }
        }
        if let Value::Object(map) = v {
            for (k, s) in &r.properties {
                match map.get_mut(k) {
                    Some(p) => {
                        path.push(Seg::Key(k.clone()));
                        self.at(*s, p, path, out, depth + 1);
                        path.pop();
                    }
                    None if r.required.contains(k) => {
                        path.push(Seg::Key(k.clone()));
                        issue_at(out, path, "Missing key".into());
                        path.pop();
                    }
                    None => {}
                }
            }
            for k in &r.required {
                if !map.contains_key(k) && !r.properties.iter().any(|(p, _)| p == k) {
                    path.push(Seg::Key(k.clone()));
                    issue_at(out, path, "Missing key".into());
                    path.pop();
                }
            }
            match r.additional {
                Some(None) => map.retain(|k, _| r.properties.iter().any(|(p, _)| p == k)),
                Some(Some(s)) => {
                    for (k, p) in map.iter_mut() {
                        if !r.properties.iter().any(|(n, _)| n == k) {
                            path.push(Seg::Key(k.clone()));
                            self.at(s, p, path, out, depth + 1);
                            path.pop();
                        }
                    }
                }
                None => {}
            }
        }
        for s in &r.all_of {
            self.at(*s, v, path, out, depth + 1);
        }
        for branches in [&r.any_of, &r.one_of] {
            if !branches.is_empty() {
                self.union(branches, v, path, out, depth);
            }
        }
    }

    /// Valid when one branch is: the value becomes what that branch made of
    /// it. Otherwise the closest branch's issues, or the expected types
    /// when every branch fails on the value's type alone.
    fn union(
        &self,
        branches: &[usize],
        v: &mut Value,
        path: &mut Vec<Seg>,
        out: &mut Vec<Issue>,
        depth: usize,
    ) {
        let mut tried: Vec<Vec<Issue>> = Vec::new();
        for b in branches {
            let mut candidate = v.clone();
            let mut issues = Vec::new();
            self.at(*b, &mut candidate, path, &mut issues, depth + 1);
            if issues.is_empty() {
                *v = candidate;
                return;
            }
            tried.push(issues);
        }
        let here = |i: &Vec<Issue>| {
            i.len() == 1 && i[0].path == *path && i[0].message.starts_with("Expected ")
        };
        if tried.iter().all(here) {
            let mut names: Vec<&str> = Vec::new();
            for t in &tried {
                for n in t[0].message["Expected ".len()..].split(" | ") {
                    if !names.contains(&n) {
                        names.push(n);
                    }
                }
            }
            out.push(issue(path, format!("Expected {}", names.join(" | "))));
            return;
        }
        if let Some(closest) = tried.into_iter().min_by_key(Vec::len) {
            out.extend(closest);
        }
    }
}

/// JSON equality with `1` equal to `1.0`.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| same(a, b))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, a)| y.get(k).is_some_and(|b| same(a, b)))
        }
        _ => a == b,
    }
}

fn issue(path: &[Seg], message: String) -> Issue {
    Issue {
        path: path.to_vec(),
        message,
    }
}

/// OpenCode's error for input that failed its schema: at most five
/// issues, the arguments as given (cut at [`MAX_ECHO`]), and the
/// instruction to call again.
pub fn message(tool: &str, issues: &[Issue], input: &Value) -> String {
    let mut lines: Vec<String> = issues
        .iter()
        .take(MAX_ISSUES)
        .map(|i| {
            let mut path = String::new();
            for seg in &i.path {
                match seg {
                    Seg::Index(n) => path.push_str(&format!("[{n}]")),
                    Seg::Key(k) if path.is_empty() => path.push_str(k),
                    Seg::Key(k) => {
                        path.push('.');
                        path.push_str(k);
                    }
                }
            }
            if path.is_empty() {
                path.push_str("root");
            }
            format!("- {path}: {}", i.message)
        })
        .collect();
    if issues.len() > MAX_ISSUES {
        let more = issues.len() - MAX_ISSUES;
        lines.push(format!(
            "- ...and {more} more {}",
            if more == 1 { "issue" } else { "issues" }
        ));
    }
    let mut echo = serde_json::to_string_pretty(input).unwrap_or_default();
    if echo.len() > MAX_ECHO {
        let mut cut = MAX_ECHO;
        while !echo.is_char_boundary(cut) {
            cut -= 1;
        }
        echo.truncate(cut);
        echo.push_str("\n… (cut)");
    }
    format!(
        "Invalid arguments for tool \"{tool}\":\n{}\n\nArguments provided:\n{echo}\n\nUpdate the arguments and call the tool again.",
        lines.join("\n")
    )
}

/// Compiled schemas by tool name, each compiled once while the tool's
/// schema stays the same. A name keeps its few latest schemas, since Code
/// Mode shows a program a tool's schema in a form of its own.
#[derive(Default)]
pub struct Cache {
    compiled: Mutex<HashMap<String, Vec<Compiled>>>,
}

type Compiled = (Value, Option<Arc<Validator>>);

/// Schemas kept per tool name.
const PER_NAME: usize = 2;

impl Cache {
    /// Check a call to `tool` against `schema`: the input to run with
    /// (closed objects' extras dropped), or the error for the model.
    pub fn check(&self, tool: &str, schema: &Value, input: &Value) -> Result<Value, String> {
        let validator = {
            let mut compiled = self.compiled.lock().unwrap_or_else(|e| e.into_inner());
            let kept = compiled.entry(tool.to_string()).or_default();
            match kept.iter().find(|(s, _)| s == schema) {
                Some((_, v)) => v.clone(),
                None => {
                    let v = compile(schema).map(Arc::new);
                    if kept.len() == PER_NAME {
                        kept.remove(0);
                    }
                    kept.push((schema.clone(), v.clone()));
                    v
                }
            }
        };
        let Some(validator) = validator else {
            return Ok(input.clone());
        };
        let mut value = input.clone();
        let issues = validator.check(&mut value);
        if issues.is_empty() {
            Ok(value)
        } else {
            Err(message(tool, &issues, input))
        }
    }

    /// How many schemas are compiled (for tests).
    pub fn len(&self) -> usize {
        self.compiled
            .lock()
            .map(|c| c.values().map(Vec::len).sum())
            .unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(schema: &Value, input: Value) -> Result<Value, String> {
        Cache::default().check("raw", schema, &input)
    }

    fn raw() -> Value {
        json!({
            "type": "object",
            "properties": {
                "value": {"type": "string"},
                "nested": {
                    "type": "object",
                    "properties": {"count": {"type": "integer", "minimum": 1}},
                    "required": ["count"],
                    "additionalProperties": false
                }
            },
            "required": ["value"],
            "additionalProperties": false
        })
    }

    // tool-schema.test.ts: "raw JSON schemas validate and decode tool
    // input". Adapted: issues and the echoed arguments come in property
    // name order (CodeSmith's JSON maps are sorted).
    #[test]
    fn oc_tool_schema_raw_json_schemas_validate_and_decode_tool_input() {
        let s = raw();
        assert_eq!(
            run(&s, json!({"value": "ok", "extra": true})),
            Ok(json!({"value": "ok"}))
        );
        assert_eq!(
            run(&s, json!({"value": 1})).unwrap_err(),
            "Invalid arguments for tool \"raw\":\n- value: Expected string\n\nArguments provided:\n{\n  \"value\": 1\n}\n\nUpdate the arguments and call the tool again."
        );
        assert_eq!(
            run(&s, json!({})).unwrap_err(),
            "Invalid arguments for tool \"raw\":\n- value: Missing key\n\nArguments provided:\n{}\n\nUpdate the arguments and call the tool again."
        );
        assert_eq!(
            run(&s, json!({"value": "ok", "nested": {"count": 0}})).unwrap_err(),
            "Invalid arguments for tool \"raw\":\n- nested.count: Expected a value greater than or equal to 1\n\nArguments provided:\n{\n  \"nested\": {\n    \"count\": 0\n  },\n  \"value\": \"ok\"\n}\n\nUpdate the arguments and call the tool again."
        );
        assert_eq!(
            run(&s, json!({"value": 1, "nested": {"count": 0}})).unwrap_err(),
            "Invalid arguments for tool \"raw\":\n- nested.count: Expected a value greater than or equal to 1\n- value: Expected string\n\nArguments provided:\n{\n  \"nested\": {\n    \"count\": 0\n  },\n  \"value\": 1\n}\n\nUpdate the arguments and call the tool again."
        );
    }

    // tool-schema.test.ts: "raw JSON schemas resolve draft-07 definitions".
    // Adapted: the issue names what the reference expects ("Expected
    // string"), where OpenCode says "Expected value".
    #[test]
    fn oc_tool_schema_raw_json_schemas_resolve_draft_07_definitions() {
        let s = json!({
            "type": "object",
            "properties": {"value": {"$ref": "#/definitions/value"}},
            "required": ["value"],
            "definitions": {"value": {"type": "string"}}
        });
        assert_eq!(run(&s, json!({"value": "ok"})), Ok(json!({"value": "ok"})));
        assert_eq!(
            run(&s, json!({"value": 1})).unwrap_err(),
            "Invalid arguments for tool \"raw\":\n- value: Expected string\n\nArguments provided:\n{\n  \"value\": 1\n}\n\nUpdate the arguments and call the tool again."
        );
    }

    // tool-schema.test.ts: "input error prompts limit normalized issues"
    #[test]
    fn oc_tool_schema_input_error_prompts_limit_normalized_issues() {
        let issues: Vec<Issue> = (1..=6)
            .map(|i| Issue {
                path: vec![],
                message: format!("issue {i}"),
            })
            .collect();
        assert_eq!(
            message("limited", &issues, &json!({})),
            "Invalid arguments for tool \"limited\":\n- root: issue 1\n- root: issue 2\n- root: issue 3\n- root: issue 4\n- root: issue 5\n- ...and 1 more issue\n\nArguments provided:\n{}\n\nUpdate the arguments and call the tool again."
        );
        let s = json!({"type": "object", "required": ["a", "b", "c", "d", "e", "f", "g"]});
        let e = run(&s, json!({})).unwrap_err();
        assert!(
            e.contains("- e: Missing key\n- ...and 2 more issues\n"),
            "{e}"
        );
    }

    // tool-schema.test.ts: "raw JSON schemas pass input through when they
    // cannot be imported"
    #[test]
    fn oc_tool_schema_raw_json_schemas_pass_input_through_when_they_cannot_be_imported() {
        let s = json!({"type": "object", "properties": {"value": {"$ref": "#/$defs/missing"}}});
        assert_eq!(
            run(&s, json!({"value": 1, "extra": true})),
            Ok(json!({"value": 1, "extra": true}))
        );
        let remote =
            json!({"type": "object", "properties": {"v": {"$ref": "https://example.com/s.json"}}});
        assert!(run(&remote, json!({"v": 1})).is_ok());
    }

    #[test]
    fn checks_the_keywords_tool_schemas_use() {
        let s = json!({
            "type": "object",
            "properties": {
                "mode": {"enum": ["a", "b"]},
                "kind": {"const": "x"},
                "name": {"type": "string", "minLength": 2, "maxLength": 3, "pattern": "^[a-z]+$"},
                "tags": {"type": "array", "items": {"type": "string"}, "maxItems": 2},
                "pair": {"type": "array", "prefixItems": [{"type": "integer"}, {"type": "boolean"}]},
                "maybe": {"type": ["string", "null"]},
                "either": {"anyOf": [{"type": "string"}, {"type": "number"}]},
                "dict": {"type": "object", "additionalProperties": {"type": "integer"}},
                "low": {"type": "number", "exclusiveMaximum": 5, "multipleOf": 0.5},
                "old": {"type": "string", "nullable": true}
            }
        });
        assert!(run(
            &s,
            json!({"mode": "a", "kind": "x", "name": "ab", "tags": ["t"], "pair": [1.0, true],
            "maybe": null, "either": 2, "dict": {"k": 3}, "low": 4.5, "old": null})
        )
        .is_ok());
        let e = run(&s, json!({"mode": "c", "kind": "y", "name": "A", "tags": [1, "t", "u"], "pair": [1.5, "no"],
            "maybe": 1, "either": true, "dict": {"k": "3"}, "low": 5}))
        .unwrap_err();
        for want in [
            "- dict.k: Expected an integer",
            "- either: Expected string | number",
            "- kind: Expected \"x\"",
            "- low: Expected a value less than 5",
            "- maybe: Expected string | null",
        ] {
            assert!(e.contains(want), "{want}: {e}");
        }
        assert!(e.contains("- ...and 7 more issues"), "{e}");
        let e = run(
            &s,
            json!({"mode": "c", "name": "A", "tags": [1, "t", "u"], "pair": [1.5, "no"]}),
        )
        .unwrap_err();
        for want in [
            "- mode: Expected \"a\" | \"b\"",
            "- name: Expected a value with a length of at least 2",
            "- name: Expected a string matching the RegExp ^[a-z]+$",
            "- pair[0]: Expected an integer",
            "- pair[1]: Expected boolean",
        ] {
            assert!(e.contains(want), "{want}: {e}");
        }
        let e = run(&s, json!({"tags": [1, "t", "u"]})).unwrap_err();
        assert!(
            e.contains(
                "- tags: Expected a value with a length of at most 2\n- tags[0]: Expected string"
            ),
            "{e}"
        );
    }

    #[test]
    fn follows_recursive_references_and_closed_objects_inside_unions() {
        let s = json!({
            "$defs": {"node": {"type": "object", "properties": {"name": {"type": "string"},
                "children": {"type": "array", "items": {"$ref": "#/$defs/node"}}},
                "required": ["name"], "additionalProperties": false}},
            "type": "object",
            "properties": {"tree": {"$ref": "#/$defs/node"},
                "loop": {"$ref": "#/properties/loop"},
                "pick": {"oneOf": [{"type": "object", "properties": {"a": {"type": "string"}}, "required": ["a"], "additionalProperties": false},
                                   {"type": "object", "properties": {"b": {"type": "string"}}, "required": ["b"], "additionalProperties": false}]}}
        });
        let ok = run(
            &s,
            json!({"tree": {"name": "r", "children": [{"name": "c", "x": 1}]}, "loop": 1, "pick": {"b": "y", "z": 0}}),
        );
        assert_eq!(
            ok,
            Ok(
                json!({"tree": {"name": "r", "children": [{"name": "c"}]}, "loop": 1, "pick": {"b": "y"}})
            )
        );
        let e = run(
            &s,
            json!({"tree": {"name": "r", "children": [{"children": []}]}}),
        )
        .unwrap_err();
        assert!(e.contains("- tree.children[0].name: Missing key"), "{e}");
    }

    #[test]
    fn compiles_once_per_schema() {
        let cache = Cache::default();
        let s = raw();
        assert!(cache.check("raw", &s, &json!({"value": "a"})).is_ok());
        assert!(cache.check("raw", &s, &json!({"value": 1})).is_err());
        assert_eq!(cache.len(), 1);
        // A changed schema is compiled too; the latest two are kept.
        let open = json!({"type": "object"});
        assert!(cache.check("raw", &open, &json!({"value": 1})).is_ok());
        assert!(cache.check("raw", &s, &json!({"value": 1})).is_err());
        assert_eq!(cache.len(), 2);
        let other = json!({"type": "object", "required": ["x"]});
        assert!(cache.check("raw", &other, &json!({})).is_err());
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn cuts_a_long_echo() {
        let s = json!({"type": "object", "properties": {"n": {"type": "number"}}});
        let e = run(&s, json!({"n": "x".repeat(MAX_ECHO * 2)})).unwrap_err();
        assert!(e.len() < MAX_ECHO + 300, "{}", e.len());
        assert!(e.ends_with("… (cut)\n\nUpdate the arguments and call the tool again."));
    }
}
