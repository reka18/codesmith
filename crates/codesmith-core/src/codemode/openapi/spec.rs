//! From an OpenAPI 3.x document to Code Mode tools: one tool per operation
//! that can be called correctly, and the reason for each one that cannot.
//! Ported from OpenCode's `packages/codemode/src/codemode/openapi/spec.ts`
//! and `index.ts` (MIT).
//!
//! Schemas are projected by direction as OpenCode does: request schemas
//! omit `readOnly` properties and response schemas omit `writeOnly` ones,
//! with `required` kept consistent, through `$ref` chains and `allOf`
//! composition (cycles solved with Tarjan's algorithm so diamond-shaped
//! graphs stay linear). Component schemas become `$defs`; unlike OpenCode,
//! a tool carries only the definitions its schema reaches, so a large API
//! does not repeat every component in every tool.

use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};

const METHODS: [&str; 8] = [
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];
const PARAMETER_LOCATIONS: [Location; 3] = [Location::Path, Location::Query, Location::Header];
/// Header parameters a tool never takes from the model: what the request
/// sets itself (OpenCode's three), and, in CodeSmith, framing and routing
/// headers and cookies, which could retarget or smuggle a request.
const IGNORED_HEADERS: [&str; 11] = [
    "accept",
    "content-type",
    "authorization",
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "cookie",
    "proxy-authorization",
    "te",
    "upgrade",
];
/// How deep `$ref` and `allOf` chains are followed while projecting; past
/// it a schema counts as not hidden (a guard against a crafted document
/// overflowing the stack).
const MAX_DEPTH: usize = 256;

/// The operation identity handed to auth resolution and errors.
#[derive(Debug, Clone, PartialEq)]
pub struct Operation {
    pub operation_id: Option<String>,
    /// Upper case.
    pub method: String,
    pub path: String,
    pub summary: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Carrier {
    Header,
    Query,
    Cookie,
}

/// A security scheme from `components.securitySchemes`.
#[derive(Debug, Clone, PartialEq)]
pub enum Scheme {
    ApiKey { name: String, carrier: Carrier },
    Http { scheme: String },
    OAuth2,
    OpenIdConnect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    Path,
    Query,
    Header,
    Body,
}

impl Location {
    fn name(self) -> &'static str {
        match self {
            Location::Path => "path",
            Location::Query => "query",
            Location::Header => "header",
            Location::Body => "body",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Simple,
    Form,
    DeepObject,
}

/// One input of an operation.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    /// The name the model passes it under.
    pub input_name: String,
    /// Its name in the request.
    pub name: String,
    pub location: Location,
    pub required: bool,
    pub schema: Value,
    pub style: Option<Style>,
    pub explode: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyMode {
    /// The body's properties are the tool's fields.
    Object,
    /// The body is the tool's `body` field.
    Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Body {
    pub required: bool,
    pub mode: BodyMode,
    pub media_type: String,
}

/// One alternative of a security requirement: scheme names and scopes.
pub type Requirement = Vec<(String, Vec<String>)>;

/// What a call needs to build its request.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub operation: Operation,
    /// The server URL and the path template.
    pub url: String,
    pub fields: Vec<Field>,
    pub body: Option<Body>,
    pub security: Vec<Requirement>,
    pub schemes: BTreeMap<String, Scheme>,
}

/// One generated tool.
#[derive(Debug, Clone, PartialEq)]
pub struct Tool {
    /// Its path under the namespace, split at dots (`["users", "get"]`).
    pub path: Vec<String>,
    pub description: String,
    pub input: Value,
    pub output: Option<Value>,
    pub plan: Plan,
}

impl Tool {
    pub fn dotted(&self) -> String {
        self.path.join(".")
    }
}

/// An operation that could not be represented as a tool, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Skipped {
    pub method: String,
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Generated {
    pub tools: Vec<Tool>,
    pub skipped: Vec<Skipped>,
}

impl Generated {
    /// The tool at a dotted path.
    pub fn tool(&self, dotted: &str) -> Option<&Tool> {
        self.tools.iter().find(|t| t.dotted() == dotted)
    }
}

type Parsed<T> = Result<T, String>;

fn non_empty(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn as_array(v: Option<&Value>) -> &[Value] {
    v.and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

fn pointer<'a>(root: &'a Value, r: &str) -> Option<&'a Value> {
    let mut item = root;
    for segment in r[2..].split('/') {
        let segment = segment.replace("~1", "/").replace("~0", "~");
        item = item.as_object()?.get(&segment)?;
    }
    Some(item)
}

/// Follow local `$ref`s (`#/...`) until a value without one, stopping at a
/// cycle or a dangling reference.
pub fn resolve<'a>(document: &'a Value, value: &'a Value) -> &'a Value {
    let mut current = value;
    let mut seen = BTreeSet::new();
    loop {
        let Some(r) = non_empty(current.as_object().and_then(|o| o.get("$ref"))) else {
            return current;
        };
        if !r.starts_with("#/") || !seen.insert(r.clone()) {
            return current;
        }
        match pointer(document, &r) {
            Some(target) => current = target,
            None => return current,
        }
    }
}

// ----- directional projection ----------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Request,
    Response,
}

impl Direction {
    fn keyword(self) -> &'static str {
        match self {
            Direction::Request => "readOnly",
            Direction::Response => "writeOnly",
        }
    }
}

#[derive(Clone, Copy)]
struct Resource<'a> {
    value: &'a Value,
    root: &'a Value,
}

