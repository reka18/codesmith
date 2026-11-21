//! TypeScript signatures for Code Mode tools, as OpenCode renders them
//! (`packages/codemode/src/codemode/tool-schema.ts`): JSON Schema to a
//! TypeScript type, compact (`{ a: string; b?: number }`) or pretty, where
//! every field carries its description and constraints as JSDoc
//! (`@default`, `@format`, `@minimum`, `@pattern`, `@minItems`, ...).

use serde_json::{Map, Value};
use std::cell::Cell;
use std::collections::BTreeSet;

const MAX_RENDER_DEPTH: usize = 8;
/// `$ref` hops along one path, counted apart from nesting so references
/// keep their indentation.
const MAX_REF_HOPS: usize = 8;
/// Nodes one type may render or check (see `Budget`).
const MAX_RENDER_NODES: usize = 20_000;
/// Characters the pieces of one type may add up to while rendering.
const MAX_RENDER_OUTPUT: usize = 4 * 1024 * 1024;
/// The longest type a signature shows; past it the type is `unknown`.
pub const MAX_TYPE_LEN: usize = 64 * 1024;
const ANNOTATIONS: &[&str] = &[
    "title",
    "description",
    "default",
    "examples",
    "readOnly",
    "writeOnly",
];
const MIN_SAFE_INTEGER: f64 = -9007199254740991.0;
const MAX_SAFE_INTEGER: f64 = 9007199254740991.0;

/// A path segment that needs no quoting: `[A-Za-z_$][A-Za-z0-9_$]*`.
pub fn is_identifier(s: &str) -> bool {
    let mut c = s.chars();
    c.next()
        .is_some_and(|f| f.is_ascii_alphabetic() || f == '_' || f == '$')
        && c.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '$')
}

