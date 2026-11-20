//! Tool input schema projection, ported from OpenCode's `tool-schema.ts` and
//! `gemini-json-schema.ts` (MIT). Every tool schema gets an object root; then
//! the model family's rules apply: Gemini rejects a few common JSON Schema
//! shapes, Moonshot (Kimi) others. The Gemini API always uses Gemini's rules.

use serde_json::{json, Map, Value};

/// Keys whose value is a map of name to schema.
const SCHEMA_MAPS: [&str; 6] = [
    "properties",
    "patternProperties",
    "$defs",
    "definitions",
    "dependentSchemas",
    "dependencies",
];
/// Keys whose value is data, never a schema.
const VALUES: [&str; 5] = ["const", "default", "enum", "examples", "dependentRequired"];

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Sanitizer {
    Gemini,
    Moonshot,
}

/// The sanitizer a model id selects, if any.
pub fn for_model(model: &str) -> Option<Sanitizer> {
    let m = model.to_ascii_lowercase();
    if m.contains("gemini") {
        Some(Sanitizer::Gemini)
    } else if m.contains("kimi") {
        Some(Sanitizer::Moonshot)
    } else {
        None
    }
}

/// The schema a protocol sends for a tool's input.
pub fn project(schema: &Value, sanitizer: Option<Sanitizer>) -> Value {
    let root = object_root(schema);
    match sanitizer {
        Some(Sanitizer::Gemini) => gemini(&root),
        Some(Sanitizer::Moonshot) => moonshot(&root),
        None => root,
    }
}

fn map_values(m: &Map<String, Value>, f: impl Fn(&str, &Value) -> Value) -> Value {
    Value::Object(m.iter().map(|(k, v)| (k.clone(), f(k, v))).collect())
}

/// Tool arguments are always an object, and most providers reject a schema
/// whose root does not say so (`{}`, a bare `properties` map).
pub fn object_root(schema: &Value) -> Value {
    let Value::Object(m) = schema else {
        return json!({"type": "object"});
    };
    let untyped = !m.contains_key("type");
    let mut out = Map::new();
    // `{ not: { type: "null" } }` is an empty-struct marker; drop it.
    let not_null = m
        .get("not")
        .and_then(Value::as_object)
        .is_some_and(|n| n.len() == 1 && n.get("type") == Some(&json!("null")));
    if (untyped || m.get("type") == Some(&json!("object"))) && not_null {
        out.insert("type".into(), json!("object"));
        out.extend(
            m.iter()
                .filter(|(k, _)| *k != "not")
                .map(|(k, v)| (k.clone(), v.clone())),
        );
        return Value::Object(out);
    }
    if !untyped {
        return schema.clone();
    }
    // `anyOf: [{type: object}, ...]` from empty-struct encodings; drop it.
    let marker = m.get("anyOf").and_then(Value::as_array).is_some_and(|a| {
        a.iter().any(|b| {
            b.as_object()
                .is_some_and(|o| o.len() == 1 && o.get("type") == Some(&json!("object")))
        })
    });
    out.insert("type".into(), json!("object"));
    out.extend(
        m.iter()
            .filter(|(k, _)| !(marker && *k == "anyOf"))
            .map(|(k, v)| (k.clone(), v.clone())),
    );
    Value::Object(out)
}

fn gemini_node(schema: &Value) -> Value {
    if let Value::Array(a) = schema {
        return Value::Array(a.iter().map(gemini_node).collect());
    }
    let Value::Object(m) = schema else {
        return schema.clone();
    };
    let properties = m.get("properties").and_then(Value::as_object);
    let mut out = Map::new();
    for (k, v) in m {
        let key = k.as_str();
        if VALUES.contains(&key) {
            out.insert(k.clone(), v.clone());
            continue;
        }
        if SCHEMA_MAPS.contains(&key) {
            if let Value::Object(map) = v {
                out.insert(k.clone(), map_values(map, |_, s| gemini_node(s)));
                continue;
            }
        }
        match key {
            // `required` may only name declared properties.
            "required" if properties.is_some() && v.is_array() => {
                let props = properties.unwrap();
                let names: Vec<Value> = v
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|n| n.as_str().is_some_and(|n| props.contains_key(n)))
                    .cloned()
                    .collect();
                out.insert(k.clone(), Value::Array(names));
            }
            // Draft-04 boolean exclusive bounds become the numeric form.
            "exclusiveMinimum" | "exclusiveMaximum" if v.is_boolean() => {
                let bound = if key == "exclusiveMinimum" {
                    "minimum"
                } else {
                    "maximum"
                };
                if v == &json!(true) {
                    if let Some(n) = m.get(bound).filter(|n| n.is_number()) {
                        out.insert(k.clone(), n.clone());
                    }
                }
            }
            "minimum" if m.get("exclusiveMinimum") == Some(&json!(true)) => {}
            "maximum" if m.get("exclusiveMaximum") == Some(&json!(true)) => {}
            // Draft-07 tuples are `prefixItems` plus `items` in 2020-12.
            "items" if v.is_array() => {
                out.insert("prefixItems".into(), gemini_node(v));
            }
            "additionalItems" if m.get("items").is_some_and(Value::is_array) => {
                out.insert("items".into(), gemini_node(v));
            }
            _ => {
                out.insert(k.clone(), gemini_node(v));
            }
        }
    }
    Value::Object(out)
}