fn key(v: &Value) -> usize {
    v as *const Value as usize
}

/// Memoized results with Tarjan bookkeeping for one question.
struct Solver<T> {
    values: HashMap<usize, T>,
    pending: HashMap<usize, usize>,
    stack: Vec<usize>,
}

impl<T> Default for Solver<T> {
    fn default() -> Self {
        Solver {
            values: HashMap::new(),
            pending: HashMap::new(),
            stack: Vec::new(),
        }
    }
}

/// Whether the document uses `readOnly` or `writeOnly` anywhere: most do
/// not, and then projection is skipped.
pub fn has_directional(v: &Value) -> bool {
    match v {
        Value::Array(a) => a.iter().any(has_directional),
        Value::Object(o) => {
            o.get("readOnly") == Some(&Value::Bool(true))
                || o.get("writeOnly") == Some(&Value::Bool(true))
                || o.values().any(has_directional)
        }
        _ => false,
    }
}

struct Projector<'a> {
    document: &'a Value,
    direction: Direction,
    hidden: Solver<bool>,
    names: Solver<BTreeSet<String>>,
}

const NESTED: [&str; 6] = [
    "items",
    "additionalProperties",
    "unevaluatedProperties",
    "propertyNames",
    "then",
    "else",
];
const NESTED_LISTS: [&str; 3] = ["anyOf", "oneOf", "prefixItems"];
const NESTED_MAPS: [&str; 4] = [
    "patternProperties",
    "dependentSchemas",
    "$defs",
    "definitions",
];

impl<'a> Projector<'a> {
    fn new(document: &'a Value, direction: Direction) -> Self {
        Projector {
            document,
            direction,
            hidden: Solver::default(),
            names: Solver::default(),
        }
    }

    /// One `$ref` hop: local `$defs`/`definitions` pointers resolve against
    /// the schema being projected, others rebase onto their target.
    fn hop(&self, r: Resource<'a>) -> Resource<'a> {
        let Some(reference) = non_empty(r.value.as_object().and_then(|o| o.get("$ref"))) else {
            return r;
        };
        if !reference.starts_with("#/") {
            return r;
        }
        let local = reference.starts_with("#/$defs/") || reference.starts_with("#/definitions/");
        match pointer(if local { r.root } else { self.document }, &reference) {
            Some(target) => Resource {
                value: target,
                root: if local { r.root } else { target },
            },
            None => r,
        }
    }

    /// Whether a schema is hidden in this direction: declared so itself, or
    /// through `allOf` or the `$ref` it points at.
    fn is_hidden(&mut self, r: Resource<'a>, scope: &mut usize, depth: usize) -> bool {
        let Some(o) = r.value.as_object() else {
            return false;
        };
        if o.get(self.direction.keyword()) == Some(&Value::Bool(true)) {
            return true;
        }
        if depth > MAX_DEPTH {
            return false;
        }
        let k = key(r.value);
        if let Some(v) = self.hidden.values.get(&k) {
            return *v;
        }
        if let Some(&p) = self.hidden.pending.get(&k) {
            *scope = (*scope).min(p);
            return false;
        }
        let index = self.hidden.pending.len();
        let base = self.hidden.stack.len();
        self.hidden.pending.insert(k, index);
        self.hidden.stack.push(k);
        let mut inner = usize::MAX;
        let mut value = false;
        for item in as_array(o.get("allOf")) {
            if self.is_hidden(
                Resource {
                    value: item,
                    root: r.root,
                },
                &mut inner,
                depth + 1,
            ) {
                value = true;
                break;
            }
        }
        if !value {
            let target = self.hop(r);
            if !std::ptr::eq(target.value, r.value) {
                value = self.is_hidden(target, &mut inner, depth + 1);
            }
        }
        if inner < index {
            *scope = (*scope).min(inner);
            return value;
        }
        for m in self.hidden.stack.split_off(base) {
            self.hidden.pending.remove(&m);
            self.hidden.values.insert(m, value);
        }
        value
    }

    /// Hidden property names a schema declares or inherits through `$ref`
    /// and `allOf`, so `required` lists stay consistent.
    fn hidden_names(
        &mut self,
        r: Resource<'a>,
        scope: &mut usize,
        depth: usize,
    ) -> BTreeSet<String> {
        let Some(o) = r.value.as_object() else {
            return BTreeSet::new();
        };
        if depth > MAX_DEPTH {
            return BTreeSet::new();
        }
        let k = key(r.value);
        if let Some(v) = self.names.values.get(&k) {
            return v.clone();
        }
        if let Some(&p) = self.names.pending.get(&k) {
            *scope = (*scope).min(p);
            return BTreeSet::new();
        }
        let index = self.names.pending.len();
        let base = self.names.stack.len();
        self.names.pending.insert(k, index);
        self.names.stack.push(k);
        let mut inner = usize::MAX;
        let mut value = BTreeSet::new();
        if let Some(props) = o.get("properties").and_then(Value::as_object) {
            for (name, property) in props {
                let mut fresh = usize::MAX;
                if self.is_hidden(
                    Resource {
                        value: property,
                        root: r.root,
                    },
                    &mut fresh,
                    depth + 1,
                ) {
                    value.insert(name.clone());
                }
            }
        }
        for item in as_array(o.get("allOf")) {
            let names = self.hidden_names(
                Resource {
                    value: item,
                    root: r.root,
                },
                &mut inner,
                depth + 1,
            );
            value.extend(names);
        }
        let target = self.hop(r);
        if !std::ptr::eq(target.value, r.value) {
            let names = self.hidden_names(target, &mut inner, depth + 1);
            value.extend(names);
        }
        if inner < index {
            *scope = (*scope).min(inner);
            return value;
        }
        for m in self.names.stack.split_off(base) {
            self.names.pending.remove(&m);
            self.names.values.insert(m, value.clone());
        }
        value
    }

    /// The schema with hidden properties removed. `not`, `if` and
    /// `contains` pass through: they negate or select rather than assert.
    fn project(&mut self, r: Resource<'a>, excluded: &BTreeSet<String>) -> Value {
        let Some(o) = r.value.as_object() else {
            return r.value.clone();
        };
        let mut hidden = excluded.clone();
        let mut scope = usize::MAX;
        hidden.extend(self.hidden_names(r, &mut scope, 0));
        let none = BTreeSet::new();
        let mut out = Map::new();
        for (k, item) in o {
            let sub = |value: &'a Value| Resource {
                value,
                root: r.root,
            };
            let v = match (k.as_str(), item) {
                ("properties", Value::Object(props)) => Value::Object(
                    props
                        .iter()
                        .filter(|(name, _)| !hidden.contains(*name))
                        .map(|(name, p)| (name.clone(), self.project(sub(p), &none)))
                        .collect(),
                ),
                ("required", Value::Array(names)) => Value::Array(
                    names
                        .iter()
                        .filter(|n| n.as_str().is_none_or(|n| !hidden.contains(n)))
                        .cloned()
                        .collect(),
                ),
                // allOf branches describe one object: hidden names apply to each.
                ("allOf", Value::Array(items)) => Value::Array(
                    items
                        .iter()
                        .map(|i| self.project(sub(i), &hidden))
                        .collect(),
                ),
                (k, _) if NESTED.contains(&k) => self.project(sub(item), &none),
                (k, Value::Array(items)) if NESTED_LISTS.contains(&k) => {
                    Value::Array(items.iter().map(|i| self.project(sub(i), &none)).collect())
                }
                (k, Value::Object(map)) if NESTED_MAPS.contains(&k) => Value::Object(
                    map.iter()
                        .map(|(n, i)| (n.clone(), self.project(sub(i), &none)))
                        .collect(),
                ),
                _ => item.clone(),
            };
            out.insert(k.clone(), v);
        }
        Value::Object(out)
    }
}

