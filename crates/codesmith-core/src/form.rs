//! Forms: structured input asked of the user, as OpenCode's `Form` service
//! keeps them. MCP elicitations become forms (see [`crate::mcp`]); a front
//! end shows one through its question UI ([`present`]) and replies.
//!
//! A form is a list of fields (text, number, integer, boolean, multiselect,
//! and external: a URL the user opens and acknowledges). A field's `when`
//! conditions, all of which must hold against the answers so far, make it
//! active; an inactive field is neither required nor answerable. Against a
//! multiselect answer `eq` means the selection includes the value and `neq`
//! that it does not; an unanswered referenced field fails both, so hiding a
//! field hides the fields that depend on it.
//!
//! [`Forms`] holds the pending ones: [`Forms::ask`] waits for a reply or a
//! cancellation. Listeners see each form created, replied to or cancelled;
//! a listener that fails undoes the change (a created form is dropped, a
//! reply leaves the form pending), as OpenCode's event publication does.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// One choice of a string or multiselect field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Choice {
    pub value: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// One visibility condition (see the module docs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct When {
    pub key: String,
    pub op: Op,
    pub value: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Eq,
    Neq,
}

/// What every input field has.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Base {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<Vec<When>>,
}

/// A form field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Field {
    String {
        #[serde(flatten)]
        base: Base,
        /// `email`, `uri`, `date` or `date-time`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<String>,
        #[serde(default, rename = "minLength", skip_serializing_if = "Option::is_none")]
        min_length: Option<usize>,
        #[serde(default, rename = "maxLength", skip_serializing_if = "Option::is_none")]
        max_length: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pattern: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        options: Option<Vec<Choice>>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        custom: bool,
    },
    Number {
        #[serde(flatten)]
        base: Base,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        minimum: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        maximum: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<f64>,
    },
    Integer {
        #[serde(flatten)]
        base: Base,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        minimum: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        maximum: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<f64>,
    },
    Boolean {
        #[serde(flatten)]
        base: Base,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<bool>,
    },
    Multiselect {
        #[serde(flatten)]
        base: Base,
        options: Vec<Choice>,
        #[serde(default, rename = "minItems", skip_serializing_if = "Option::is_none")]
        min_items: Option<usize>,
        #[serde(default, rename = "maxItems", skip_serializing_if = "Option::is_none")]
        max_items: Option<usize>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        custom: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<Vec<String>>,
    },
    /// A URL to open; answered `true` once the user did what it asks.
    External {
        key: String,
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
}

impl Field {
    pub fn key(&self) -> &str {
        match self {
            Field::External { key, .. } => key,
            Field::String { base, .. }
            | Field::Number { base, .. }
            | Field::Integer { base, .. }
            | Field::Boolean { base, .. }
            | Field::Multiselect { base, .. } => &base.key,
        }
    }

    /// What every input field has; `None` for an external one.
    pub fn base(&self) -> Option<&Base> {
        match self {
            Field::External { .. } => None,
            Field::String { base, .. }
            | Field::Number { base, .. }
            | Field::Integer { base, .. }
            | Field::Boolean { base, .. }
            | Field::Multiselect { base, .. } => Some(base),
        }
    }
}

/// The answers, by field key: strings, numbers, booleans, string lists.
pub type Answer = BTreeMap<String, Value>;

/// A form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Info {
    pub id: String,
    /// Its owner: a session id, or `global` for an MCP elicitation, which
    /// no session can be told apart for (OpenCode's temporary owner).
    pub session_id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    pub fields: Vec<Field>,
}

/// The owner of MCP elicitation forms.
pub const GLOBAL: &str = "global";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum State {
    Pending,
    Answered {
        answer: Answer,
    },
    /// The message says why nobody answered, e.g. nobody could.
    Cancelled {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
}

/// What a form's listeners see.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Created(Info),
    Replied {
        id: String,
        session_id: String,
        answer: Answer,
    },
    Cancelled {
        id: String,
        session_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    NotFound(String),
    AlreadySettled(String),
    AlreadyExists(String),
    InvalidAnswer {
        id: String,
        message: String,
    },
    InvalidForm(String),
    /// A listener failed.
    Listener(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotFound(id) => write!(f, "Form not found: {id}"),
            Error::AlreadySettled(id) => write!(f, "Form already settled: {id}"),
            Error::AlreadyExists(id) => write!(f, "Form already exists: {id}"),
            Error::InvalidAnswer { message, .. } | Error::InvalidForm(message) => {
                f.write_str(message)
            }
            Error::Listener(why) => write!(f, "a form listener failed: {why}"),
        }
    }
}

