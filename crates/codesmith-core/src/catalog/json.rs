//! JSON that keeps object keys in document order.
//!
//! The catalog's order carries meaning (a model's first API is its default,
//! the first cache TTL is the one kept), and `serde_json::Value` sorts keys,
//! so the catalog is read into this instead.

use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{Serialize, SerializeMap, SerializeSeq, Serializer};
use serde_json::{Map, Value};
use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(entries) => entries.iter().rev().find(|(k, _)| k == key).map(|e| &e.1),
            _ => None,
        }
    }

    pub fn str(&self) -> Option<&str> {
        match self {
            Json::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// A finite number.
    pub fn num(&self) -> Option<f64> {
        match self {
            Json::Number(n) => n.as_f64().filter(|f| f.is_finite()),
            _ => None,
        }
    }

    pub fn array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(a) => Some(a),
            _ => None,
        }
    }

    /// An object's entries in document order (a repeated key keeps its last
    /// value, in its first place, as `JSON.parse` does).
    pub fn entries(&self) -> Option<&[(String, Json)]> {
        match self {
            Json::Object(o) => Some(o),
            _ => None,
        }
    }

    pub fn is_object(&self) -> bool {
        matches!(self, Json::Object(_))
    }

    /// A list of strings; anything else in it fails the whole list.
    pub fn strings(&self) -> Option<Vec<String>> {
        self.array()?
            .iter()
            .map(|v| v.str().map(str::to_string))
            .collect()
    }

    /// The plain JSON value (keys sorted).
    pub fn to_value(&self) -> Value {
        match self {
            Json::Null => Value::Null,
            Json::Bool(b) => Value::Bool(*b),
            Json::Number(n) => Value::Number(n.clone()),
            Json::String(s) => Value::String(s.clone()),
            Json::Array(a) => Value::Array(a.iter().map(Json::to_value).collect()),
            Json::Object(o) => Value::Object(
                o.iter()
                    .map(|(k, v)| (k.clone(), v.to_value()))
                    .collect::<Map<_, _>>(),
            ),
        }
    }

    pub fn parse(text: &str) -> serde_json::Result<Json> {
        serde_json::from_str(text)
    }
}

impl From<&Value> for Json {
    fn from(v: &Value) -> Json {
        match v {
            Value::Null => Json::Null,
            Value::Bool(b) => Json::Bool(*b),
            Value::Number(n) => Json::Number(n.clone()),
            Value::String(s) => Json::String(s.clone()),
            Value::Array(a) => Json::Array(a.iter().map(Json::from).collect()),
            Value::Object(o) => {
                Json::Object(o.iter().map(|(k, v)| (k.clone(), Json::from(v))).collect())
            }
        }
    }
}

impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Json, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Json;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("JSON")
            }
            fn visit_unit<E>(self) -> Result<Json, E> {
                Ok(Json::Null)
            }
            fn visit_none<E>(self) -> Result<Json, E> {
                Ok(Json::Null)
            }
            fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Json, D::Error> {
                Json::deserialize(d)
            }
            fn visit_bool<E>(self, b: bool) -> Result<Json, E> {
                Ok(Json::Bool(b))
            }
            fn visit_i64<E>(self, n: i64) -> Result<Json, E> {
                Ok(Json::Number(n.into()))
            }
            fn visit_u64<E>(self, n: u64) -> Result<Json, E> {
                Ok(Json::Number(n.into()))
            }
            fn visit_f64<E>(self, n: f64) -> Result<Json, E> {
                Ok(serde_json::Number::from_f64(n).map_or(Json::Null, Json::Number))
            }
            fn visit_str<E>(self, s: &str) -> Result<Json, E> {
                Ok(Json::String(s.to_string()))
            }
            fn visit_string<E>(self, s: String) -> Result<Json, E> {
                Ok(Json::String(s))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Json, A::Error> {
                let mut out = Vec::new();
                while let Some(v) = seq.next_element()? {
                    out.push(v);
                }
                Ok(Json::Array(out))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Json, A::Error> {
                let mut out: Vec<(String, Json)> = Vec::new();
                // Where each key is, so duplicates (the last wins) stay linear.
                let mut at: std::collections::HashMap<String, usize> = Default::default();
                while let Some((k, v)) = map.next_entry::<String, Json>()? {
                    match at.get(&k) {
                        Some(&i) => out[i].1 = v,
                        None => {
                            at.insert(k.clone(), out.len());
                            out.push((k, v));
                        }
                    }
                }
                Ok(Json::Object(out))
            }
        }
        d.deserialize_any(V)
    }
}

impl Serialize for Json {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Json::Null => s.serialize_unit(),
            Json::Bool(b) => s.serialize_bool(*b),
            Json::Number(n) => n.serialize(s),
            Json::String(v) => s.serialize_str(v),
            Json::Array(a) => {
                let mut seq = s.serialize_seq(Some(a.len()))?;
                for v in a {
                    seq.serialize_element(v)?;
                }
                seq.end()
            }
            Json::Object(o) => {
                let mut map = s.serialize_map(Some(o.len()))?;
                for (k, v) in o {
                    map.serialize_entry(k, v)?;
                }
                map.end()
            }
        }
    }
}