// ----- normalization --------------------------------------------------------

/// OpenAPI's schema dialect as JSON Schema (OpenCode uses Effect's
/// `fromSchemaOpenApi3_0`/`3_1` for this): component references point at
/// `$defs`, and for 3.0 `nullable` becomes a `null` type and boolean
/// `exclusiveMinimum`/`exclusiveMaximum` become numbers.
fn normalize(v3_0: bool, value: &Value) -> Value {
    fn walk(v3_0: bool, value: &Value, depth: usize) -> Value {
        match value {
            Value::Array(a) => Value::Array(a.iter().map(|i| walk(v3_0, i, depth + 1)).collect()),
            Value::Object(o) if depth < 512 => {
                let mut out: Map<String, Value> = o
                    .iter()
                    .map(|(k, v)| {
                        let v = match (k.as_str(), v) {
                            ("$ref", Value::String(r)) => Value::String(
                                r.strip_prefix("#/components/schemas/")
                                    .map_or_else(|| r.clone(), |rest| format!("#/$defs/{rest}")),
                            ),
                            // Values, not schemas.
                            ("enum" | "const" | "default" | "example" | "examples", v) => v.clone(),
                            // Names to schemas: each value is a schema.
                            (
                                "properties" | "patternProperties" | "$defs" | "definitions"
                                | "dependentSchemas",
                                Value::Object(m),
                            ) => Value::Object(
                                m.iter()
                                    .map(|(n, s)| (n.clone(), walk(v3_0, s, depth + 1)))
                                    .collect(),
                            ),
                            _ => walk(v3_0, v, depth + 1),
                        };
                        (k.clone(), v)
                    })
                    .collect();
                if v3_0 {
                    for (flag, bound) in [
                        ("exclusiveMinimum", "minimum"),
                        ("exclusiveMaximum", "maximum"),
                    ] {
                        if let Some(Value::Bool(b)) = out.get(flag).cloned() {
                            out.remove(flag);
                            if b {
                                if let Some(n) = out.remove(bound) {
                                    out.insert(flag.into(), n);
                                }
                            }
                        }
                    }
                    if out.remove("nullable") == Some(Value::Bool(true)) {
                        match out.get("type").cloned() {
                            Some(Value::String(t)) => {
                                out.insert("type".into(), json!([t, "null"]));
                                if let Some(Value::Array(e)) = out.get_mut("enum") {
                                    if !e.contains(&Value::Null) {
                                        e.push(Value::Null);
                                    }
                                }
                            }
                            Some(Value::Array(mut ts)) => {
                                if !ts.contains(&json!("null")) {
                                    ts.push(json!("null"));
                                }
                                out.insert("type".into(), Value::Array(ts));
                            }
                            _ => {
                                return json!({"anyOf": [Value::Object(out), {"type": "null"}]});
                            }
                        }
                    }
                }
                Value::Object(out)
            }
            v => v.clone(),
        }
    }
    if !value.is_object() {
        return json!({});
    }
    walk(v3_0, value, 0)
}