impl std::error::Error for Error {}

/// What [`Forms::create`] takes; a missing id gets a new one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Create {
    pub id: Option<String>,
    pub session_id: String,
    pub title: String,
    pub metadata: Option<Value>,
    pub fields: Vec<Field>,
}

type Listener = Arc<dyn Fn(&Event) -> Result<(), String> + Send + Sync>;

struct Entry {
    form: Info,
    state: State,
    done: tokio::sync::watch::Sender<Option<State>>,
}

/// The forms of a process (see the module docs).
#[derive(Default, Clone)]
pub struct Forms {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    entries: Vec<Entry>,
    listeners: Vec<(u64, Listener)>,
    next: u64,
}

fn new_id() -> String {
    use rand::Rng;
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!(
        "frm_{t:x}{:06x}",
        rand::thread_rng().gen::<u32>() & 0xffffff
    )
}

/// The forms MCP elicitations create.
pub fn global() -> &'static Forms {
    static G: std::sync::OnceLock<Forms> = std::sync::OnceLock::new();
    G.get_or_init(Forms::default)
}

impl Forms {
    pub fn new() -> Forms {
        Forms::default()
    }

    /// Call `f` for every event; returns its id for [`Forms::unlisten`].
    pub fn listen(&self, f: impl Fn(&Event) -> Result<(), String> + Send + Sync + 'static) -> u64 {
        let mut g = self.inner.lock().unwrap();
        g.next += 1;
        let id = g.next;
        g.listeners.push((id, Arc::new(f)));
        id
    }

    pub fn unlisten(&self, id: u64) {
        self.inner
            .lock()
            .unwrap()
            .listeners
            .retain(|(i, _)| *i != id);
    }

    fn publish(&self, e: &Event) -> Result<(), Error> {
        let listeners: Vec<Listener> = self
            .inner
            .lock()
            .unwrap()
            .listeners
            .iter()
            .map(|(_, l)| l.clone())
            .collect();
        for l in listeners {
            l(e).map_err(Error::Listener)?;
        }
        Ok(())
    }

    pub fn create(&self, input: Create) -> Result<Info, Error> {
        let id = input.id.unwrap_or_else(new_id);
        if let Some(why) = validate_fields(&input.fields) {
            return Err(Error::InvalidForm(why));
        }
        let form = Info {
            id: id.clone(),
            session_id: input.session_id,
            title: input.title,
            metadata: input.metadata,
            fields: input.fields,
        };
        {
            let mut g = self.inner.lock().unwrap();
            if g.entries.iter().any(|e| e.form.id == id) {
                return Err(Error::AlreadyExists(id));
            }
            g.entries.push(Entry {
                form: form.clone(),
                state: State::Pending,
                done: tokio::sync::watch::channel(None).0,
            });
        }
        if let Err(e) = self.publish(&Event::Created(form.clone())) {
            self.inner
                .lock()
                .unwrap()
                .entries
                .retain(|e| e.form.id != id);
            return Err(e);
        }
        Ok(form)
    }

    /// Create the form and wait until it is answered or cancelled.
    pub async fn ask(&self, input: Create) -> Result<State, Error> {
        let form = self.create(input)?;
        self.wait(&form.id).await
    }

    /// Wait until form `id` is answered or cancelled.
    pub async fn wait(&self, id: &str) -> Result<State, Error> {
        let mut rx = {
            let g = self.inner.lock().unwrap();
            let e = g
                .entries
                .iter()
                .find(|e| e.form.id == id)
                .ok_or_else(|| Error::NotFound(id.into()))?;
            if e.state != State::Pending {
                return Ok(e.state.clone());
            }
            e.done.subscribe()
        };
        loop {
            if let Some(s) = rx.borrow_and_update().clone() {
                return Ok(s);
            }
            if rx.changed().await.is_err() {
                return Err(Error::NotFound(id.into()));
            }
        }
    }