/// Gemini accepts a recursive `$ref` only when the loop passes through an
/// optional property or possibly-empty array `items`; cut the others.
fn cut_loops(schema: &Value, target: &str, safe: bool) -> Value {
    if let Value::Array(a) = schema {
        return Value::Array(a.iter().map(|s| cut_loops(s, target, safe)).collect());
    }
    let Value::Object(m) = schema else {
        return schema.clone();
    };
    if m.get("$ref").and_then(Value::as_str) == Some(target) && !safe {
        return json!({});
    }
    let required: Vec<&str> = m
        .get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    map_values(m, |k, v| {
        if VALUES.contains(&k) {
            return v.clone();
        }
        if k == "items" {
            let may_be_empty = !m
                .get("minItems")
                .and_then(Value::as_f64)
                .is_some_and(|n| n > 0.0);
            return cut_loops(v, target, safe || may_be_empty);
        }
        if let (true, Value::Object(map)) = (SCHEMA_MAPS.contains(&k), v) {
            return if k == "properties" {
                map_values(map, |name, c| {
                    cut_loops(c, target, safe || !required.contains(&name))
                })
            } else {
                map_values(map, |_, c| cut_loops(c, target, safe))
            };
        }
        cut_loops(v, target, safe)
    })
}

/// Gemini's `parametersJsonSchema` takes standard JSON Schema but rejects a
/// few shapes published tool schemas often contain; rewrite only those.
pub fn gemini(schema: &Value) -> Value {
    let Value::Object(mut m) = gemini_node(schema) else {
        return json!({});
    };
    // A dialect marker says nothing about the arguments; leave it out.
    m.remove("$schema");
    let defs = map_values(&m, |k, v| match v {
        Value::Object(d) if k == "$defs" || k == "definitions" => map_values(d, |name, def| {
            cut_loops(def, &format!("#/{k}/{name}"), false)
        }),
        _ => v.clone(),
    });
    match cut_loops(&defs, "#", false) {
        v @ Value::Object(_) => v,
        _ => json!({}),
    }
}

fn tuple_items(items: &[Value]) -> Value {
    let projected: Vec<Value> = items.iter().map(moonshot_node).collect();
    match projected.len() {
        0 => json!({}),
        1 => projected.into_iter().next().unwrap(),
        _ => json!({"anyOf": projected}),
    }
}

/// Moonshot rejects an `enum` without a sibling `type`: one type, or one plus "null".
fn enum_type(values: &[Value]) -> Option<Value> {
    let mut types: Vec<&str> = Vec::new();
    for v in values {
        let t = match v {
            Value::Null => "null",
            Value::Array(_) => "array",
            Value::Bool(_) => "boolean",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Object(_) => "object",
        };
        if !types.contains(&t) {
            types.push(t);
        }
    }
    match types.as_slice() {
        [one] => Some(json!(one)),
        [a, b] if types.contains(&"null") => {
            let other = if *a == "null" { b } else { a };
            Some(json!([other, "null"]))
        }
        _ => None,
    }
}

fn moonshot_node(schema: &Value) -> Value {
    if let Value::Array(a) = schema {
        return Value::Array(a.iter().map(moonshot_node).collect());
    }
    let Value::Object(m) = schema else {
        return schema.clone();
    };
    // `$ref` siblings are rejected.
    if let Some(r) = m.get("$ref").filter(|r| r.is_string()) {
        return json!({"$ref": r});
    }
    let mut out = Map::new();
    if !m.contains_key("type") {
        if let Some(t) = m
            .get("enum")
            .and_then(Value::as_array)
            .and_then(|e| enum_type(e))
        {
            out.insert("type".into(), t);
        }
    }
    for (k, v) in m {
        let key = k.as_str();
        if VALUES.contains(&key) {
            out.insert(k.clone(), v.clone());
            continue;
        }
        if SCHEMA_MAPS.contains(&key) {
            if let Value::Object(map) = v {
                out.insert(k.clone(), map_values(map, |_, s| moonshot_node(s)));
                continue;
            }
        }
        match key {
            "items" if m.get("prefixItems").is_some_and(Value::is_array) => {}
            "items" if v.is_array() => {
                out.insert(k.clone(), tuple_items(v.as_array().unwrap()));
            }
            "prefixItems" => match v {
                Value::Array(prefix) => {
                    let mut all = prefix.clone();
                    match m.get("items") {
                        Some(i @ Value::Object(_)) => all.push(i.clone()),
                        Some(Value::Array(rest)) => all.extend(rest.iter().cloned()),
                        _ => {}
                    }
                    out.insert("items".into(), tuple_items(&all));
                }
                _ if m.contains_key("items") => {}
                _ => {
                    out.insert("items".into(), json!({}));
                }
            },
            "unevaluatedItems" => {}
            _ => {
                out.insert(k.clone(), moonshot_node(v));
            }
        }
    }
    Value::Object(out)
}

pub fn moonshot(schema: &Value) -> Value {
    match moonshot_node(schema) {
        v @ Value::Object(_) => v,
        _ => json!({}),
    }
}