/// `#/$defs/<name>` references anywhere in `v`.
fn references(v: &Value, out: &mut BTreeSet<String>) {
    match v {
        Value::Array(a) => a.iter().for_each(|i| references(i, out)),
        Value::Object(o) => {
            if let Some(Value::String(r)) = o.get("$ref") {
                if let Some(name) = r.strip_prefix("#/$defs/") {
                    let name = name.split('/').next().unwrap_or(name);
                    out.insert(name.replace("~1", "/").replace("~0", "~"));
                }
            }
            o.values().for_each(|i| references(i, out));
        }
        _ => {}
    }
}

/// `schema` with the component definitions it reaches added to its
/// `$defs` (its own definitions win).
fn with_definitions(mut schema: Value, definitions: &BTreeMap<String, Value>) -> Value {
    if definitions.is_empty() {
        return schema;
    }
    let mut reached = BTreeSet::new();
    let mut queue = BTreeSet::new();
    references(&schema, &mut queue);
    while let Some(name) = queue.pop_first() {
        if !reached.insert(name.clone()) {
            continue;
        }
        if let Some(d) = definitions.get(&name) {
            let mut more = BTreeSet::new();
            references(d, &mut more);
            queue.extend(more.into_iter().filter(|n| !reached.contains(n)));
        }
    }
    let used: Map<String, Value> = reached
        .into_iter()
        .filter_map(|n| definitions.get(&n).map(|d| (n, d.clone())))
        .collect();
    if used.is_empty() {
        return schema;
    }
    let Some(o) = schema.as_object_mut() else {
        return schema;
    };
    let mut defs = used;
    if let Some(Value::Object(local)) = o.get("$defs") {
        for (k, v) in local {
            defs.insert(k.clone(), v.clone());
        }
    }
    o.insert("$defs".into(), Value::Object(defs));
    schema
}

struct Context<'a> {
    document: &'a Value,
    v3_0: bool,
    directional: bool,
    request: Projector<'a>,
    response: Projector<'a>,
}