fn quote(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

fn key(name: &str) -> String {
    if is_identifier(name) {
        name.to_string()
    } else {
        quote(name)
    }
}

/// JavaScript's `String(number)` for the numbers a schema holds.
fn number(n: &serde_json::Number) -> String {
    match n.as_f64() {
        Some(f) if f.fract() == 0.0 && f.abs() < 1e21 => format!("{}", f as i128),
        Some(f) => {
            let s = format!("{f}");
            if f.abs() >= 1e21 || (f != 0.0 && f.abs() < 1e-6) {
                // JS switches to exponent form here; serde's is close enough.
                n.to_string()
            } else {
                s
            }
        }
        None => n.to_string(),
    }
}

/// `JSON.stringify` as JavaScript writes it (numbers as JS prints them).
fn literal(v: &Value) -> String {
    match v {
        Value::Number(n) => number(n),
        Value::Array(a) => format!("[{}]", a.iter().map(literal).collect::<Vec<_>>().join(",")),
        Value::Object(o) => format!(
            "{{{}}}",
            o.iter()
                .map(|(k, v)| format!("{}:{}", quote(k), literal(v)))
                .collect::<Vec<_>>()
                .join(",")
        ),
        _ => v.to_string(),
    }
}

/// `#/$defs/Name` or `#/definitions/Name`: the name.
fn definition_name(r: &str) -> Option<String> {
    let rest = r.strip_prefix('#')?;
    let decoded = percent_decode(rest)?;
    if decoded.is_empty() {
        return None;
    }
    let tokens: Vec<String> = decoded
        .strip_prefix('/')?
        .split('/')
        .map(|t| t.replace("~1", "/").replace("~0", "~"))
        .collect();
    (tokens.len() == 2 && (tokens[0] == "$defs" || tokens[0] == "definitions"))
        .then(|| tokens[1].clone())
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(h, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn obj(s: &Value) -> Option<&Map<String, Value>> {
    s.as_object()
}

fn get<'a>(s: &'a Value, k: &str) -> Option<&'a Value> {
    s.as_object().and_then(|o| o.get(k))
}

fn type_is(s: &Value, t: &str) -> bool {
    get(s, "type").and_then(Value::as_str) == Some(t)
}

/// Effect's `number | "Infinity" | "-Infinity" | "NaN"` sentinel member.
fn number_sentinel(s: &Value) -> bool {
    type_is(s, "string")
        && get(s, "enum").and_then(Value::as_array).is_some_and(|e| {
            !e.is_empty()
                && e.iter()
                    .all(|v| matches!(v.as_str(), Some("NaN" | "Infinity" | "-Infinity")))
        })
}

fn intersection(members: &[String]) -> String {
    let concrete: Vec<&String> = members.iter().filter(|m| *m != "unknown").collect();
    match concrete.len() {
        0 => "unknown".into(),
        1 => concrete[0].clone(),
        _ => concrete
            .iter()
            .map(|m| {
                if m.contains(" | ") {
                    format!("({m})")
                } else {
                    m.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(" & "),
    }
}

fn unresolved(
    s: &Value,
    defs: &Defs,
    seen: &BTreeSet<String>,
    visited: &mut Vec<*const Value>,
    budget: &Budget,
) -> bool {
    let p = s as *const Value;
    if visited.contains(&p) || visited.len() > 64 {
        return false;
    }
    // Out of budget: say unresolved, so the caller renders `unknown`.
    if !budget.take() {
        return true;
    }
    visited.push(p);
    let out = (|| {
        if let Some(r) = get(s, "$ref").and_then(Value::as_str) {
            let Some(name) = definition_name(r) else {
                return true;
            };
            let Some(def) = lookup(defs, &name) else {
                return true;
            };
            if seen.contains(&name) {
                return true;
            }
            let mut seen = seen.clone();
            seen.insert(name);
            if unresolved(def, defs, &seen, visited, budget) {
                return true;
            }
        }
        let mut items: Vec<&Value> = vec![];
        for k in ["anyOf", "oneOf", "allOf"] {
            if let Some(a) = get(s, k).and_then(Value::as_array) {
                items.extend(a);
            }
        }
        if let Some(p) = get(s, "properties").and_then(Value::as_object) {
            items.extend(p.values());
        }
        if let Some(i) = get(s, "items") {
            items.push(i);
        }
        if let Some(a) = get(s, "additionalProperties").filter(|a| a.is_object()) {
            items.push(a);
        }
        items
            .into_iter()
            .any(|i| unresolved(i, defs, seen, visited, budget))
    })();
    visited.pop();
    out
}

fn doc_tags(s: &Value) -> Vec<String> {
    let mut tags = vec![];
    let num = |k: &str| {
        get(s, k)
            .filter(|v| v.is_number())
            .and_then(|v| v.as_number())
    };
    if get(s, "deprecated") == Some(&Value::Bool(true)) {
        tags.push("@deprecated".into());
    }
    if let Some(d) = get(s, "default") {
        tags.push(format!("@default {}", literal(d)));
    }
    if let Some(f) = get(s, "format").and_then(Value::as_str) {
        tags.push(format!("@format {f}"));
    }
    let integer = type_is(s, "integer");
    if integer {
        tags.push("@integer".into());
    }
    if let Some(n) = num("minimum") {
        if !(integer && n.as_f64() == Some(MIN_SAFE_INTEGER)) {
            tags.push(format!("@minimum {}", number(n)));
        }
    }
    if let Some(n) = num("maximum") {
        if !(integer && n.as_f64() == Some(MAX_SAFE_INTEGER)) {
            tags.push(format!("@maximum {}", number(n)));
        }
    }
    for k in [
        "exclusiveMinimum",
        "exclusiveMaximum",
        "multipleOf",
        "minLength",
        "maxLength",
    ] {
        if let Some(n) = num(k) {
            tags.push(format!("@{k} {}", number(n)));
        }
    }
    if let Some(p) = get(s, "pattern").and_then(Value::as_str) {
        tags.push(format!("@pattern {p}"));
    }
    for k in ["minItems", "maxItems"] {
        if let Some(n) = num(k) {
            tags.push(format!("@{k} {}", number(n)));
        }
    }
    if get(s, "uniqueItems") == Some(&Value::Bool(true)) {
        tags.push("@uniqueItems true".into());
    }
    tags
}

fn doc_lines(s: &Value, width: usize) -> Vec<String> {
    let summary = doc_tags(s).join(" ");
    let description = get(s, "description").and_then(Value::as_str).unwrap_or("");
    let mut lines: Vec<String> = description
        .split('\n')
        .map(|l| l.trim_end().to_string())
        .collect();
    while lines.first().is_some_and(|l| l.trim().is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    if !summary.is_empty() && lines.len() == 1 && !summary.contains('\n') {
        let first = &lines[0];
        let inline = format!(
            "{first}{} {summary}",
            if first.ends_with('.') { "" } else { "." }
        );
        if width + utf16_len(&inline) + 7 <= 120 {
            return vec![inline];
        }
    }
    if !summary.is_empty() {
        lines.extend(summary.split('\n').map(String::from));
    }
    lines
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

fn jsdoc(s: &Value, pad: &str) -> String {
    let mut content = doc_lines(s, pad.len());
    let types: Vec<&str> = match get(s, "type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    };
    let mut append = |label: &str, child: &Value| {
        for (i, line) in doc_lines(child, pad.len() + label.len() + 2)
            .into_iter()
            .enumerate()
        {
            if i == 0 {
                content.push(format!("{label}: {line}"));
            } else if line.is_empty() {
                content.push(String::new());
            } else {
                content.push(format!("  {line}"));
            }
        }
    };
    if types.contains(&"array") {
        if let Some(items) = get(s, "items") {
            append("Each item", items);
        }
    }
    let properties = get(s, "properties");
    if types.contains(&"object") || properties.is_some() {
        if let Some(extra) = get(s, "additionalProperties").filter(|a| a.is_object()) {
            let named = properties
                .and_then(Value::as_object)
                .is_some_and(|p| !p.is_empty());
            append(
                if named {
                    "Each additional value"
                } else {
                    "Each value"
                },
                extra,
            );
        }
    }
    if content.is_empty() {
        return String::new();
    }
    let escaped: Vec<String> = content.iter().map(|l| l.replace("*/", "* /")).collect();
    if escaped.len() == 1 && pad.len() + utf16_len(&escaped[0]) + 7 <= 120 {
        return format!("{pad}/** {} */\n", escaped[0]);
    }
    let body = escaped
        .iter()
        .map(|l| {
            if l.is_empty() {
                format!("{pad} *")
            } else {
                format!("{pad} * {l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("{pad}/**\n{body}\n{pad} */\n")
}

/// The definitions in scope: the outermost first, so a nested `$defs`
/// shadows an inherited one (looked up from the end).
type Defs<'a> = Vec<&'a Map<String, Value>>;

fn lookup<'a>(defs: &Defs<'a>, name: &str) -> Option<&'a Value> {
    defs.iter().rev().find_map(|m| m.get(name))
}

/// What one type may spend, so a hostile schema (tool schemas come from MCP
/// servers) cannot make rendering explode: every node rendered or checked
/// counts against `nodes`, every rendered piece's length against `output`.
/// Once either runs out, the rest renders as `unknown`.
struct Budget {
    nodes: Cell<usize>,
    output: Cell<usize>,
}

impl Budget {
    fn new() -> Budget {
        Budget {
            nodes: Cell::new(MAX_RENDER_NODES),
            output: Cell::new(MAX_RENDER_OUTPUT),
        }
    }

    /// Spend one node; false once the budget is gone.
    fn take(&self) -> bool {
        let n = self.nodes.get();
        if n == 0 || self.output.get() == 0 {
            self.nodes.set(0);
            return false;
        }
        self.nodes.set(n - 1);
        true
    }

    fn spend_output(&self, len: usize) {
        self.output.set(self.output.get().saturating_sub(len));
    }
}

const SKIP_REF: u8 = 1;
const SKIP_UNION: u8 = 2;
const SKIP_ALL: u8 = 4;

/// A schema with some of its keys hidden or its `type` replaced, without
/// copying it.
#[derive(Clone, Copy)]
struct View<'a> {
    s: &'a Value,
    skip: u8,
    ty: Option<&'a Value>,
}

impl<'a> View<'a> {
    fn of(s: &'a Value) -> View<'a> {
        View {
            s,
            skip: 0,
            ty: None,
        }
    }

    fn hidden(&self, k: &str) -> bool {
        match k {
            "$ref" => self.skip & SKIP_REF != 0,
            "anyOf" | "oneOf" => self.skip & SKIP_UNION != 0,
            "allOf" => self.skip & SKIP_ALL != 0,
            _ => false,
        }
    }

    fn get(&self, k: &str) -> Option<&'a Value> {
        if k == "type" && self.ty.is_some() {
            return self.ty;
        }
        if self.hidden(k) {
            return None;
        }
        get(self.s, k)
    }

    fn type_is(&self, t: &str) -> bool {
        self.get("type").and_then(Value::as_str) == Some(t)
    }

    fn without(&self, skip: u8) -> View<'a> {
        View {
            skip: self.skip | skip,
            ..*self
        }
    }

    fn with_type(&self, t: &'a Value) -> View<'a> {
        View {
            ty: Some(t),
            ..*self
        }
    }

    /// `{ not: { type: "null" } }` with nothing but annotations beside it.
    fn empty_struct(&self) -> bool {
        if self.ty.is_some() {
            return false;
        }
        let Some(o) = obj(self.s) else { return false };
        let Some(not) = o.get("not").and_then(Value::as_object) else {
            return false;
        };
        not.get("type").and_then(Value::as_str) == Some("null")
            && not.len() == 1
            && o.keys()
                .all(|k| self.hidden(k) || k == "not" || ANNOTATIONS.contains(&k.as_str()))
    }
}

struct Ctx<'a, 'b> {
    defs: Defs<'a>,
    pretty: bool,
    budget: &'b Budget,
}

fn render<'a>(
    v: View<'a>,
    ctx: &Ctx<'a, '_>,
    depth: usize,
    hops: usize,
    seen: &BTreeSet<String>,
) -> String {
    if depth > MAX_RENDER_DEPTH || !ctx.budget.take() {
        return "unknown".into();
    }
    let out = render_node(v, ctx, depth, hops, seen);
    ctx.budget.spend_output(out.len());
    out
}

fn render_node<'a>(
    v: View<'a>,
    ctx: &Ctx<'a, '_>,
    depth: usize,
    hops: usize,
    seen: &BTreeSet<String>,
) -> String {
    let local = ["definitions", "$defs"]
        .iter()
        .filter_map(|k| v.get(k).and_then(Value::as_object))
        .collect::<Vec<_>>();
    let merged;
    let ctx = if local.is_empty() {
        ctx
    } else {
        let mut defs = ctx.defs.clone();
        defs.extend(local);
        merged = Ctx {
            defs,
            pretty: ctx.pretty,
            budget: ctx.budget,
        };
        &merged
    };
    if let Some(r) = v.get("$ref").filter(|r| !r.is_null()) {
        let name = r.as_str().and_then(definition_name);
        let Some((name, def)) = name
            .filter(|n| !seen.contains(n))
            .and_then(|n| lookup(&ctx.defs, &n).map(|d| (n, d)))
        else {
            return "unknown".into();
        };
        // A reference counts toward its own depth limit, so a chain of
        // them cannot recurse without bound.
        if hops >= MAX_REF_HOPS {
            return "unknown".into();
        }
        let mut inner = seen.clone();
        inner.insert(name);
        return intersection(&[
            render(View::of(def), ctx, depth, hops + 1, &inner),
            render(v.without(SKIP_REF), ctx, depth + 1, hops, seen),
        ]);
    }
    if let Some(c) = v.get("const") {
        return literal(c);
    }
    if let Some(e) = v.get("enum").and_then(Value::as_array) {
        return e.iter().map(literal).collect::<Vec<_>>().join(" | ");
    }
    let listed = v
        .get("anyOf")
        .and_then(Value::as_array)
        .or_else(|| v.get("oneOf").and_then(Value::as_array));
    if let Some(listed) = listed {
        let alternatives: Vec<&Value> = if listed.iter().any(|i| type_is(i, "number")) {
            listed.iter().filter(|i| !number_sentinel(i)).collect()
        } else {
            listed.iter().collect()
        };
        if alternatives.iter().any(|i| type_is(i, "number"))
            && alternatives
                .iter()
                .all(|i| type_is(i, "number") || number_sentinel(i))
        {
            return "number".into();
        }
        if alternatives.len() == 2
            && type_is(alternatives[0], "object")
            && get(alternatives[0], "properties").is_none()
            && type_is(alternatives[1], "array")
            && get(alternatives[1], "items").is_none()
        {
            return "{}".into();
        }
        let mut members = Vec::with_capacity(alternatives.len());
        for i in alternatives {
            let m = render(View::of(i), ctx, depth + 1, hops, seen);
            if m == "unknown" {
                return "unknown".into();
            }
            members.push(m);
        }
        return intersection(&[
            members.join(" | "),
            render(v.without(SKIP_UNION), ctx, depth + 1, hops, seen),
        ]);
    }
    if let Some(all) = v.get("allOf").and_then(Value::as_array) {
        if all
            .iter()
            .any(|i| unresolved(i, &ctx.defs, &BTreeSet::new(), &mut vec![], ctx.budget))
        {
            return "unknown".into();
        }
        let mut members = vec![render(v.without(SKIP_ALL), ctx, depth + 1, hops, seen)];
        members.extend(
            all.iter()
                .map(|i| render(View::of(i), ctx, depth + 1, hops, seen)),
        );
        return intersection(&members);
    }
    if let Some(types) = v.get("type").and_then(Value::as_array) {
        return types
            .iter()
            .map(|t| render(v.with_type(t), ctx, depth + 1, hops, seen))
            .collect::<Vec<_>>()
            .join(" | ");
    }
    match v.get("type").and_then(Value::as_str) {
        Some("string") => return "string".into(),
        Some("number") | Some("integer") => return "number".into(),
        Some("boolean") => return "boolean".into(),
        Some("null") => return "null".into(),
        Some("array") => {
            let empty = Value::Object(Map::new());
            let items = v.get("items").unwrap_or(&empty);
            return format!(
                "Array<{}>",
                render(View::of(items), ctx, depth + 1, hops, seen)
            );
        }
        _ => {}
    }
    if v.type_is("object") || v.get("properties").is_some() {
        let required: Vec<&str> = v
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let empty = Map::new();
        let properties = v
            .get("properties")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        let index = v
            .get("additionalProperties")
            .filter(|a| a.is_object())
            .map(|a| render(View::of(a), ctx, depth + 1, hops, seen));
        let field = |name: &str, f: &Value| {
            format!(
                "{}{}: {}",
                key(name),
                if required.contains(&name) { "" } else { "?" },
                render(View::of(f), ctx, depth + 1, hops, seen)
            )
        };
        if !ctx.pretty {
            let mut fields: Vec<String> = properties.iter().map(|(k, f)| field(k, f)).collect();
            if let Some(i) = &index {
                fields.push(format!("[key: string]: {i}"));
            }
            return if fields.is_empty() {
                "{}".into()
            } else {
                format!("{{ {} }}", fields.join("; "))
            };
        }
        if properties.is_empty() && index.is_none() {
            return "{}".into();
        }
        let pad = "  ".repeat(depth + 1);
        let mut lines: Vec<String> = properties
            .iter()
            .map(|(k, f)| format!("{}{pad}{},", jsdoc(f, &pad), field(k, f)))
            .collect();
        if let Some(i) = &index {
            lines.push(format!("{pad}[key: string]: {i},"));
        }
        return format!("{{\n{}\n{}}}", lines.join("\n"), "  ".repeat(depth));
    }
    if v.empty_struct() {
        return "{}".into();
    }
    "unknown".into()
}

/// A JSON Schema as a TypeScript type, `pretty` with JSDoc on its fields
/// (OpenCode's `jsonSchemaToTypeScript`). A schema too large or too
/// tangled to render within [`MAX_RENDER_NODES`] and [`MAX_TYPE_LEN`] comes
/// out partly or wholly `unknown`.
pub fn typescript(schema: &Value, pretty: bool) -> String {
    let budget = Budget::new();
    let ctx = Ctx {
        defs: Vec::new(),
        pretty,
        budget: &budget,
    };
    let out = render(View::of(schema), &ctx, 0, 0, &BTreeSet::new());
    if out.len() > MAX_TYPE_LEN {
        return "unknown".into();
    }
    out
}

/// The parameter list of a call: `()` for an empty input, otherwise
/// `(<pretty input type>)`.
pub fn parameters(schema: &Value) -> String {
    let compact = typescript(schema, false);
    if compact == "{}" {
        return "()".into();
    }
    format!("({})", typescript(schema, true))
}

/// The parameter list of a freeform tool whose input is one required
/// string field: that string, taken directly (`(patchText: string)`, with
/// the field's JSDoc inline), as in OpenCode's `stringParameter`. `None`
/// when the input is not such a field.
pub fn string_parameter(schema: &Value) -> Option<String> {
    let name = codesmith_ai::freeform_field(schema)?;
    let field = &schema["properties"][&name];
    let identifier = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    let label = if identifier { name.as_str() } else { "input" };
    let doc = jsdoc(field, "");
    let doc = doc
        .strip_suffix('\n')
        .map_or(doc.clone(), |d| format!("{d} "));
    Some(format!("({doc}{label}: {})", typescript(field, false)))
}

/// `tools.ns.name(...): Promise<...>`, as OpenCode's catalog and search
/// show a tool. `output` is the tool's output schema, when it has one.
pub fn signature(path: &str, input: &Value, output: Option<&Value>) -> String {
    let out = output.map_or_else(|| "unknown".to_string(), |o| typescript(o, true));
    format!("{path}{}: Promise<{out}>", parameters(input))
}

/// OpenCode's `packages/codemode/test/signature.test.ts`, for JSON Schema
/// tools (the Effect Schema cases have no counterpart here).
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ts(v: Value) -> String {
        typescript(&v, false)
    }
    fn pretty(v: Value) -> String {
        typescript(&v, true)
    }
    fn lines(l: &[&str]) -> String {
        l.join("\n")
    }

    fn list_issues() -> Value {
        json!({
            "type": "object",
            "properties": {
                "owner": { "type": "string", "description": "Repository owner" },
                "after": { "type": "string", "description": "Cursor from the previous response's pageInfo" },
                "perPage": { "type": "number", "description": "Results per page", "default": 30 },
                "labels": { "type": "array", "items": { "type": "string" }, "description": "Filter by labels", "minItems": 1, "maxItems": 10 },
                "state": { "type": "string", "enum": ["open", "closed"] },
            },
            "required": ["owner"],
        })
    }

    // CodeSmith's JSON objects keep their keys sorted, so fields list in
    // name order here where OpenCode keeps the schema's order.
    const LIST_ISSUES: &[&str] = &[
        "{",
        "  /** Cursor from the previous response's pageInfo */",
        "  after?: string,",
        "  /** Filter by labels. @minItems 1 @maxItems 10 */",
        "  labels?: Array<string>,",
        "  /** Repository owner */",
        "  owner: string,",
        "  /** Results per page. @default 30 */",
        "  perPage?: number,",
        "  state?: \"open\" | \"closed\",",
        "}",
    ];

    #[test]
    fn oc_described_fields_get_compact_jsdoc_undescribed_and_unconstrained_fields_get_none() {
        assert_eq!(pretty(list_issues()), lines(LIST_ISSUES));
    }

    #[test]
    fn oc_compact_mode_output_is_unchanged_by_the_pretty_machinery() {
        assert_eq!(
            ts(list_issues()),
            r#"{ after?: string; labels?: Array<string>; owner: string; perPage?: number; state?: "open" | "closed" }"#
        );
    }

    #[test]
    fn oc_nested_objects_recurse_with_increasing_indent_and_their_own_jsdoc() {
        let s = json!({"type": "object", "properties": {"filter": {"type": "object", "description": "Search filter",
            "properties": {"state": {"type": "string", "description": "Issue state"}}}}});
        assert_eq!(
            pretty(s),
            lines(&[
                "{",
                "  /** Search filter */",
                "  filter?: {",
                "    /** Issue state */",
                "    state?: string,",
                "  },",
                "}"
            ])
        );
    }

    #[test]
    fn oc_constraints_and_annotations_share_compact_tagged_jsdoc() {
        let p = pretty(json!({"type": "object", "properties": {
            "legacy": {"type": "string", "deprecated": true},
            "homepage": {"type": "string", "format": "uri"},
            "tags": {"type": "array", "items": {"type": "string"}, "minItems": 2, "maxItems": 5, "default": ["a", "b"]},
        }}));
        assert!(p.contains("  /** @deprecated */\n  legacy?: string"), "{p}");
        assert!(
            p.contains("  /** @format uri */\n  homepage?: string"),
            "{p}"
        );
        assert!(
            p.contains(
                "  /** @default [\"a\",\"b\"] @minItems 2 @maxItems 5 */\n  tags?: Array<string>"
            ),
            "{p}"
        );
    }

    #[test]
    fn oc_labels_immediate_item_and_dictionary_value_metadata_above_the_field() {
        let s = json!({"properties": {
            "recipients": {"type": "array", "description": "People to notify", "minItems": 1,
                "items": {"type": "string", "description": "Email address", "format": "email"}},
            "scores": {"type": "object", "additionalProperties": {"type": "number", "minimum": 0}},
            "names": {"type": "array", "items": {"type": "string", "description": "Display name"}},
            "plain": {"type": "object", "additionalProperties": {"type": "string"}},
        }});
        assert_eq!(
            pretty(s.clone()),
            lines(&[
                "{",
                "  /** Each item: Display name */",
                "  names?: Array<string>,",
                "  plain?: {",
                "    [key: string]: string,",
                "  },",
                "  /**",
                "   * People to notify. @minItems 1",
                "   * Each item: Email address. @format email",
                "   */",
                "  recipients?: Array<string>,",
                "  /** Each value: @minimum 0 */",
                "  scores?: {",
                "    [key: string]: number,",
                "  },",
                "}",
            ])
        );
        assert_eq!(ts(s), "{ names?: Array<string>; plain?: { [key: string]: string }; recipients?: Array<string>; scores?: { [key: string]: number } }");
    }

    #[test]
    fn oc_does_not_apply_additional_property_metadata_to_named_properties() {
        let s = json!({"properties": {"scores": {"type": "object",
            "properties": {"title": {"type": "string", "description": "Score title"}},
            "additionalProperties": {"type": "number", "minimum": 0}}}});
        assert_eq!(
            pretty(s),
            lines(&[
                "{",
                "  /** Each additional value: @minimum 0 */",
                "  scores?: {",
                "    /** Score title */",
                "    title?: string,",
                "    [key: string]: number,",
                "  },",
                "}",
            ])
        );
    }

    #[test]
    fn oc_labels_array_and_dictionary_contents_in_type_array_unions() {
        let s = json!({"properties": {
            "recipients": {"type": ["array", "null"], "items": {"type": "string", "format": "email"}},
            "scores": {"type": ["object", "null"], "additionalProperties": {"type": "number", "minimum": 0}},
            "either": {"type": ["array", "object"], "items": {"type": "string", "minLength": 1},
                "additionalProperties": {"type": "number", "minimum": 0}},
        }});
        assert_eq!(
            pretty(s),
            lines(&[
                "{",
                "  /**",
                "   * Each item: @minLength 1",
                "   * Each value: @minimum 0",
                "   */",
                "  either?: Array<string> | {",
                "      [key: string]: number,",
                "    },",
                "  /** Each item: @format email */",
                "  recipients?: Array<string> | null,",
                "  /** Each value: @minimum 0 */",
                "  scores?: {",
                "      [key: string]: number,",
                "    } | null,",
                "}",
            ])
        );
    }

    #[test]
    fn oc_keeps_multiline_item_metadata_indented_under_its_label_and_escapes_terminators() {
        let s = json!({"properties": {"values": {"type": "array", "minItems": 1,
            "items": {"type": "string", "description": "\nFirst */ line\n\nSecond line\n", "pattern": "^\\d+\n*/$"}}}});
        assert_eq!(
            pretty(s),
            lines(&[
                "{",
                "  /**",
                "   * @minItems 1",
                "   * Each item: First * / line",
                "   *",
                "   *   Second line",
                "   *   @pattern ^\\d+",
                "   *   * /$",
                "   */",
                "  values?: Array<string>,",
                "}",
            ])
        );
    }

    #[test]
    fn oc_does_not_flatten_nested_containers_reference_targets_or_branches_into_item_metadata() {
        let s = json!({
            "$defs": {"Email": {"type": "string", "format": "email"}},
            "properties": {
                "matrix": {"type": "array", "items": {"type": "array", "minItems": 2, "items": {"type": "integer", "minimum": 0}}},
                "refs": {"type": "array", "items": {"$ref": "#/$defs/Email", "description": "Recipient"}},
                "choices": {"type": "array", "items": {"anyOf": [{"type": "string", "minLength": 1}, {"type": "number", "minimum": 0}]}},
            },
        });
        assert_eq!(
            pretty(s),
            lines(&[
                "{",
                "  choices?: Array<string | number>,",
                "  /** Each item: @minItems 2 */",
                "  matrix?: Array<Array<number>>,",
                "  /** Each item: Recipient */",
                "  refs?: Array<string>,",
                "}",
            ])
        );
    }

    #[test]
    fn oc_renders_constraints_without_changing_the_compact_type() {
        let cases = [
            (
                json!({"type": "number", "minimum": 0}),
                "@minimum 0",
                "number",
            ),
            (
                json!({"type": "number", "maximum": 0}),
                "@maximum 0",
                "number",
            ),
            (
                json!({"type": "number", "exclusiveMinimum": 0}),
                "@exclusiveMinimum 0",
                "number",
            ),
            (
                json!({"type": "number", "exclusiveMaximum": 0}),
                "@exclusiveMaximum 0",
                "number",
            ),
            (
                json!({"type": "number", "multipleOf": 0.25}),
                "@multipleOf 0.25",
                "number",
            ),
            (
                json!({"type": "string", "minLength": 0}),
                "@minLength 0",
                "string",
            ),
            (
                json!({"type": "string", "maxLength": 0}),
                "@maxLength 0",
                "string",
            ),
            (
                json!({"type": "string", "pattern": "^[a-z]+$"}),
                "@pattern ^[a-z]+$",
                "string",
            ),
            (
                json!({"type": "array", "minItems": 0}),
                "@minItems 0",
                "Array<unknown>",
            ),
            (
                json!({"type": "array", "maxItems": 0}),
                "@maxItems 0",
                "Array<unknown>",
            ),
            (
                json!({"type": "array", "uniqueItems": true}),
                "@uniqueItems true",
                "Array<unknown>",
            ),
            (
                json!({"type": "string", "minLength": 0, "maxLength": 0}),
                "@minLength 0 @maxLength 0",
                "string",
            ),
            (
                json!({"type": "array", "minItems": 0, "maxItems": 0}),
                "@minItems 0 @maxItems 0",
                "Array<unknown>",
            ),
            (
                json!({"type": "array", "minItems": 1, "maxItems": 10, "uniqueItems": true}),
                "@minItems 1 @maxItems 10 @uniqueItems true",
                "Array<unknown>",
            ),
        ];
        for (value, summary, ty) in cases {
            let s = json!({"type": "object", "properties": {"value": value}});
            assert_eq!(
                pretty(s.clone()),
                lines(&[
                    "{",
                    &format!("  /** {summary} */"),
                    &format!("  value?: {ty},"),
                    "}"
                ])
            );
            assert_eq!(ts(s), format!("{{ value?: {ty} }}"));
        }
    }

    #[test]
    fn oc_documents_integer_numbers_without_adding_redundant_types_or_requiring_uniqueness_when_false(
    ) {
        let s = json!({"type": "object", "properties": {
            "count": {"type": "integer"}, "amount": {"type": "number"}, "name": {"type": "string"},
            "enabled": {"type": "boolean"}, "values": {"type": "array", "uniqueItems": false},
            "choice": {"type": ["integer", "string"]},
        }});
        assert_eq!(
            pretty(s),
            lines(&[
                "{",
                "  amount?: number,",
                "  choice?: number | string,",
                "  /** @integer */",
                "  count?: number,",
                "  enabled?: boolean,",
                "  name?: string,",
                "  values?: Array<unknown>,",
                "}",
            ])
        );
    }

    #[test]
    fn oc_preserves_falsy_defaults_alongside_constraints() {
        for (v, shown) in [
            (json!(false), "false"),
            (json!(null), "null"),
            (json!(""), "\"\""),
        ] {
            let p = pretty(json!({"properties": {"value": {"default": v, "minLength": 0}}}));
            assert!(
                p.contains(&format!("  /** @default {shown} @minLength 0 */\n")),
                "{p}"
            );
        }
    }

    #[test]
    fn oc_escapes_comment_terminators_in_summaries_and_descriptions() {
        assert_eq!(
            pretty(
                json!({"properties": {"value": {"type": "string", "default": "*/", "format": "*/", "pattern": "^a*/b$"}}})
            ),
            lines(&[
                "{",
                "  /** @default \"* /\" @format * / @pattern ^a* /b$ */",
                "  value?: string,",
                "}"
            ])
        );
        let p = pretty(
            json!({"type": "object", "properties": {"note": {"type": "string", "description": "Ends */ early"}}}),
        );
        assert!(
            p.contains("  /** Ends * / early */") && !p.contains("Ends */"),
            "{p}"
        );
    }

    #[test]
    fn oc_multiline_descriptions_become_prefixed_blocks_with_blank_edges_trimmed() {
        let s = json!({"type": "object", "properties": {"query": {"type": "string", "description": "\nFirst line\n\nSecond line\n"}}});
        assert_eq!(
            pretty(s),
            lines(&[
                "{",
                "  /**",
                "   * First line",
                "   *",
                "   * Second line",
                "   */",
                "  query?: string,",
                "}"
            ])
        );
    }

    #[test]
    fn oc_preserves_inclusive_and_exclusive_numeric_bounds_together() {
        let p = pretty(
            json!({"properties": {"value": {"type": "integer", "minimum": -10, "maximum": 10,
            "exclusiveMinimum": -5, "exclusiveMaximum": 5, "multipleOf": 2}}}),
        );
        assert!(p.contains("  /** @integer @minimum -10 @maximum 10 @exclusiveMinimum -5 @exclusiveMaximum 5 @multipleOf 2 */\n  value?: number,"), "{p}");
    }

    #[test]
    fn oc_combines_a_short_description_with_its_summary() {
        for (d, shown) in [
            ("Maximum attempts", "Maximum attempts."),
            ("Maximum attempts.", "Maximum attempts."),
            ("Maximum attempts!", "Maximum attempts!."),
        ] {
            let p = pretty(
                json!({"properties": {"attempts": {"description": d, "type": "integer", "minimum": 1, "default": 3}}}),
            );
            assert!(
                p.contains(&format!(
                    "  /** {shown} @default 3 @integer @minimum 1 */\n"
                )),
                "{p}"
            );
        }
    }

    #[test]
    fn oc_keeps_multiline_descriptions_intact_and_appends_a_compact_summary() {
        let s = json!({"properties": {"attempts": {"description": "\nMaximum attempts\n\nIncludes the initial request.\n", "type": "integer", "minimum": 1}}});
        assert_eq!(
            pretty(s),
            lines(&[
                "{",
                "  /**",
                "   * Maximum attempts",
                "   *",
                "   * Includes the initial request.",
                "   * @integer @minimum 1",
                "   */",
                "  attempts?: number,",
                "}",
            ])
        );
    }

    #[test]
    fn oc_uses_a_block_for_long_descriptions_without_truncating_or_rewriting_them() {
        let d = "A detailed description. ".repeat(8).trim().to_string();
        let s =
            json!({"properties": {"name": {"type": "string", "description": d, "minLength": 1}}});
        assert_eq!(
            pretty(s),
            lines(&[
                "{",
                "  /**",
                &format!("   * {d}"),
                "   * @minLength 1",
                "   */",
                "  name?: string,",
                "}"
            ])
        );
    }

    #[test]
    fn oc_preserves_pattern_backslashes_and_prefixes_every_line_of_multiline_summary_values() {
        let s = json!({"properties": {"value": {"type": "string", "pattern": "^\\d+\n*/$", "default": "a\nb"}}});
        assert_eq!(
            pretty(s),
            lines(&[
                "{",
                "  /**",
                "   * @default \"a\\nb\" @pattern ^\\d+",
                "   * * /$",
                "   */",
                "  value?: string,",
                "}"
            ])
        );
    }

    #[test]
    fn oc_stays_total_on_cyclic_refs_and_pathological_nesting_in_both_modes() {
        let cyclic = json!({"$ref": "#/$defs/Node", "$defs": {"Node": {"type": "object",
            "properties": {"child": {"$ref": "#/$defs/Node"}, "name": {"type": "string"}}}}});
        assert_eq!(ts(cyclic.clone()), "{ child?: unknown; name?: string }");
        assert!(pretty(cyclic).contains("child?: unknown"));
        let mut deep = json!({"type": "string"});
        for _ in 0..12 {
            deep = json!({"type": "object", "properties": {"next": deep}});
        }
        for p in [false, true] {
            let r = typescript(&deep, p);
            assert!(r.contains("unknown") && r.contains("next?:"), "{r}");
        }
    }

    #[test]
    fn oc_intersects_ref_and_union_siblings_instead_of_discarding_them() {
        assert_eq!(
            ts(
                json!({"$ref": "#/$defs/User", "properties": {"active": {"type": "boolean"}}, "required": ["active"],
                "$defs": {"User": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}}})
            ),
            "{ id: string } & { active: boolean }"
        );
        assert_eq!(
            ts(
                json!({"type": "object", "properties": {"common": {"type": "boolean"}}, "required": ["common"],
                "anyOf": [
                    {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]},
                    {"type": "object", "properties": {"count": {"type": "number"}}, "required": ["count"]},
                ]})
            ),
            "({ name: string } | { count: number }) & { common: boolean }"
        );
        assert_eq!(
            ts(json!({"$ref": "https://example.com/schema.json"})),
            "unknown"
        );
        assert_eq!(
            ts(
                json!({"$ref": "#/$defs/User/properties/id", "$defs": {"User": {"type": "object"}, "id": {"type": "string"}}})
            ),
            "unknown"
        );
        assert_eq!(
            ts(json!({"type": ["object", "null"], "properties": {"name": {"type": "string"}}})),
            "{ name?: string } | null"
        );
    }

    #[test]
    fn oc_keeps_malformed_unresolved_and_recursive_references_unknown() {
        for r in [
            "#/$defs/%",
            "#/$defs/A~2B",
            "#/$defs/Missing",
            "#/$defs/Loop",
            "https://example.test/schema",
        ] {
            assert_eq!(
                ts(json!({"$ref": r, "$defs": {"Loop": {"$ref": "#/$defs/Loop"}}})),
                "unknown",
                "{r}"
            );
        }
    }

    #[test]
    fn oc_resolves_root_definitions_and_lets_defs_take_precedence() {
        for k in ["definitions", "$defs"] {
            let mut s = json!({"$ref": format!("#/{k}/Value")});
            s[k] = json!({"Value": {"type": "string"}});
            assert_eq!(ts(s.clone()), "string");
            assert_eq!(pretty(s.clone()), "string");
            s["$defs"] = json!({"Value": {"type": "number"}});
            assert_eq!(ts(s.clone()), "number");
            assert_eq!(pretty(s), "number");
        }
    }

    #[test]
    fn oc_nested_definitions_shadow_inherited_definitions_without_affecting_siblings() {
        for k in ["definitions", "$defs"] {
            let mut nested = json!({"$ref": format!("#/{k}/Value")});
            nested[k] = json!({"Value": {"type": "boolean"}});
            let s = json!({"type": "object", "definitions": {"Inherited": {"type": "string"}},
                "$defs": {"Value": {"type": "number"}},
                "properties": {"nested": nested, "inherited": {"$ref": "#/definitions/Inherited"}, "sibling": {"$ref": "#/$defs/Value"}}});
            assert_eq!(
                ts(s.clone()),
                "{ inherited?: string; nested?: boolean; sibling?: number }"
            );
            assert_eq!(
                pretty(s),
                lines(&[
                    "{",
                    "  inherited?: string,",
                    "  nested?: boolean,",
                    "  sibling?: number,",
                    "}"
                ])
            );
        }
    }

    #[test]
    fn oc_non_identifier_property_names_render_as_quoted_keys() {
        let s = json!({"type": "object", "properties": {
            "123": {"type": "number"},
            "foo-bar": {"type": "string"},
            "@type": {"type": "string"},
            "x.y": {"type": "number", "description": "Dotted name"},
            "plain": {"type": "boolean"},
        }, "required": ["@type"]});
        assert_eq!(
            ts(s.clone()),
            r#"{ "123"?: number; "@type": string; "foo-bar"?: string; plain?: boolean; "x.y"?: number }"#
        );
        assert_eq!(
            pretty(s),
            lines(&[
                "{",
                "  \"123\"?: number,",
                "  \"@type\": string,",
                "  \"foo-bar\"?: string,",
                "  plain?: boolean,",
                "  /** Dotted name */",
                "  \"x.y\"?: number,",
                "}",
            ])
        );
        let out = json!({"type": "object", "properties": {"content-type": {"type": "string"}}, "required": ["content-type"]});
        assert_eq!(ts(out.clone()), r#"{ "content-type": string }"#);
        assert_eq!(
            pretty(out),
            lines(&["{", "  \"content-type\": string,", "}"])
        );
    }

    #[test]
    fn oc_union_schemas_render_every_alternative() {
        let lit = ["NaN", "Infinity", "-Infinity"];
        assert_eq!(
            ts(json!({"anyOf": [{"type": "number"}, {"type": "string", "enum": lit}]})),
            "number"
        );
        assert_eq!(
            ts(
                json!({"anyOf": [{"type": "number"}, {"type": "string", "enum": ["NaN"]},
                {"type": "string", "enum": ["Infinity"]}, {"type": "string", "enum": ["-Infinity"]}]})
            ),
            "number"
        );
        assert_eq!(
            ts(
                json!({"anyOf": [{"type": "number"}, {"type": "string", "enum": ["NaN", "Infinity", "unknown"]}]})
            ),
            r#"number | "NaN" | "Infinity" | "unknown""#
        );
        let s = json!({"anyOf": [{"type": "string"}, {"type": "number"}]});
        assert_eq!(ts(s.clone()), "string | number");
        assert_eq!(pretty(s), "string | number");
        let s = json!({"oneOf": [{"type": "number"}, {"type": "null"}]});
        assert_eq!(ts(s.clone()), "number | null");
        assert_eq!(pretty(s), "number | null");
        assert_eq!(
            ts(
                json!({"type": "object", "properties": {"value": {"anyOf": [{"type": "string"}, {"type": "number"}]}}})
            ),
            "{ value?: string | number }"
        );
        assert_eq!(
            ts(json!({"anyOf": [{"type": "number"}, {"type": "boolean"}]})),
            "number | boolean"
        );
    }

    #[test]
    fn oc_all_of_keeps_siblings_and_parenthesized_union_members_in_order() {
        let s = json!({"properties": {"common": {"type": "boolean"}},
            "allOf": [{"type": "object", "properties": {"id": {"type": "string"}}}, {"type": ["string", "null"]}]});
        assert_eq!(
            ts(s.clone()),
            "{ common?: boolean } & { id?: string } & (string | null)"
        );
        assert_eq!(
            pretty(s),
            lines(&[
                "{",
                "    common?: boolean,",
                "  } & {",
                "    id?: string,",
                "  } & (string | null)"
            ])
        );
    }

    #[test]
    fn oc_all_of_does_not_discard_an_unresolved_constraint() {
        for p in [false, true] {
            for r in [
                "#/$defs/Missing",
                "#/definitions/Missing",
                "https://example.com/external.json",
            ] {
                let r = json!({ "$ref": r });
                for s in [
                    json!({"allOf": [{"type": "string"}, r]}),
                    json!({"allOf": [{"type": "string"}, {"allOf": [r]}]}),
                    json!({"allOf": [{"properties": {"nested": r}}, {"type": "string"}]}),
                ] {
                    assert_eq!(typescript(&s, p), "unknown", "{s}");
                }
            }
            let s = json!({"type": "string", "allOf": [{"$ref": "#/$defs/Constraint"}],
                "$defs": {"Constraint": {"description": "TypeScript-neutral constraint"}}});
            assert_eq!(typescript(&s, p), "string");
        }
    }

    #[test]
    fn oc_raw_schemas_render_only_the_exact_empty_struct_shapes_as_empty() {
        assert_eq!(ts(json!({"not": {"type": "null"}})), "{}");
        assert_eq!(
            ts(json!({"anyOf": [{"type": "object"}, {"type": "array"}]})),
            "{}"
        );
        assert_eq!(
            ts(json!({"type": "string", "not": {"type": "null"}})),
            "string"
        );
        assert_eq!(ts(json!({"not": {"type": "string"}})), "unknown");
        assert_eq!(
            ts(json!({"not": {"type": "null", "description": "Not null"}})),
            "unknown"
        );
    }

    #[test]
    fn oc_constraints_survive_input_and_output_signatures() {
        let s = json!({"type": "object", "properties": {
            "count": {"type": "integer", "minimum": 0, "maximum": 10},
            "name": {"type": "string", "minLength": 1, "maxLength": 20, "pattern": "^[a-z]+$"},
            "labels": {"type": "array", "items": {"type": "string", "minLength": 1}, "minItems": 1, "maxItems": 5},
            "scores": {"type": "object", "additionalProperties": {"type": "integer", "minimum": 0}},
        }, "required": ["count", "name", "labels", "scores"]});
        let ty = lines(&[
            "{",
            "  /** @integer @minimum 0 @maximum 10 */",
            "  count: number,",
            "  /**",
            "   * @minItems 1 @maxItems 5",
            "   * Each item: @minLength 1",
            "   */",
            "  labels: Array<string>,",
            "  /** @minLength 1 @maxLength 20 @pattern ^[a-z]+$ */",
            "  name: string,",
            "  /** Each value: @integer @minimum 0 */",
            "  scores: {",
            "    [key: string]: number,",
            "  },",
            "}",
        ]);
        assert_eq!(
            signature("tools.constrained", &s, Some(&s)),
            format!("tools.constrained({ty}): Promise<{ty}>")
        );
    }

    #[test]
    fn oc_a_raw_json_schema_tool_signature_carries_field_jsdoc_and_summaries() {
        let mut want = LIST_ISSUES.to_vec();
        want[0] = "tools.github.list_issues({";
        let last = want.len() - 1;
        want[last] = "}): Promise<unknown>";
        assert_eq!(
            signature("tools.github.list_issues", &list_issues(), Some(&json!({}))),
            lines(&want)
        );
    }

    #[test]
    fn oc_empty_inputs_advertise_a_zero_argument_call() {
        assert_eq!(
            signature(
                "tools.ping",
                &json!({"type": "object", "properties": {}}),
                Some(&json!({"type": "string"}))
            ),
            "tools.ping(): Promise<string>"
        );
        assert_ne!(
            parameters(&json!({"type": "object", "additionalProperties": {}})),
            "()"
        );
    }

    #[test]
    fn oc_catalog_signatures_use_bracket_notation_for_dashed_tool_names() {
        let s = json!({"type": "object", "properties": {"query": {"type": "string"}, "libraryName": {"type": "string"}},
            "required": ["query", "libraryName"]});
        assert_eq!(
            signature(r#"tools.context7["resolve-library-id"]"#, &s, Some(&json!({}))),
            "tools.context7[\"resolve-library-id\"]({\n  libraryName: string,\n  query: string,\n}): Promise<unknown>"
        );
    }

    /// `levels` definitions, each an `anyOf` of `k` references to the next.
    fn fan(k: usize, levels: usize) -> Value {
        let mut defs = Map::new();
        for l in 0..levels {
            let refs: Vec<_> = (0..k)
                .map(|_| json!({"$ref": format!("#/$defs/D{}", l + 1)}))
                .collect();
            defs.insert(format!("D{l}"), json!({"anyOf": refs}));
        }
        defs.insert(format!("D{levels}"), json!({"type": "string"}));
        json!({"type": "object", "properties": {"a": {"$ref": "#/$defs/D0"}}, "$defs": defs})
    }

    fn chain(n: usize) -> Value {
        let mut defs = Map::new();
        for i in 0..n {
            defs.insert(
                format!("D{i}"),
                json!({"$ref": format!("#/$defs/D{}", i + 1)}),
            );
        }
        defs.insert(format!("D{n}"), json!({"type": "string"}));
        json!({"$ref": "#/$defs/D0", "$defs": defs})
    }

    #[test]
    fn a_long_reference_chain_renders_on_a_small_stack() {
        // Unbounded, this recursed once per link and overflowed the stack.
        let s = chain(5_000);
        let out = std::thread::Builder::new()
            .stack_size(512 * 1024)
            .spawn(move || (typescript(&s, false), typescript(&s, true)))
            .unwrap()
            .join()
            .expect("rendering must not overflow");
        assert_eq!(out, ("unknown".to_string(), "unknown".to_string()));
        // Short chains still resolve.
        assert_eq!(ts(chain(3)), "string");
    }

    #[test]
    fn reference_fan_out_is_bounded() {
        for p in [false, true] {
            let start = std::time::Instant::now();
            let out = typescript(&fan(9, 8), p);
            assert!(start.elapsed() < std::time::Duration::from_secs(2));
            assert!(out.len() <= MAX_TYPE_LEN, "{}", out.len());
        }
        // A small fan-out renders fully.
        assert_eq!(ts(fan(2, 2)), "{ a?: string | string | string | string }");
    }

    #[test]
    fn all_of_reference_checks_share_the_budget() {
        let mut s = fan(9, 8);
        s["properties"]["a"] = json!({"allOf": [{"$ref": "#/$defs/D0"}]});
        let start = std::time::Instant::now();
        let _ = typescript(&s, true);
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn an_oversized_type_is_unknown() {
        let big = "x".repeat(MAX_TYPE_LEN);
        assert_eq!(ts(json!({"const": big})), "unknown");
        let props: Map<String, Value> = (0..20_000)
            .map(|i| (format!("p{i}"), json!({"type": "string"})))
            .collect();
        let out = ts(json!({"type": "object", "properties": props}));
        assert!(out.len() <= MAX_TYPE_LEN);
    }
}
