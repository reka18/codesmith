//! Tool input repair, as OpenCode's tool-input-repair plugin does it before
//! a call's input is checked: fixes for the mistakes models make, applied
//! only where the tool's input schema leaves no doubt.
//!
//! - a stringified root or nested object: `'{"limit":"20"}'` → `{limit: 20}`
//! - a closed object's extras: `{limit: "20", extra: true}` → `{limit: 20}`
//! - an optional null or empty-object placeholder: `{limit: null}` → `{}`
//! - a numeric or boolean string: `{limit: "20", enabled: "false"}`
//! - a nullable field: `{count: "2"}` → `{count: 2}`
//! - a stringified array, or one item for an array: `{tags: '["a"]', count: "2"}`
//!   → `{tags: ["a"], count: [2]}`
//! - tuple positions: `{pair: ["2", "false"]}` → `{pair: [2, false]}`
//! - typed dictionaries: `{counts: {first: "2"}}` → `{counts: {first: 2}}`
//! - nested fields and local references (`#/$defs/x`, `#/definitions/x`)
//!
//! A call to another harness's name for a tool (`bash`), when no tool has
//! that name, runs as the matching tool, and the result says so
//! ([`alias`], [`alias_note`]).

use serde_json::{Map, Value};

/// How deep a schema is followed.
const MAX_DEPTH: usize = 6;

/// Tool names models carry over from other harnesses.
const ALIASES: &[(&str, &str)] = &[("bash", "shell")];

/// The tool to run a call to `name` as: the one another harness's name
/// stands for, when no tool is called `name` and that one is `offered`.
pub fn alias(name: &str, offered: &[String]) -> Option<&'static str> {
    if offered.iter().any(|o| o == name) {
        return None;
    }
    ALIASES
        .iter()
        .find(|(from, to)| *from == name && offered.iter().any(|o| o == to))
        .map(|(_, to)| *to)
}

/// What the result of an aliased call adds (OpenCode's words).
pub fn alias_note(called: &str, ran: &str) -> String {
    format!("There is no \"{called}\" tool, so this ran as \"{ran}\". Call \"{ran}\" directly.")
}

/// `input` repaired against the tool's input `schema`; unchanged unless the
/// schema is an object's.
pub fn repair(input: &Value, schema: &Value) -> Value {
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return input.clone();
    }
    fix(input, schema, schema, 0).unwrap_or_else(|| input.clone())
}

/// JavaScript's `typeof` for a JSON value.
fn type_of(v: &Value) -> &'static str {
    match v {
        Value::String(_) => "string",
        Value::Number(_) => "number",
        Value::Bool(_) => "boolean",
        Value::Null | Value::Array(_) | Value::Object(_) => "object",
    }
}

fn is_obj(v: &Value) -> bool {
    v.is_object()
}

/// The repaired value, or `None` when it stays as it is.
fn fix(value: &Value, schema: &Value, root: &Value, depth: usize) -> Option<Value> {
    if depth > MAX_DEPTH {
        return None;
    }
    let s = schema.as_object()?;
    if let Some(r) = s.get("$ref").and_then(Value::as_str) {
        let defs = if let Some(name) = r.strip_prefix("#/$defs/") {
            (!name.contains('/')).then(|| (root.get("$defs"), name))
        } else if let Some(name) = r.strip_prefix("#/definitions/") {
            (!name.contains('/')).then(|| (root.get("definitions"), name))
        } else {
            None
        };
        let (defs, name) = defs?;
        let name = name.replace("~1", "/").replace("~0", "~");
        let target = defs?.as_object()?.get(&name).filter(|t| is_obj(t))?;
        return fix(value, target, root, depth + 1);
    }
    match s.get("type") {
        Some(Value::Array(types)) => {
            let has = |t: &str| types.iter().any(|x| x.as_str() == Some(t));
            if (value.is_null() && has("null")) || has(type_of(value)) {
                return None;
            }
            let rest: Vec<&Value> = types
                .iter()
                .filter(|t| t.as_str() != Some("null"))
                .collect();
            if rest.len() != 1 {
                return None;
            }
            let mut one = s.clone();
            one.insert("type".into(), rest[0].clone());
            fix(value, &Value::Object(one), root, depth + 1)
        }
        None => {
            if s.get("anyOf").is_some_and(Value::is_array)
                && s.get("oneOf").is_some_and(Value::is_array)
            {
                return None;
            }
            let branches = s
                .get("anyOf")
                .filter(|b| b.is_array())
                .or_else(|| s.get("oneOf"))?
                .as_array()?;
            if value.is_null() {
                return None;
            }
            if branches.iter().any(|b| {
                !is_obj(b) || b.get("type").and_then(Value::as_str) == Some(type_of(value))
            }) {
                return None;
            }
            let candidates: Vec<&Value> = branches
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) != Some("null"))
                .collect();
            match candidates.as_slice() {
                [one] => fix(value, one, root, depth + 1),
                _ => None,
            }
        }
        Some(Value::String(t)) => match t.as_str() {
            "number" | "integer" => {
                let text = value.as_str()?;
                number(text, t == "integer")
            }
            "boolean" => match value.as_str()? {
                "true" => Some(Value::Bool(true)),
                "false" => Some(Value::Bool(false)),
                _ => None,
            },
            "object" => fix_object(value, s, root, depth),
            "array" => fix_array(value, s, root, depth),
            _ => None,
        },
        Some(_) => None,
    }
}

