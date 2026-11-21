//! An OpenAPI tool call: the request built from the model's input, the
//! credentials applied, and the response decoded. Ported from OpenCode's
//! `packages/codemode/src/codemode/openapi/runtime.ts` (MIT).
//!
//! Input is validated before credentials are resolved or anything is
//! sent. Credentials are applied last, so neither the model's header
//! parameters nor configured headers can replace them.

use super::spec::{Body, BodyMode, Carrier, Field, Location, Operation, Plan, Scheme, Style};
use base64::Engine;
use serde_json::{Map, Value};

/// The most of an error body an error message quotes.
const MAX_ERROR_BODY_CHARS: usize = 1_024;

/// Credential material for one security scheme.
#[derive(Clone, PartialEq)]
pub enum Credential {
    Bearer(String),
    Basic {
        username: String,
        password: String,
    },
    /// Sent where the scheme's `apiKey` declaration says.
    ApiKey(String),
    /// A header of any name, for nonstandard schemes.
    Header {
        name: String,
        value: String,
    },
}

impl std::fmt::Debug for Credential {
    // Never print the secret.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self {
            Credential::Bearer(_) => "Bearer",
            Credential::Basic { .. } => "Basic",
            Credential::ApiKey(_) => "ApiKey",
            Credential::Header { .. } => "Header",
        };
        write!(f, "Credential::{kind}(..)")
    }
}

/// What a resolver is asked about.
#[derive(Debug, Clone, PartialEq)]
pub struct AuthContext<'a> {
    pub name: &'a str,
    pub scheme: &'a Scheme,
    pub scopes: &'a [String],
    pub operation: &'a Operation,
}

/// Resolves a scheme's credential: `Ok(None)` tries the next alternative,
/// an error stops the call.
pub type Resolver = dyn Fn(&AuthContext) -> Result<Option<Credential>, String> + Send + Sync;

/// The request to send.
#[derive(Clone, PartialEq)]
pub struct Prepared {
    pub method: String,
    pub url: url::Url,
    /// Lower-case names, each once.
    pub headers: Vec<(String, String)>,
    /// The media type and the encoded body.
    pub body: Option<(String, Vec<u8>)>,
}