impl<'a> Context<'a> {
    fn projector(&mut self, d: Direction) -> &mut Projector<'a> {
        match d {
            Direction::Request => &mut self.request,
            Direction::Response => &mut self.response,
        }
    }

    fn project_raw(&mut self, value: &'a Value, d: Direction) -> Value {
        if !self.directional {
            return value.clone();
        }
        self.projector(d)
            .project(Resource { value, root: value }, &BTreeSet::new())
    }

    fn project(&mut self, value: &'a Value, d: Direction) -> Value {
        let v = self.project_raw(value, d);
        normalize(self.v3_0, &v)
    }

    fn definitions(&mut self, d: Direction) -> BTreeMap<String, Value> {
        let doc = self.document;
        doc.pointer("/components/schemas")
            .and_then(Value::as_object)
            .map(|schemas| {
                schemas
                    .iter()
                    .map(|(name, v)| (name.clone(), self.project(v, d)))
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn media(media_type: &str) -> String {
    media_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

pub fn is_json_media(media_type: &str) -> bool {
    let m = media(media_type);
    m == "application/json" || m.ends_with("+json")
}

fn is_binary_media(document: &Value, media_type: &str, value: &Value) -> bool {
    let m = media(media_type);
    if !is_json_media(&m) && !m.starts_with("text/") {
        return true;
    }
    let Some(o) = value.as_object() else {
        return false;
    };
    let Some(schema) = o.get("schema") else {
        return false;
    };
    resolve(document, schema).get("format") == Some(&json!("binary"))
}

fn flattenable(schema: &Value, required: bool) -> bool {
    let Some(o) = schema.as_object() else {
        return false;
    };
    required
        && o.get("type") == Some(&json!("object"))
        && o.get("properties").is_some_and(Value::is_object)
        && o.get("additionalProperties") == Some(&Value::Bool(false))
        && o.get("nullable") != Some(&Value::Bool(true))
        && !o.contains_key("allOf")
        && !o.contains_key("anyOf")
        && !o.contains_key("oneOf")
}

/// A field before its input name is chosen.
struct Planned {
    name: String,
    location: Location,
    required: bool,
    schema: Value,
    style: Option<Style>,
    explode: Option<bool>,
}

fn parameters<'a>(
    cx: &mut Context<'a>,
    path_item: &'a Map<String, Value>,
    operation: &'a Map<String, Value>,
) -> Parsed<Vec<Planned>> {
    // Operation parameters override path parameters with the same location
    // and name, keeping the first one's place.
    let mut declared: Vec<(String, String, &'a Map<String, Value>)> = Vec::new();
    for raw in as_array(path_item.get("parameters"))
        .iter()
        .chain(as_array(operation.get("parameters")))
    {
        let Some(p) = resolve(cx.document, raw).as_object() else {
            return Err("parameter declaration is invalid or unresolved".into());
        };
        let (Some(name), Some(location)) = (non_empty(p.get("name")), non_empty(p.get("in")))
        else {
            return Err("parameter declaration is missing name or location".into());
        };
        match declared
            .iter_mut()
            .find(|(n, l, _)| *n == name && *l == location)
        {
            Some(slot) => slot.2 = p,
            None => declared.push((name, location, p)),
        }
    }
    let mut unordered = Vec::new();
    for (name, location, p) in declared {
        let location = match location.as_str() {
            "cookie" => return Err(format!("cookie parameter '{name}' is not supported")),
            "path" => Location::Path,
            "query" => Location::Query,
            "header" => Location::Header,
            other => {
                return Err(format!(
                    "parameter '{name}' uses unsupported location '{other}'"
                ))
            }
        };
        if location == Location::Header
            && IGNORED_HEADERS.contains(&name.to_ascii_lowercase().as_str())
        {
            continue;
        }
        if !p.contains_key("schema") && !p.contains_key("content") {
            return Err(format!(
                "parameter '{name}' declares neither schema nor content"
            ));
        }
        if p.contains_key("content") {
            return Err(format!(
                "parameter '{name}' uses unsupported content encoding"
            ));
        }
        if p.contains_key("style") && non_empty(p.get("style")).is_none() {
            return Err(format!("parameter '{name}' has an invalid style"));
        }
        if p.get("explode").is_some_and(|e| !e.is_boolean()) {
            return Err(format!("parameter '{name}' has an invalid explode value"));
        }
        match p.get("allowReserved") {
            Some(Value::Bool(true)) => {
                return Err(format!(
                    "parameter '{name}' uses unsupported allowReserved encoding"
                ))
            }
            Some(Value::Bool(false)) | None => {}
            Some(_) => {
                return Err(format!(
                    "parameter '{name}' has an invalid allowReserved value"
                ))
            }
        }
        let declared_style = non_empty(p.get("style")).unwrap_or_else(|| {
            if location == Location::Query {
                "form".into()
            } else {
                "simple".into()
            }
        });
        if location == Location::Query && declared_style != "form" && declared_style != "deepObject"
        {
            return Err(format!(
                "query parameter '{name}' uses unsupported style '{declared_style}'"
            ));
        }
        if location != Location::Query && declared_style != "simple" {
            return Err(format!(
                "{} parameter '{name}' uses unsupported style '{declared_style}'",
                location.name()
            ));
        }
        let style = match declared_style.as_str() {
            "deepObject" => Style::DeepObject,
            "form" => Style::Form,
            _ => Style::Simple,
        };
        let explode = p
            .get("explode")
            .and_then(Value::as_bool)
            .unwrap_or(style == Style::Form);
        if style == Style::DeepObject && !explode {
            return Err(format!(
                "query parameter '{name}' uses deepObject with explode=false"
            ));
        }
        let mut schema = match p.get("schema") {
            Some(s) => cx.project(s, Direction::Request),
            None => json!({}),
        };
        if let (Some(description), Some(o)) =
            (non_empty(p.get("description")), schema.as_object_mut())
        {
            o.entry("description").or_insert(Value::String(description));
        }
        unordered.push(Planned {
            name,
            location,
            required: p.get("required") == Some(&Value::Bool(true)) || location == Location::Path,
            schema,
            style: Some(style),
            explode: Some(explode),
        });
    }
    Ok(PARAMETER_LOCATIONS
        .iter()
        .flat_map(|l| {
            let mut picked = Vec::new();
            let mut i = 0;
            while i < unordered.len() {
                if unordered[i].location == *l {
                    picked.push(unordered.remove(i));
                } else {
                    i += 1;
                }
            }
            picked
        })
        .collect())
}

fn json_content(content: &Map<String, Value>) -> Option<(&String, Option<&Value>)> {
    content
        .iter()
        .find(|(m, _)| is_json_media(m))
        .and_then(|(m, v)| v.as_object().map(|o| (m, o.get("schema"))))
}

fn request_body<'a>(
    cx: &mut Context<'a>,
    operation: &'a Map<String, Value>,
) -> Parsed<(Vec<Planned>, Option<Body>)> {
    let Some(rb) = operation
        .get("requestBody")
        .map(|v| resolve(cx.document, v))
        .and_then(Value::as_object)
    else {
        return Ok((vec![], None));
    };
    static EMPTY: std::sync::LazyLock<Map<String, Value>> = std::sync::LazyLock::new(Map::new);
    let content = rb
        .get("content")
        .and_then(Value::as_object)
        .unwrap_or(&EMPTY);
    let Some((media_type, schema)) = json_content(content) else {
        let declared: Vec<&str> = content.keys().map(String::as_str).collect();
        return Err(format!(
            "request body has no JSON content (declared: {})",
            if declared.is_empty() {
                "none".to_string()
            } else {
                declared.join(", ")
            }
        ));
    };
    static NULL: Value = Value::Null;
    let schema = schema.unwrap_or(&NULL);
    let required = rb.get("required") == Some(&Value::Bool(true));
    let resolved = resolve(cx.document, schema);
    let projected = cx.project_raw(resolved, Direction::Request);
    if !flattenable(&projected, required) {
        return Ok((
            vec![Planned {
                name: "body".into(),
                location: Location::Body,
                required,
                schema: cx.project(schema, Direction::Request),
                style: None,
                explode: None,
            }],
            Some(Body {
                required,
                mode: BodyMode::Value,
                media_type: media_type.clone(),
            }),
        ));
    }
    let required_props: BTreeSet<&str> = as_array(projected.get("required"))
        .iter()
        .filter_map(Value::as_str)
        .collect();
    // Field schemas were projected with the body as their root; projecting
    // each again from itself would misresolve shadowed local `$defs`.
    let fields = projected["properties"]
        .as_object()
        .map(|props| {
            props
                .iter()
                .map(|(name, v)| Planned {
                    name: name.clone(),
                    location: Location::Body,
                    required: required && required_props.contains(name.as_str()),
                    schema: normalize(cx.v3_0, v),
                    style: None,
                    explode: None,
                })
                .collect()
        })
        .unwrap_or_default();
    Ok((
        fields,
        Some(Body {
            required,
            mode: BodyMode::Object,
            media_type: media_type.clone(),
        }),
    ))
}

fn input<'a>(
    cx: &mut Context<'a>,
    path_item: &'a Map<String, Value>,
    operation: &'a Map<String, Value>,
) -> Parsed<(Vec<Field>, Option<Body>)> {
    let mut planned = parameters(cx, path_item, operation)?;
    let (body_fields, body) = request_body(cx, operation)?;
    planned.extend(body_fields);
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for f in &planned {
        *counts.entry(f.name.as_str()).or_default() += 1;
    }
    let conflicts: BTreeSet<String> = counts
        .into_iter()
        .filter(|(_, n)| *n > 1)
        .map(|(k, _)| k.to_string())
        .collect();
    let mut used = BTreeSet::new();
    let fields = planned
        .into_iter()
        .map(|f| {
            let base = if conflicts.contains(&f.name) {
                format!("{}_{}", f.location.name(), f.name)
            } else {
                f.name.clone()
            };
            let mut index = 1;
            let input_name = loop {
                let candidate = if index == 1 {
                    base.clone()
                } else {
                    format!("{base}_{index}")
                };
                if !used.contains(&candidate) {
                    break candidate;
                }
                index += 1;
            };
            used.insert(input_name.clone());
            Field {
                input_name,
                name: f.name,
                location: f.location,
                required: f.required,
                schema: f.schema,
                style: f.style,
                explode: f.explode,
            }
        })
        .collect();
    Ok((fields, body))
}

fn input_schema(fields: &[Field], definitions: &BTreeMap<String, Value>) -> Value {
    let required: Vec<&str> = fields
        .iter()
        .filter(|f| f.required)
        .map(|f| f.input_name.as_str())
        .collect();
    let mut s = json!({
        "type": "object",
        "properties": fields.iter().map(|f| (f.input_name.clone(), f.schema.clone())).collect::<Map<_, _>>(),
    });
    if !required.is_empty() {
        s["required"] = json!(required);
    }
    with_definitions(s, definitions)
}

fn successful<'a>(
    document: &'a Value,
    operation: &'a Map<String, Value>,
) -> Parsed<Vec<&'a Map<String, Value>>> {
    let Some(responses) = operation.get("responses").and_then(Value::as_object) else {
        return Ok(vec![]);
    };
    let mut selected: Vec<(&String, &Value)> = responses
        .iter()
        .filter(|(s, _)| {
            s.len() == 3 && s.starts_with('2') && s.chars().all(|c| c.is_ascii_digit())
        })
        .collect();
    selected.sort_by(|a, b| a.0.cmp(b.0));
    selected.extend(
        responses
            .iter()
            .filter(|(s, _)| s.eq_ignore_ascii_case("2XX")),
    );
    let mut out = Vec::new();
    for (_, v) in selected {
        match resolve(document, v).as_object() {
            Some(o) if non_empty(o.get("$ref")).is_none() => out.push(o),
            _ => return Err("successful response declaration is invalid or unresolved".into()),
        }
    }
    Ok(out)
}