    pub fn get(&self, id: &str) -> Result<Info, Error> {
        self.with(id, |e| e.form.clone())
    }

    pub fn state(&self, id: &str) -> Result<State, Error> {
        self.with(id, |e| e.state.clone())
    }

    fn with<T>(&self, id: &str, f: impl FnOnce(&Entry) -> T) -> Result<T, Error> {
        let g = self.inner.lock().unwrap();
        g.entries
            .iter()
            .find(|e| e.form.id == id)
            .map(f)
            .ok_or_else(|| Error::NotFound(id.into()))
    }

    /// The pending forms, of `session` when given.
    pub fn list(&self, session: Option<&str>) -> Vec<Info> {
        let g = self.inner.lock().unwrap();
        g.entries
            .iter()
            .filter(|e| e.state == State::Pending)
            .filter(|e| session.is_none_or(|s| e.form.session_id == s))
            .map(|e| e.form.clone())
            .collect()
    }

    pub fn reply(&self, id: &str, answer: Answer) -> Result<(), Error> {
        let (form, state) = self.with(id, |e| (e.form.clone(), e.state.clone()))?;
        if state != State::Pending {
            return Err(Error::AlreadySettled(id.into()));
        }
        if let Some(message) = validate_answer(&form.fields, &answer) {
            return Err(Error::InvalidAnswer {
                id: id.into(),
                message,
            });
        }
        self.publish(&Event::Replied {
            id: id.into(),
            session_id: form.session_id,
            answer: answer.clone(),
        })?;
        self.settle(id, State::Answered { answer })
    }

    pub fn cancel(&self, id: &str, message: Option<String>) -> Result<(), Error> {
        let (form, state) = self.with(id, |e| (e.form.clone(), e.state.clone()))?;
        if state != State::Pending {
            return Err(Error::AlreadySettled(id.into()));
        }
        self.publish(&Event::Cancelled {
            id: id.into(),
            session_id: form.session_id,
        })?;
        self.settle(id, State::Cancelled { message })
    }

    fn settle(&self, id: &str, state: State) -> Result<(), Error> {
        let mut g = self.inner.lock().unwrap();
        let e = g
            .entries
            .iter_mut()
            .find(|e| e.form.id == id)
            .ok_or_else(|| Error::NotFound(id.into()))?;
        if e.state != State::Pending {
            return Err(Error::AlreadySettled(id.into()));
        }
        e.state = state.clone();
        let _ = e.done.send(Some(state));
        // Settled forms are kept a while for `state`; drop the oldest past a bound.
        let settled = g
            .entries
            .iter()
            .filter(|e| e.state != State::Pending)
            .count();
        if settled > 256 {
            if let Some(i) = g.entries.iter().position(|e| e.state != State::Pending) {
                g.entries.remove(i);
            }
        }
        Ok(())
    }
}

/// Why `answer` does not fit `fields`, if it does not.
pub fn validate_answer(fields: &[Field], answer: &Answer) -> Option<String> {
    for key in answer.keys() {
        if !fields.iter().any(|f| f.key() == key) {
            return Some(format!("Unknown form field: {key}"));
        }
    }
    for field in fields {
        let value = answer.get(field.key());
        let Some(base) = field.base() else {
            if value != Some(&Value::Bool(true)) {
                return Some(format!(
                    "External form field must be acknowledged: {}",
                    field.key()
                ));
            }
            continue;
        };
        let active = is_active(base, answer);
        let Some(value) = value else {
            if base.required && active {
                return Some(format!("Missing required form field: {}", base.key));
            }
            continue;
        };
        if !active {
            return Some(format!("Form field is not active: {}", base.key));
        }
        if let Some(why) = validate_field(field, value) {
            return Some(why);
        }
    }
    None
}

/// Whether the field's conditions hold against `answer`.
pub fn is_active(base: &Base, answer: &Answer) -> bool {
    base.when
        .as_ref()
        .is_none_or(|w| w.iter().all(|c| matches(c, answer.get(&c.key))))
}