impl std::fmt::Debug for Prepared {
    // Headers may hold credentials: only their names.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Prepared")
            .field("method", &self.method)
            .field("path", &self.url.path())
            .field(
                "headers",
                &self.headers.iter().map(|(k, _)| k).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl Prepared {
    pub fn set_header(&mut self, name: &str, value: &str) {
        let name = name.to_ascii_lowercase();
        self.headers.retain(|(k, _)| *k != name);
        self.headers.push((name, value.to_string()));
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn set_query(&mut self, name: &str, value: &str) {
        let pairs: Vec<(String, String)> = self
            .url
            .query_pairs()
            .filter(|(k, _)| k != name)
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        self.url.set_query(None);
        let mut q = self.url.query_pairs_mut();
        for (k, v) in &pairs {
            q.append_pair(k, v);
        }
        q.append_pair(name, value);
    }
}

/// Credentials as headers and query parameters.
#[derive(Default, Clone, PartialEq)]
pub struct Applied {
    pub headers: Vec<(String, String)>,
    pub query: Vec<(String, String)>,
}

impl Applied {
    pub fn apply(&self, p: &mut Prepared) {
        for (k, v) in &self.query {
            p.set_query(k, v);
        }
        for (k, v) in &self.headers {
            p.set_header(k, v);
        }
    }

    /// The query parameter names that carry credentials.
    pub fn query_names(&self) -> Vec<&str> {
        self.query.iter().map(|(k, _)| k.as_str()).collect()
    }

    /// The credential values, to keep out of error messages: each header
    /// and query value, and for `Bearer`/`Basic` headers what follows the
    /// scheme too.
    pub fn secrets(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (_, v) in self.headers.iter().chain(&self.query) {
            out.push(v.clone());
            if let Some((_, rest)) = v.split_once(' ') {
                out.push(rest.to_string());
            }
        }
        out.sort_by_key(|s| std::cmp::Reverse(s.len()));
        out
    }
}

fn label(op: &Operation) -> String {
    format!("{} {}", op.method, op.path)
}

/// `encodeURIComponent`, with `!'()*` encoded too.
fn encode_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// JavaScript's `String(value)` for a scalar.
fn scalar_text(v: &Value) -> Option<String> {
    match v {
        Value::Null => Some("null".into()),
        Value::Bool(b) => Some(b.to_string()),
        // A whole float prints as JavaScript prints it: `2`, not `2.0`.
        Value::Number(n) => Some(match n.as_f64() {
            Some(f) if n.is_f64() && f.fract() == 0.0 && f.abs() < 1e15 => format!("{}", f as i64),
            _ => n.to_string(),
        }),
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

fn nested(field: &Field) -> String {
    format!(
        "Parameter '{}' contains an unsupported nested value.",
        field.input_name
    )
}

fn serialize_simple(
    field: &Field,
    value: &Value,
    encode: fn(&str) -> String,
) -> Result<String, String> {
    let scalar = |v: &Value| {
        scalar_text(v)
            .map(|s| encode(&s))
            .ok_or_else(|| nested(field))
    };
    match value {
        Value::Array(items) => Ok(items
            .iter()
            .map(scalar)
            .collect::<Result<Vec<_>, _>>()?
            .join(",")),
        Value::Object(o) => {
            let mut parts = Vec::new();
            for (k, v) in o {
                let rendered = scalar(v)?;
                if field.explode == Some(true) {
                    parts.push(format!("{}={rendered}", encode(k)));
                } else {
                    parts.push(encode(k));
                    parts.push(rendered);
                }
            }
            Ok(parts.join(","))
        }
        v => scalar(v),
    }
}

fn plain(s: &str) -> String {
    s.to_string()
}

fn serialize_query(field: &Field, value: &Value) -> Result<Vec<(String, String)>, String> {
    let flat = |o: &Map<String, Value>, what: &str, key: &dyn Fn(&str) -> String| {
        let mut out = Vec::new();
        for (k, v) in o {
            match v {
                Value::Array(_) | Value::Object(_) => {
                    return Err(format!(
                        "{what} '{}' contains an unsupported nested value.",
                        field.input_name
                    ))
                }
                v => out.push((key(k), scalar_text(v).unwrap_or_default())),
            }
        }
        Ok(out)
    };
    if field.style == Some(Style::DeepObject) {
        let Value::Object(o) = value else {
            return Err(format!(
                "Deep-object parameter '{}' must be an object.",
                field.input_name
            ));
        };
        return flat(o, "Deep-object parameter", &|k| {
            format!("{}[{k}]", field.name)
        });
    }
    match value {
        Value::Array(_) if field.explode != Some(true) => Ok(vec![(
            field.name.clone(),
            serialize_simple(field, value, plain)?,
        )]),
        Value::Array(items) => items
            .iter()
            .map(|i| {
                scalar_text(i)
                    .map(|s| (field.name.clone(), s))
                    .ok_or_else(|| nested(field))
            })
            .collect(),
        Value::Object(o) if field.explode == Some(true) => {
            flat(o, "Query parameter", &|k| k.to_string())
        }
        v => Ok(vec![(
            field.name.clone(),
            serialize_simple(field, v, plain)?,
        )]),
    }
}

fn get<'a>(input: &'a Map<String, Value>, field: &Field) -> Option<&'a Value> {
    input.get(&field.input_name)
}

fn build_url(plan: &Plan, input: &Map<String, Value>) -> Result<String, String> {
    let mut url = plan.url.clone();
    for field in plan.fields.iter().filter(|f| f.location == Location::Path) {
        let Some(item) = get(input, field) else {
            return Err(format!(
                "Missing required path parameter '{}'.",
                field.input_name
            ));
        };
        let value = serialize_simple(field, item, encode_component)?;
        // URL normalization collapses `.` and `..`, which could retarget the request.
        if value.is_empty() || value == "." || value == ".." {
            return Err(format!("Invalid path parameter '{}'.", field.input_name));
        }
        url = url.replace(&format!("{{{}}}", field.name), &value);
    }
    if let Some(start) = url.find('{') {
        if let Some(len) = url[start..].find('}') {
            if len > 1 && !url[start + 1..start + len].contains('{') {
                return Err(format!(
                    "Unresolved path parameter {}.",
                    &url[start..=start + len]
                ));
            }
        }
    }
    Ok(url)
}

/// Build the request for `input`, failing on input it cannot send before
/// anything else happens. `headers` are the configured static headers;
/// declared header parameters may override them.
pub fn build(plan: &Plan, input: &Value, headers: &[(String, String)]) -> Result<Prepared, String> {
    static EMPTY: std::sync::LazyLock<Map<String, Value>> = std::sync::LazyLock::new(Map::new);
    let input = input.as_object().unwrap_or(&EMPTY);
    let url = build_url(plan, input)?;
    if let Some(missing) = plan
        .fields
        .iter()
        .find(|f| f.required && f.location != Location::Path && get(input, f).is_none())
    {
        let what = if missing.location == Location::Body {
            "body field".to_string()
        } else {
            format!("{} parameter", location_name(missing.location))
        };
        return Err(format!("Missing required {what} '{}'.", missing.input_name));
    }
    let mut url = url::Url::parse(&url)
        .map_err(|_| format!("{} has an invalid URL.", label(&plan.operation)))?;
    let mut query = Vec::new();
    for field in plan.fields.iter().filter(|f| f.location == Location::Query) {
        if let Some(item) = get(input, field) {
            query.extend(serialize_query(field, item)?);
        }
    }
    if !query.is_empty() {
        let mut q = url.query_pairs_mut();
        for (k, v) in &query {
            q.append_pair(k, v);
        }
    }
    let mut p = Prepared {
        method: plan.operation.method.clone(),
        url,
        headers: Vec::new(),
        body: None,
    };
    for (k, v) in headers {
        p.set_header(k, v);
    }
    for field in plan
        .fields
        .iter()
        .filter(|f| f.location == Location::Header)
    {
        if let Some(item) = get(input, field) {
            let v = serialize_simple(field, item, plain)?;
            p.set_header(&field.name, &v);
        }
    }
    let encode = |value: &Value, body: &Body| {
        serde_json::to_vec(value)
            .map(|b| (body.media_type.clone(), b))
            .map_err(|_| format!("Invalid JSON body for {}.", label(&plan.operation)))
    };
    match &plan.body {
        Some(body) if body.mode == BodyMode::Value => {
            let field = plan.fields.iter().find(|f| f.location == Location::Body);
            if let Some(v) = field.and_then(|f| get(input, f)) {
                p.body = Some(encode(v, body)?);
            }
        }
        Some(body) => {
            let entries: Map<String, Value> = plan
                .fields
                .iter()
                .filter(|f| f.location == Location::Body)
                .filter_map(|f| get(input, f).map(|v| (f.name.clone(), v.clone())))
                .collect();
            if body.required || !entries.is_empty() {
                p.body = Some(encode(&Value::Object(entries), body)?);
            }
        }
        None => {}
    }
    Ok(p)
}

fn location_name(l: Location) -> &'static str {
    match l {
        Location::Path => "path",
        Location::Query => "query",
        Location::Header => "header",
        Location::Body => "body",
    }
}

/// The first security alternative whose every scheme has a credential, as
/// headers and query parameters.
pub fn resolve_auth(plan: &Plan, resolver: Option<&Resolver>) -> Result<Applied, String> {
    if plan.security.is_empty() {
        return Ok(Applied::default());
    }
    let mut unavailable: Vec<String> = Vec::new();
    'alternatives: for requirement in &plan.security {
        if requirement.is_empty() {
            return Ok(Applied::default());
        }
        let mut credentials = Vec::new();
        for (name, scopes) in requirement {
            let (Some(scheme), Some(resolve)) = (plan.schemes.get(name), resolver) else {
                unavailable.push(name.clone());
                continue 'alternatives;
            };
            let credential = resolve(&AuthContext {
                name,
                scheme,
                scopes,
                operation: &plan.operation,
            })?;
            let Some(credential) = credential else {
                unavailable.push(name.clone());
                continue 'alternatives;
            };
            credentials.push((name.as_str(), scheme, credential));
        }
        return apply_credentials(&credentials);
    }
    let mut names: Vec<String> = Vec::new();
    for n in unavailable {
        if !names.contains(&n) {
            names.push(n);
        }
    }
    Err(format!(
        "{} requires authentication; no credential available for: {}.",
        label(&plan.operation),
        names.join(", ")
    ))
}

fn apply_credentials(credentials: &[(&str, &Scheme, Credential)]) -> Result<Applied, String> {
    let mut out = Applied::default();
    let mut add = |carrier: Carrier, name: String, value: String| {
        let (target, what) = match carrier {
            Carrier::Header => (&mut out.headers, "header"),
            _ => (&mut out.query, "query"),
        };
        if target.iter().any(|(k, _)| *k == name) {
            return Err(format!(
                "Authentication resolves multiple credentials for {what} '{name}'."
            ));
        }
        target.push((name, value));
        Ok(())
    };
    for (name, scheme, credential) in credentials {
        match credential {
            Credential::Bearer(token) => add(
                Carrier::Header,
                "authorization".into(),
                format!("Bearer {token}"),
            )?,
            Credential::Basic { username, password } => add(
                Carrier::Header,
                "authorization".into(),
                format!(
                    "Basic {}",
                    base64::engine::general_purpose::STANDARD
                        .encode(format!("{username}:{password}"))
                ),
            )?,
            Credential::Header { name, value } => {
                add(Carrier::Header, name.to_ascii_lowercase(), value.clone())?
            }
            Credential::ApiKey(value) => {
                let Scheme::ApiKey {
                    name: parameter,
                    carrier,
                } = scheme
                else {
                    return Err(format!(
                        "Security scheme '{name}' is not an apiKey scheme; resolve a bearer, basic, or header credential for it."
                    ));
                };
                match carrier {
                    Carrier::Cookie => {
                        return Err(format!("Cookie authentication '{name}' is not supported."))
                    }
                    Carrier::Header => add(
                        Carrier::Header,
                        parameter.to_ascii_lowercase(),
                        value.clone(),
                    )?,
                    Carrier::Query => add(Carrier::Query, parameter.clone(), value.clone())?,
                }
            }
        }
    }
    Ok(out)
}

fn media(content_type: Option<&str>) -> String {
    content_type
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// The response as the program sees it: JSON decoded, other text as a
/// string, an empty body as `null`; a non-2xx status or malformed JSON is
/// an error.
pub fn decode(
    op: &Operation,
    status: u16,
    content_type: Option<&str>,
    body: &[u8],
) -> Result<Value, String> {
    let text = String::from_utf8_lossy(body);
    let m = media(content_type);
    let json = m == "application/json" || m.ends_with("+json");
    let decoded: Option<Value> = if text.is_empty() {
        Some(Value::Null)
    } else if json {
        serde_json::from_str(&text).ok()
    } else {
        None
    };
    let parsed = match (&decoded, json) {
        (Some(v), true) => v.clone(),
        (None, true) => Value::String(text.to_string()),
        _ if text.is_empty() => Value::Null,
        _ => Value::String(text.to_string()),
    };
    if !(200..300).contains(&status) {
        let rendered = match &parsed {
            Value::String(s) => s.clone(),
            v => v.to_string(),
        };
        let summary = if rendered.is_empty() || rendered == "null" {
            "no response body".to_string()
        } else if rendered.chars().count() > MAX_ERROR_BODY_CHARS {
            format!(
                "{}...",
                rendered
                    .chars()
                    .take(MAX_ERROR_BODY_CHARS)
                    .collect::<String>()
            )
        } else {
            rendered
        };
        return Err(format!(
            "{} failed with HTTP {status}: {summary}",
            label(op)
        ));
    }
    if json && decoded.is_none() {
        return Err(format!("{} returned malformed JSON.", label(op)));
    }
    Ok(parsed)
}

pub(super) fn too_large(op: &Operation, max: usize) -> String {
    format!(
        "{} response exceeds {} MiB.",
        label(op),
        max / (1024 * 1024)
    )
}

pub(super) fn transport(op: &Operation, e: &reqwest::Error) -> String {
    // Without the URL: an API key may be in its query.
    let mut msg = format!("{} failed: transport error", label(op));
    let mut source: Option<&dyn std::error::Error> = std::error::Error::source(e);
    if e.is_timeout() {
        msg.push_str(" (timed out)");
    } else if e.is_connect() {
        msg.push_str(" (could not connect)");
    }
    // The innermost cause names the problem (refused, reset, DNS) without
    // the URL.
    let mut last = None;
    while let Some(s) = source {
        last = Some(s.to_string());
        source = s.source();
    }
    if let Some(l) = last.filter(|l| !l.contains("://")) {
        msg.push_str(&format!(": {l}"));
    }
    msg
}

pub(super) fn label_of(op: &Operation) -> String {
    label(op)
}