fn output<'a>(
    cx: &mut Context<'a>,
    operation: &'a Map<String, Value>,
    definitions: &BTreeMap<String, Value>,
) -> Parsed<Option<Value>> {
    if operation.get("x-websocket") == Some(&Value::Bool(true)) {
        return Err("WebSocket operations are not supported".into());
    }
    let responses = successful(cx.document, operation)?;
    let contents = || {
        responses
            .iter()
            .filter_map(|r| r.get("content").and_then(Value::as_object))
    };
    if contents().any(|c| c.keys().any(|m| media(m) == "text/event-stream")) {
        return Err("SSE operations are not supported".into());
    }
    if contents().any(|c| c.iter().any(|(m, v)| is_binary_media(cx.document, m, v))) {
        return Err("binary responses are not supported".into());
    }
    let mut outcomes = Vec::new();
    for r in &responses {
        let content = match r.get("content") {
            None => None,
            Some(Value::Object(c)) => Some(c),
            Some(_) => return Ok(None),
        };
        let Some(content) = content.filter(|c| !c.is_empty()) else {
            outcomes.push(json!({"type": "null"}));
            continue;
        };
        for (m, v) in content {
            if !is_json_media(m) {
                outcomes.push(json!({"type": "string"}));
                continue;
            }
            let Some(schema) = v.as_object().and_then(|o| o.get("schema")) else {
                return Ok(None);
            };
            outcomes.push(cx.project(schema, Direction::Response));
        }
    }
    if outcomes.is_empty() {
        return Ok(None);
    }
    let schema = if outcomes.len() == 1 {
        outcomes.remove(0)
    } else {
        json!({"anyOf": outcomes})
    };
    Ok(Some(with_definitions(schema, definitions)))
}

fn sanitize(raw: &str) -> String {
    let mut out = String::new();
    let mut gap = false;
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '$' {
            if gap {
                out.push('_');
                gap = false;
            }
            out.push(c);
        } else {
            gap = true;
        }
    }
    if gap {
        out.push('_');
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        return "operation".into();
    }
    if trimmed.starts_with(|c: char| c.is_ascii_digit()) {
        return format!("_{trimmed}");
    }
    trimmed.to_string()
}