fn matches(when: &When, value: Option<&Value>) -> bool {
    let Some(value) = value else {
        return false;
    };
    let hit = match value {
        Value::Array(items) => items.contains(&when.value),
        v => same(v, &when.value),
    };
    match when.op {
        Op::Eq => hit,
        Op::Neq => !hit,
    }
}

/// JSON equality with numbers compared as numbers (`1` is `1.0`).
fn same(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    }
}

/// Why `fields` do not make a form, if they do not: no fields, a repeated
/// key, or a condition on a later or missing field, with a value of the
/// wrong type or not among the field's closed options.
pub fn validate_fields(fields: &[Field]) -> Option<String> {
    if fields.is_empty() {
        return Some("Form must have at least one field".into());
    }
    let mut earlier: Vec<&Field> = Vec::new();
    let mut keys: Vec<&str> = Vec::new();
    for field in fields {
        if keys.contains(&field.key()) {
            return Some(format!("Duplicate form field key: {}", field.key()));
        }
        keys.push(field.key());
        let Some(base) = field.base() else {
            continue;
        };
        for when in base.when.iter().flatten() {
            let Some(target) = earlier.iter().find(|f| f.key() == when.key) else {
                return Some(format!(
                    "Form field condition must reference an earlier field: {} -> {}",
                    base.key, when.key
                ));
            };
            if let Some(why) = validate_when(when, target) {
                return Some(format!("{why}: {} -> {}", base.key, when.key));
            }
        }
        earlier.push(field);
    }
    None
}

fn validate_when(when: &When, target: &Field) -> Option<&'static str> {
    let (options, custom) = match target {
        Field::Boolean { .. } => {
            return (!when.value.is_boolean())
                .then_some("Form field condition value must be a boolean")
        }
        Field::Number { .. } | Field::Integer { .. } => {
            return (!when.value.is_number())
                .then_some("Form field condition value must be a number")
        }
        Field::String {
            options, custom, ..
        } => (options.as_deref(), *custom),
        Field::Multiselect {
            options, custom, ..
        } => (Some(options.as_slice()), *custom),
        Field::External { .. } => return None,
    };
    let Some(v) = when.value.as_str() else {
        return Some("Form field condition value must be a string");
    };
    match options {
        Some(o) if !custom && !o.iter().any(|c| c.value == v) => {
            Some("Form field condition value must be one of the field's options")
        }
        _ => None,
    }
}

