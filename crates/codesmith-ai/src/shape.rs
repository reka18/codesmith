//! Type checks for the fields of recognized stream events, after OpenCode's
//! event schemas: a field CodeSmith reads that has the wrong JSON type makes
//! the event invalid (a non-retryable [`Error::Decode`]), while unknown
//! fields, unknown event types and absent fields pass.

use crate::Error;
use serde_json::Value;

/// Whether `v[k]` is absent, `null` when `null` is allowed, or passes `ok`.
pub(crate) fn field(v: &Value, k: &str, null: bool, ok: impl Fn(&Value) -> bool) -> bool {
    match v.get(k) {
        None => true,
        Some(Value::Null) => null,
        Some(x) => ok(x),
    }
}

/// Absent or a string (`Schema.optional(Schema.String)`).
pub(crate) fn opt_str(v: &Value, k: &str) -> bool {
    field(v, k, false, Value::is_string)
}

/// Absent or a number.
pub(crate) fn opt_num(v: &Value, k: &str) -> bool {
    field(v, k, false, Value::is_number)
}

/// Absent, `null` or a string (`optionalNull(Schema.String)`).
pub(crate) fn null_str(v: &Value, k: &str) -> bool {
    field(v, k, true, Value::is_string)
}

/// Absent, `null` or a number.
pub(crate) fn null_num(v: &Value, k: &str) -> bool {
    field(v, k, true, Value::is_number)
}

/// Every key in `keys` passes `check`.
pub(crate) fn all(v: &Value, keys: &[&str], check: fn(&Value, &str) -> bool) -> bool {
    keys.iter().all(|k| check(v, k))
}

/// Absent, `null` (when allowed) or an array whose items all pass `ok`.
pub(crate) fn array_of(v: &Value, k: &str, null: bool, ok: impl Fn(&Value) -> bool) -> bool {
    field(v, k, null, |a| {
        a.as_array().is_some_and(|a| a.iter().all(&ok))
    })
}

/// The error for an event that fails its schema (OpenCode's
/// `Invalid <route> stream event`), with the event for context.
pub(crate) fn invalid(route: &str, event: &Value) -> Error {
    Error::Decode(format!("Invalid {route} stream event: {event}"))
}