fn fallback_id(method: &str, path: &str) -> String {
    let mut words = vec![method.to_string()];
    for part in path.split('/').filter(|p| !p.is_empty()) {
        let parts: Vec<&str> = match part.strip_prefix('{').and_then(|p| p.strip_suffix('}')) {
            Some(inner) => vec!["by", inner],
            None => vec![part],
        };
        for p in parts {
            words.extend(
                p.split(|c: char| !c.is_ascii_alphanumeric())
                    .filter(|w| !w.is_empty())
                    .map(str::to_string),
            );
        }
    }
    words
        .iter()
        .enumerate()
        .map(|(i, w)| {
            let lower = w.to_lowercase();
            if i == 0 {
                lower
            } else {
                let mut c = lower.chars();
                c.next()
                    .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                    .unwrap_or_default()
            }
        })
        .collect()
}

fn available(segments: &[String], used: &BTreeSet<String>, namespaces: &BTreeSet<String>) -> bool {
    let k = segments.join(".");
    if used.contains(&k) || namespaces.contains(&k) {
        return false;
    }
    (1..segments.len()).all(|i| !used.contains(&segments[..i].join(".")))
}

fn operation_path(
    method: &str,
    path: &str,
    operation: &Map<String, Value>,
    used: &BTreeSet<String>,
    namespaces: &BTreeSet<String>,
) -> Vec<String> {
    let segments: Vec<String> = match non_empty(operation.get("operationId")) {
        Some(raw) => raw.split('.').map(sanitize).collect(),
        None => vec![sanitize(&fallback_id(method, path))],
    };
    if available(&segments, used, namespaces) {
        return segments;
    }
    let conflict = (1..segments.len()).find(|&i| used.contains(&segments[..i].join(".")));
    if let Some(end) = conflict {
        let at = end - 1;
        if at + 1 < segments.len() {
            let mut collapsed = Vec::new();
            for (i, s) in segments.iter().enumerate() {
                if i == at {
                    let next = &segments[i + 1];
                    let mut c = next.chars();
                    let capital = c
                        .next()
                        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                        .unwrap_or_default();
                    collapsed.push(format!("{s}{capital}"));
                } else if i != at + 1 {
                    collapsed.push(s.clone());
                }
            }
            if available(&collapsed, used, namespaces) {
                return collapsed;
            }
        }
    }
    let fallback = segments.join("_");
    let mut index = 2;
    loop {
        let candidate = vec![format!("{fallback}_{index}")];
        if available(&candidate, used, namespaces) {
            return candidate;
        }
        index += 1;
    }
}