fn validate_field(field: &Field, value: &Value) -> Option<String> {
    match field {
        Field::String {
            base,
            format,
            min_length,
            max_length,
            pattern,
            options,
            custom,
            ..
        } => {
            let key = &base.key;
            let Some(s) = value.as_str() else {
                return Some(format!("Expected string for form field: {key}"));
            };
            let len = s.encode_utf16().count();
            if base.required && s.is_empty() {
                return Some(format!("Missing required form field: {key}"));
            }
            if min_length.is_some_and(|m| len < m) {
                return Some(format!("Form field is too short: {key}"));
            }
            if max_length.is_some_and(|m| len > m) {
                return Some(format!("Form field is too long: {key}"));
            }
            if let Some(p) = pattern {
                match regex::Regex::new(p) {
                    Ok(re) if !re.is_match(s) => {
                        return Some(format!("Form field does not match pattern: {key}"))
                    }
                    Err(_) => return Some(format!("Form field has invalid pattern: {key}")),
                    Ok(_) => {}
                }
            }
            match format.as_deref() {
                Some("email") if !is_email(s) => {
                    return Some(format!("Expected email for form field: {key}"))
                }
                Some("uri") if url::Url::parse(s).is_err() => {
                    return Some(format!("Expected URI for form field: {key}"))
                }
                Some("date") if !is_date(s) => {
                    return Some(format!("Expected date for form field: {key}"))
                }
                Some("date-time") if chrono::DateTime::parse_from_rfc3339(s).is_err() => {
                    return Some(format!("Expected date-time for form field: {key}"))
                }
                _ => {}
            }
            if let Some(o) = options {
                if !custom && !o.iter().any(|c| c.value == s) {
                    return Some(format!("Invalid option for form field: {key}"));
                }
            }
            None
        }
        Field::Number {
            base,
            minimum,
            maximum,
            ..
        }
        | Field::Integer {
            base,
            minimum,
            maximum,
            ..
        } => {
            let key = &base.key;
            let Some(n) = value.as_f64().filter(|n| n.is_finite()) else {
                return Some(format!("Expected number for form field: {key}"));
            };
            if matches!(field, Field::Integer { .. }) && n.fract() != 0.0 {
                return Some(format!("Expected integer for form field: {key}"));
            }
            if minimum.is_some_and(|m| n < m) {
                return Some(format!("Form field is too small: {key}"));
            }
            if maximum.is_some_and(|m| n > m) {
                return Some(format!("Form field is too large: {key}"));
            }
            None
        }
        Field::Boolean { base, .. } => {
            (!value.is_boolean()).then(|| format!("Expected boolean for form field: {}", base.key))
        }
        Field::Multiselect {
            base,
            options,
            min_items,
            max_items,
            custom,
            ..
        } => {
            let key = &base.key;
            let items: Option<Vec<&str>> = value
                .as_array()
                .and_then(|a| a.iter().map(Value::as_str).collect());
            let Some(items) = items else {
                return Some(format!("Expected string array for form field: {key}"));
            };
            if base.required && items.is_empty() {
                return Some(format!("Missing required form field: {key}"));
            }
            if min_items.is_some_and(|m| items.len() < m) {
                return Some(format!("Too few selections for form field: {key}"));
            }
            if max_items.is_some_and(|m| items.len() > m) {
                return Some(format!("Too many selections for form field: {key}"));
            }
            if !custom && items.iter().any(|i| !options.iter().any(|o| o.value == *i)) {
                return Some(format!("Invalid option for form field: {key}"));
            }
            None
        }
        Field::External { .. } => None,
    }
}

fn is_email(s: &str) -> bool {
    let Some((local, domain)) = s.split_once('@') else {
        return false;
    };
    let bad = |p: &str| p.is_empty() || p.chars().any(|c| c.is_whitespace() || c == '@');
    !bad(local)
        && !bad(domain)
        && domain
            .split_once('.')
            .is_some_and(|(a, b)| !a.is_empty() && !b.is_empty())
}

fn is_date(s: &str) -> bool {
    s.len() == 10
        && s.as_bytes()[4] == b'-'
        && s.as_bytes()[7] == b'-'
        && chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok()
}

// --- Asking through the question UI ---

/// The question for each field `form` asks, in order.
fn prompt_for(form: &Info, field: &Field, first: bool) -> crate::question::Prompt {
    use crate::question::{Choice as C, Prompt};
    let intro = form
        .metadata
        .as_ref()
        .and_then(|m| m["message"].as_str())
        .filter(|_| first)
        .map(|m| format!("{m}\n\n"))
        .unwrap_or_default();
    let label = |title: &Option<String>, key: &str, description: &Option<String>| {
        let t = title.clone().unwrap_or_else(|| key.to_string());
        match description {
            Some(d) if *d != t => format!("{intro}{t}: {d}"),
            _ => format!("{intro}{t}"),
        }
    };
    let choices = |o: &[Choice]| -> Vec<C> {
        o.iter()
            .map(|c| C {
                label: c.label.clone(),
                description: c.description.clone().unwrap_or_default(),
            })
            .collect()
    };
    match field {
        Field::External {
            key,
            url,
            title,
            description,
        } => Prompt {
            question: format!(
                "{}\nOpen {url}, then confirm here.",
                label(title, key, description)
            ),
            header: form.title.clone(),
            options: vec![C {
                label: "Done".into(),
                description: format!("I opened {url} and finished there"),
            }],
            multiple: false,
        },
        Field::Boolean { base, .. } => Prompt {
            question: label(&base.title, &base.key, &base.description),
            header: form.title.clone(),
            options: vec![
                C {
                    label: "Yes".into(),
                    description: String::new(),
                },
                C {
                    label: "No".into(),
                    description: String::new(),
                },
            ],
            multiple: false,
        },
        Field::Multiselect { base, options, .. } => Prompt {
            question: label(&base.title, &base.key, &base.description),
            header: form.title.clone(),
            options: choices(options),
            multiple: true,
        },
        Field::String { base, options, .. } => Prompt {
            question: label(&base.title, &base.key, &base.description),
            header: form.title.clone(),
            options: options.as_deref().map(choices).unwrap_or_default(),
            multiple: false,
        },
        Field::Number { base, .. } | Field::Integer { base, .. } => Prompt {
            question: label(&base.title, &base.key, &base.description),
            header: form.title.clone(),
            options: vec![],
            multiple: false,
        },
    }
}