/// JavaScript's `Number(text)`, when finite (and a safe integer for
/// `integer`).
fn number(text: &str, integer: bool) -> Option<Value> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    let radix = |p: &str, r: u32| {
        t.strip_prefix(p)
            .and_then(|d| u64::from_str_radix(d, r).ok())
    };
    let n: f64 = match radix("0x", 16)
        .or_else(|| radix("0X", 16))
        .or_else(|| radix("0o", 8))
        .or_else(|| radix("0b", 2))
    {
        Some(v) => v as f64,
        None => {
            // Rust also reads `inf` and `nan`; JavaScript only `Infinity`.
            if t.chars()
                .any(|c| c.is_ascii_alphabetic() && !matches!(c, 'e' | 'E'))
            {
                return None;
            }
            t.parse().ok()?
        }
    };
    if !n.is_finite() {
        return None;
    }
    const SAFE: f64 = 9_007_199_254_740_991.0;
    let whole = n.fract() == 0.0 && n.abs() <= SAFE;
    if integer && !whole {
        return None;
    }
    Some(if whole {
        Value::from(n as i64)
    } else {
        serde_json::Number::from_f64(n).map(Value::Number)?
    })
}

fn parse_json(value: &Value) -> Option<Value> {
    match value {
        Value::String(s) => serde_json::from_str(s).ok(),
        v => Some(v.clone()),
    }
}

fn fix_object(value: &Value, s: &Map<String, Value>, root: &Value, depth: usize) -> Option<Value> {
    let parsed = parse_json(value)?;
    let Value::Object(map) = &parsed else {
        return None;
    };
    let empty = Map::new();
    let properties = s
        .get("properties")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let required: Vec<&str> = s
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let patterned = s.get("patternProperties").is_some_and(Value::is_object);
    let composed = ["allOf", "anyOf", "oneOf"]
        .iter()
        .any(|k| s.get(*k).is_some_and(Value::is_array));
    let closed = s.get("additionalProperties") == Some(&Value::Bool(false));
    let mut out = map.clone();
    let mut changed = value.is_string();
    for (key, current) in map {
        let declared = properties.contains_key(key);
        let property = if declared {
            properties.get(key)
        } else if !patterned {
            s.get("additionalProperties")
        } else {
            None
        };
        if !declared && closed && !patterned && !composed {
            out.remove(key);
            changed = true;
            continue;
        }
        let Some(property) = property.and_then(Value::as_object) else {
            continue;
        };
        // Only a bare single-type property provably rejects null and `{}`.
        let plain = property
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|t| t != "null")
            && !composed
            && [
                "anyOf", "oneOf", "allOf", "enum", "const", "$ref", "nullable",
            ]
            .iter()
            .all(|k| !property.contains_key(*k));
        let placeholder = current.as_object().is_some_and(Map::is_empty)
            && property.get("type").and_then(Value::as_str) != Some("object");
        if declared
            && !required.contains(&key.as_str())
            && plain
            && (current.is_null() || placeholder)
        {
            out.remove(key);
            changed = true;
            continue;
        }
        if let Some(v) = fix(current, &Value::Object(property.clone()), root, depth + 1) {
            if &v != current {
                out.insert(key.clone(), v);
                changed = true;
            }
        }
    }
    changed.then_some(Value::Object(out))
}

fn fix_array(value: &Value, s: &Map<String, Value>, root: &Value, depth: usize) -> Option<Value> {
    let prefix = s.get("prefixItems").and_then(Value::as_array);
    let tuple = prefix.or_else(|| s.get("items").and_then(Value::as_array));
    let parsed = parse_json(value);
    if let Some(Value::Array(items)) = &parsed {
        let mut changed = value.is_string();
        let out: Vec<Value> = items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let member = match tuple {
                    Some(t) => t.get(i).or_else(|| {
                        if prefix.is_some() {
                            s.get("items")
                        } else {
                            s.get("additionalItems")
                        }
                    }),
                    None => s.get("items"),
                };
                match member
                    .filter(|m| is_obj(m))
                    .and_then(|m| fix(item, m, root, depth + 1))
                {
                    Some(v) if &v != item => {
                        changed = true;
                        v
                    }
                    _ => item.clone(),
                }
            })
            .collect();
        return changed.then_some(Value::Array(out));
    }
    let items = s.get("items").filter(|i| is_obj(i));
    if tuple.is_some() {
        return None;
    }
    let items = items?;
    let repaired = fix(value, items, root, depth + 1).unwrap_or_else(|| value.clone());
    let compatible = match items.get("type").and_then(Value::as_str) {
        Some("object") => repaired.is_object(),
        Some("array") => repaired.is_array(),
        Some("integer") => repaired.is_i64() || repaired.is_u64(),
        Some("number") => repaired.is_number(),
        Some("string") => repaired.is_string(),
        Some("boolean") => repaired.is_boolean(),
        _ => false,
    };
    compatible.then(|| Value::Array(vec![repaired]))
}