/// A base URL as OpenCode accepts it: absolute HTTP(S), with no query
/// string or fragment. CodeSmith also refuses credentials in it.
pub fn validate_base_url(value: &str) -> Parsed<String> {
    let lower = value.to_ascii_lowercase();
    let not_absolute = || format!("server URL '{value}' is not an absolute HTTP(S) URL");
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err(not_absolute());
    }
    let url = url::Url::parse(value).map_err(|_| not_absolute())?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none_or(str::is_empty) {
        return Err(not_absolute());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(format!(
            "server URL '{value}' contains an unsupported query string or fragment"
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(format!("server URL '{value}' must not include credentials"));
    }
    Ok(value.to_string())
}

fn server_url(source: &Map<String, Value>) -> Parsed<String> {
    let url = as_array(source.get("servers"))
        .iter()
        .find_map(Value::as_object)
        .and_then(|s| non_empty(s.get("url")));
    let Some(url) = url else {
        return Err("spec declares no servers; set base_url".into());
    };
    if regex_like_template(&url) {
        return Err(format!(
            "server URL '{url}' is not an absolute URL; set base_url"
        ));
    }
    validate_base_url(&url)
}

/// Whether `s` has a `{name}` template.
fn regex_like_template(s: &str) -> bool {
    let mut open = None;
    for (i, c) in s.char_indices() {
        match c {
            '{' => open = Some(i),
            '}' => {
                if let Some(o) = open {
                    if i > o + 1 {
                        return true;
                    }
                }
                open = None;
            }
            _ => {}
        }
    }
    false
}

fn requirements(value: Option<&Value>) -> Parsed<Vec<Requirement>> {
    let Some(value) = value else {
        return Ok(vec![]);
    };
    let Some(items) = value.as_array() else {
        return Err("security declaration is not an array".into());
    };
    let mut out = Vec::new();
    for item in items {
        let Some(o) = item.as_object() else {
            return Err("security requirement is not an object".into());
        };
        let mut req = Vec::new();
        for (name, scopes) in o {
            let Some(scopes) = scopes.as_array() else {
                return Err("security requirement scopes are not string arrays".into());
            };
            let parsed: Vec<String> = scopes
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
            if parsed.len() != scopes.len() {
                return Err("security requirement scopes are not string arrays".into());
            }
            req.push((name.clone(), parsed));
        }
        out.push(req);
    }
    Ok(out)
}

fn operation_security(
    value: Option<&Value>,
    defaults: &Parsed<Vec<Requirement>>,
    schemes: &BTreeMap<String, Scheme>,
) -> Parsed<Vec<Requirement>> {
    let parsed = match value {
        None => defaults.clone()?,
        Some(v) => requirements(Some(v))?,
    };
    let cookie = |name: &str| {
        matches!(
            schemes.get(name),
            Some(Scheme::ApiKey {
                carrier: Carrier::Cookie,
                ..
            })
        )
    };
    let supported: Vec<Requirement> = parsed
        .iter()
        .filter(|r| r.iter().all(|(n, _)| schemes.contains_key(n) && !cookie(n)))
        .cloned()
        .collect();
    if parsed.is_empty() || !supported.is_empty() {
        return Ok(supported);
    }
    let mut names: Vec<&str> = Vec::new();
    for (n, _) in parsed.iter().flatten() {
        if !names.contains(&n.as_str()) {
            names.push(n);
        }
    }
    Err(match names.iter().find(|n| cookie(n)) {
        Some(c) => format!("cookie authentication '{c}' is not supported"),
        None => format!(
            "security requirement references missing or malformed scheme: {}",
            names.join(", ")
        ),
    })
}

pub fn security_schemes(document: &Value) -> BTreeMap<String, Scheme> {
    let Some(declared) = document
        .pointer("/components/securitySchemes")
        .and_then(Value::as_object)
    else {
        return BTreeMap::new();
    };
    declared
        .iter()
        .filter_map(|(name, v)| {
            let o = resolve(document, v).as_object()?;
            let scheme = match non_empty(o.get("type"))?.as_str() {
                "apiKey" => {
                    let carrier = match non_empty(o.get("in"))?.as_str() {
                        "header" => Carrier::Header,
                        "query" => Carrier::Query,
                        "cookie" => Carrier::Cookie,
                        _ => return None,
                    };
                    Scheme::ApiKey {
                        name: non_empty(o.get("name"))?,
                        carrier,
                    }
                }
                "http" => Scheme::Http {
                    scheme: non_empty(o.get("scheme"))?.to_ascii_lowercase(),
                },
                "oauth2" => Scheme::OAuth2,
                "openIdConnect" => Scheme::OpenIdConnect,
                _ => return None,
            };
            Some((name.clone(), scheme))
        })
        .collect()
}

/// One tool per representable operation of `document`; `base_url`
/// overrides every `servers` entry (and is needed when none is absolute).
pub fn from_spec(document: &Value, base_url: Option<&str>) -> Generated {
    let mut cx = Context {
        document,
        v3_0: non_empty(document.get("openapi")).is_some_and(|v| v.starts_with("3.0")),
        directional: has_directional(document),
        request: Projector::new(document, Direction::Request),
        response: Projector::new(document, Direction::Response),
    };
    let schemes = security_schemes(document);
    let default_security = requirements(document.get("security"));
    let request_defs = cx.definitions(Direction::Request);
    let response_defs = if cx.directional {
        cx.definitions(Direction::Response)
    } else {
        request_defs.clone()
    };
    let mut used = BTreeSet::new();
    let mut namespaces = BTreeSet::new();
    let mut out = Generated::default();
    let empty = Map::new();
    let doc_map = document.as_object().unwrap_or(&empty);
    let Some(paths) = document.get("paths").and_then(Value::as_object) else {
        return out;
    };
    for (path, path_value) in paths {
        let Some(path_item) = path_value.as_object() else {
            continue;
        };
        for (method, op_value) in path_item {
            if !METHODS.contains(&method.as_str()) {
                continue;
            }
            let Some(op) = op_value.as_object() else {
                continue;
            };
            let operation = Operation {
                operation_id: non_empty(op.get("operationId")),
                method: method.to_ascii_uppercase(),
                path: path.clone(),
                summary: non_empty(op.get("summary")),
                description: non_empty(op.get("description")),
            };
            let skip = |out: &mut Generated, reason: String| {
                out.skipped.push(Skipped {
                    method: method.to_ascii_uppercase(),
                    path: path.clone(),
                    reason,
                })
            };
            let output = match output(&mut cx, op, &response_defs) {
                Ok(o) => o,
                Err(e) => {
                    skip(&mut out, e);
                    continue;
                }
            };
            let base = match base_url {
                Some(b) => validate_base_url(b),
                None if op.contains_key("servers") => server_url(op),
                None if path_item.contains_key("servers") => server_url(path_item),
                None => server_url(doc_map),
            };
            let base = match base {
                Ok(b) => b,
                Err(e) => {
                    skip(&mut out, e);
                    continue;
                }
            };
            let (fields, body) = match input(&mut cx, path_item, op) {
                Ok(i) => i,
                Err(e) => {
                    skip(&mut out, e);
                    continue;
                }
            };
            let security = match operation_security(op.get("security"), &default_security, &schemes)
            {
                Ok(s) => s,
                Err(e) => {
                    skip(&mut out, e);
                    continue;
                }
            };
            let segments = operation_path(method, path, op, &used, &namespaces);
            used.insert(segments.join("."));
            for i in 1..segments.len() {
                namespaces.insert(segments[..i].join("."));
            }
            let description = operation
                .description
                .clone()
                .or_else(|| operation.summary.clone())
                .unwrap_or_else(|| format!("{} {path}", operation.method));
            out.tools.push(Tool {
                path: segments,
                description,
                input: input_schema(&fields, &request_defs),
                output,
                plan: Plan {
                    operation,
                    url: format!("{}{path}", base.trim_end_matches('/')),
                    fields,
                    body,
                    security,
                    schemes: schemes.clone(),
                },
            });
        }
    }
    out
}