/// A field's answer from the question UI's: `None` when left unanswered.
fn value_of(field: &Field, answer: &[String]) -> Option<Value> {
    let first = answer.first().map(|s| s.trim()).filter(|s| !s.is_empty());
    let by_label = |o: &[Choice], s: &str| {
        o.iter()
            .find(|c| c.label == s)
            .map_or_else(|| s.to_string(), |c| c.value.clone())
    };
    match field {
        Field::External { .. } => Some(Value::Bool(first == Some("Done"))),
        Field::Boolean { .. } => match first? {
            "Yes" | "yes" | "y" | "true" => Some(Value::Bool(true)),
            "No" | "no" | "n" | "false" => Some(Value::Bool(false)),
            other => Some(Value::String(other.into())),
        },
        Field::Multiselect { options, .. } => Some(Value::Array(
            answer
                .iter()
                .filter(|s| !s.trim().is_empty())
                .map(|s| Value::String(by_label(options, s.trim())))
                .collect(),
        )),
        Field::String { options, .. } => {
            let s = first?;
            Some(Value::String(match options {
                Some(o) => by_label(o, s),
                None => s.to_string(),
            }))
        }
        Field::Number { .. } | Field::Integer { .. } => {
            let s = first?;
            Some(match s.parse::<f64>() {
                Ok(n) if n.fract() == 0.0 && n.abs() < 9.0e15 => Value::from(n as i64),
                Ok(n) => serde_json::Number::from_f64(n)
                    .map(Value::Number)
                    .unwrap_or(Value::String(s.into())),
                Err(_) => Value::String(s.into()),
            })
        }
    }
}

/// Ask `form` through `asker`, one round per set of newly active fields:
/// the answers, or `None` when the user dismissed it.
pub fn present(form: &Info, asker: &dyn crate::question::Questioner) -> Option<Answer> {
    let mut answer = Answer::new();
    let mut asked: Vec<&str> = Vec::new();
    loop {
        let round: Vec<&Field> = form
            .fields
            .iter()
            .filter(|f| !asked.contains(&f.key()))
            .filter(|f| f.base().is_none_or(|b| is_active(b, &answer)))
            .collect();
        if round.is_empty() {
            return Some(answer);
        }
        let prompts: Vec<_> = round
            .iter()
            .enumerate()
            .map(|(i, f)| prompt_for(form, f, asked.is_empty() && i == 0))
            .collect();
        let replies = asker.ask(&prompts)?;
        for (i, f) in round.iter().enumerate() {
            asked.push(f.key());
            let reply = replies.get(i).cloned().unwrap_or_default();
            if let Some(v) = value_of(f, &reply) {
                answer.insert(f.key().to_string(), v);
            }
        }
    }
}

/// Ask form `id` of `forms` through `asker` on a blocking thread, and reply
/// with what the user said (or cancel when they dismissed it or their
/// answers do not fit). The form may be settled elsewhere meanwhile.
pub fn present_in_background(
    forms: Forms,
    id: String,
    asker: Arc<dyn crate::question::Questioner>,
) {
    let Ok(form) = forms.get(&id) else {
        return;
    };
    tokio::task::spawn_blocking(move || {
        if forms.state(&id) != Ok(State::Pending) {
            return;
        }
        match present(&form, asker.as_ref()) {
            Some(answer) => {
                if let Err(Error::InvalidAnswer { message, .. }) = forms.reply(&id, answer) {
                    let _ = forms.cancel(&id, Some(message));
                }
            }
            None => {
                let _ = forms.cancel(&id, Some("dismissed".into()));
            }
        }
    });
}
