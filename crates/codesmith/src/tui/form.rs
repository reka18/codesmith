//! The question form, as OpenCode's `FormPrompt`: one tab per field and a
//! Submit tab, choices with a cursor, multi-select checkboxes, a "Type your
//! own answer" row with its editor, text and number fields, external links
//! to acknowledge, and a compact review before submitting. The question
//! tool's prompts become fields as OpenCode's question tool makes them
//! (`q0`, `q1`, … with custom answers); MCP elicitations arrive whole.
//!
//! Pure: keys and pastes go in, an [`Outcome`] comes out, and [`Form::lines`]
//! draws it. The helpers at the top are OpenCode's `util/form`.

use super::editor::Editor;
use super::keymap::{self, Keymap};
use super::theme;
use codesmith_core::form::{Answer, Field, Info};
use codesmith_core::question::Prompt;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// A choice row: its value, label and description.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub value: Value,
    pub label: String,
    pub description: Option<String>,
}

/// The field's label: its title, or its key (an external field's URL).
pub fn label(f: &Field) -> String {
    match f {
        Field::External { title, url, .. } => title.clone().unwrap_or_else(|| url.clone()),
        _ => {
            let b = f.base().expect("an input field");
            b.title.clone().unwrap_or_else(|| b.key.clone())
        }
    }
}

fn default_of(f: &Field) -> Option<Value> {
    match f {
        Field::String { default, .. } => default.clone().map(Value::String),
        Field::Number { default, .. } | Field::Integer { default, .. } => {
            default.and_then(|n| serde_json::Number::from_f64(n).map(Value::Number))
        }
        Field::Boolean { default, .. } => default.map(Value::Bool),
        Field::Multiselect { default, .. } => default
            .clone()
            .map(|d| Value::Array(d.into_iter().map(Value::String).collect())),
        Field::External { .. } => None,
    }
}

/// The configured defaults, and the ones that are custom answers.
pub fn initial_values(fields: &[Field]) -> (BTreeMap<String, Value>, BTreeMap<String, String>) {
    let answers = fields
        .iter()
        .filter_map(|f| default_of(f).map(|v| (f.key().to_string(), v)))
        .collect();
    let custom = fields
        .iter()
        .filter_map(|f| match f {
            Field::String {
                base,
                options: Some(o),
                custom: true,
                default: Some(d),
                ..
            } if !o.iter().any(|c| c.value == *d) => Some((base.key.clone(), d.clone())),
            _ => None,
        })
        .collect();
    (answers, custom)
}

/// Whether the field is answered by typing.
pub fn textual(f: Option<&Field>) -> bool {
    matches!(
        f,
        Some(Field::Number { .. } | Field::Integer { .. } | Field::String { options: None, .. })
    )
}

/// Whether the field takes a custom answer beside its choices.
pub fn custom(f: Option<&Field>) -> bool {
    matches!(
        f,
        Some(Field::String {
            options: Some(_),
            custom: true,
            ..
        }) | Some(Field::Multiselect { custom: true, .. })
    )
}

pub fn rows(f: Option<&Field>) -> Vec<Row> {
    let options = match f {
        Some(Field::Boolean { .. }) => {
            return vec![
                Row {
                    value: Value::Bool(true),
                    label: "Yes".into(),
                    description: None,
                },
                Row {
                    value: Value::Bool(false),
                    label: "No".into(),
                    description: None,
                },
            ]
        }
        Some(Field::Multiselect { options, .. })
        | Some(Field::String {
            options: Some(options),
            ..
        }) => options,
        _ => return vec![],
    };
    options
        .iter()
        .map(|o| Row {
            value: Value::String(o.value.clone()),
            label: o.label.clone(),
            description: o.description.clone(),
        })
        .collect()
}

/// The row a value puts the cursor on (the custom row for a custom answer).
pub fn selected(f: Option<&Field>, value: Option<&Value>) -> usize {
    let Some(v) = value.filter(|v| !v.is_array()) else {
        return 0;
    };
    let rows = rows(f);
    if let Some(i) = rows.iter().position(|r| r.value == *v) {
        return i;
    }
    if v.is_string() && custom(f) {
        return rows.len();
    }
    0
}

fn valid_date(s: &str) -> bool {
    s.len() == 10
        && s.as_bytes()[4] == b'-'
        && s.as_bytes()[7] == b'-'
        && chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok()
}

fn valid_email(s: &str) -> bool {
    let Some((local, domain)) = s.split_once('@') else {
        return false;
    };
    let bad = |p: &str| p.is_empty() || p.chars().any(|c| c.is_whitespace() || c == '@');
    !bad(local)
        && !bad(domain)
        && domain
            .rsplit_once('.')
            .is_some_and(|(a, b)| !a.is_empty() && !b.is_empty())
}

fn fmt_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        n.to_string()
    }
}

/// Why `value` does not answer the field, as OpenCode words it.
pub fn validate(f: &Field, value: Option<&Value>) -> Option<String> {
    let base = f.base()?;
    let Some(value) = value else {
        return base.required.then(|| "Answer required".into());
    };
    let empty = value.as_str() == Some("") || value.as_array().is_some_and(|a| a.is_empty());
    if base.required && empty {
        return Some(if matches!(f, Field::Multiselect { .. }) {
            "Select at least one option".into()
        } else {
            "Answer required".into()
        });
    }
    match f {
        Field::String {
            format,
            min_length,
            max_length,
            pattern,
            options,
            custom,
            ..
        } => {
            let Some(s) = value.as_str() else {
                return Some("Expected text".into());
            };
            let len = s.encode_utf16().count();
            if let Some(m) = min_length.filter(|m| len < *m) {
                return Some(format!("Must be at least {m} characters"));
            }
            if let Some(m) = max_length.filter(|m| len > *m) {
                return Some(format!("Must be at most {m} characters"));
            }
            if let Some(p) = pattern {
                match regex::Regex::new(p) {
                    Ok(re) if !re.is_match(s) => return Some(format!("Must match pattern: {p}")),
                    Err(_) => return Some(format!("Invalid pattern: {p}")),
                    Ok(_) => {}
                }
            }
            match format.as_deref() {
                Some("email") if !valid_email(s) => {
                    return Some("Expected an email address".into())
                }
                Some("uri") if reqwest::Url::parse(s).is_err() => {
                    return Some("Expected a URL".into())
                }
                Some("date") if !valid_date(s) => {
                    return Some("Expected a date (YYYY-MM-DD)".into())
                }
                Some("date-time") if chrono::DateTime::parse_from_rfc3339(s).is_err() => {
                    return Some("Expected a date and time".into())
                }
                _ => {}
            }
            match options {
                Some(o) if !custom && !o.iter().any(|c| c.value == s) => {
                    Some("Select an available option".into())
                }
                _ => None,
            }
        }
        Field::Number {
            minimum, maximum, ..
        }
        | Field::Integer {
            minimum, maximum, ..
        } => {
            let Some(n) = value.as_f64().filter(|n| n.is_finite()) else {
                return Some("Expected a number".into());
            };
            if matches!(f, Field::Integer { .. }) && n.fract() != 0.0 {
                return Some("Expected an integer".into());
            }
            if let Some(m) = minimum.filter(|m| n < *m) {
                return Some(format!("Must be at least {}", fmt_num(m)));
            }
            if let Some(m) = maximum.filter(|m| n > *m) {
                return Some(format!("Must be at most {}", fmt_num(m)));
            }
            None
        }
        Field::Boolean { .. } => (!value.is_boolean()).then(|| "Expected yes or no".into()),
        Field::Multiselect {
            options,
            min_items,
            max_items,
            custom,
            ..
        } => {
            let Some(items) = value.as_array() else {
                return Some("Expected selections".into());
            };
            if let Some(m) = min_items.filter(|m| items.len() < *m) {
                return Some(format!("Select at least {m}"));
            }
            if let Some(m) = max_items.filter(|m| items.len() > *m) {
                return Some(format!("Select at most {m}"));
            }
            let known = |i: &Value| options.iter().any(|o| i.as_str() == Some(&o.value));
            (!custom && !items.iter().all(known)).then(|| "Select only available options".into())
        }
        Field::External { .. } => None,
    }
}

fn plain(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.as_f64().map_or_else(|| n.to_string(), fmt_num),
        v => v.to_string(),
    }
}

/// The answer as the review shows it: labels for choices.
pub fn display(f: &Field, value: Option<&Value>, empty_multi: &str) -> String {
    let Some(value) = value else {
        return String::new();
    };
    let rows = rows(Some(f));
    let label = |v: &Value| {
        rows.iter()
            .find(|r| r.value == *v)
            .map_or_else(|| plain(v), |r| r.label.clone())
    };
    match value {
        Value::Array(a) if a.is_empty() => empty_multi.into(),
        Value::Array(a) => a.iter().map(label).collect::<Vec<_>>().join(", "),
        v => label(v),
    }
}

fn items(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// The selection with `item` added, or taken out.
pub fn toggle(value: Option<&Value>, item: &str) -> Vec<String> {
    let mut v = items(value);
    match v.iter().position(|i| i == item) {
        Some(i) => {
            v.remove(i);
        }
        None => v.push(item.into()),
    }
    v
}

/// The selection with the custom answer `previous` replaced by `next`.
pub fn set_custom(value: Option<&Value>, previous: Option<&str>, next: &str) -> Vec<String> {
    let mut v = items(value);
    if let Some(i) = previous.and_then(|p| v.iter().position(|i| i == p)) {
        v.remove(i);
    }
    if !next.is_empty() && !v.iter().any(|i| i == next) {
        v.push(next.into());
    }
    v
}

fn list(v: Vec<String>) -> Value {
    Value::Array(v.into_iter().map(Value::String).collect())
}

/// OpenCode's question tool's field for a prompt.
pub fn question_field(p: &Prompt, i: usize) -> Field {
    let base = codesmith_core::form::Base {
        key: format!("q{i}"),
        title: (!p.header.is_empty()).then(|| p.header.clone()),
        description: Some(p.question.clone()),
        ..Default::default()
    };
    let options: Vec<_> = p
        .options
        .iter()
        .map(|o| codesmith_core::form::Choice {
            value: o.label.clone(),
            label: o.label.clone(),
            description: (!o.description.is_empty()).then(|| o.description.clone()),
        })
        .collect();
    if p.multiple {
        Field::Multiselect {
            base,
            options,
            min_items: None,
            max_items: None,
            custom: true,
            default: None,
        }
    } else {
        Field::String {
            base,
            format: None,
            min_length: None,
            max_length: None,
            pattern: None,
            default: None,
            options: (!options.is_empty()).then_some(options),
            custom: true,
        }
    }
}

/// The question tool's answers (by prompt) out of a form answer.
pub fn question_answers(n: usize, answer: &Answer) -> Vec<Vec<String>> {
    (0..n)
        .map(|i| match answer.get(&format!("q{i}")) {
            None => vec![],
            Some(Value::Array(a)) => a.iter().map(plain).collect(),
            Some(v) => vec![plain(v)],
        })
        .collect()
}

/// What a key or paste did.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Nothing,
    Reply(Answer),
    Cancel,
    /// Copy this URL (an external field's).
    Copy(String),
    /// Open this URL in the browser.
    Open(String),
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let t: String = s.chars().take(max - 1).collect();
        format!("{}…", t.trim_end())
    } else {
        s.to_string()
    }
}

pub struct Form {
    pub title: String,
    pub message: Option<String>,
    pub fields: Vec<Field>,
    pub tab: usize,
    pub answers: BTreeMap<String, Value>,
    pub custom: BTreeMap<String, String>,
    ready: BTreeSet<String>,
    pub selected: usize,
    pub editing: bool,
    pub error: String,
    /// The answer editor (a text field's, or a custom answer's).
    pub editor: Editor,
    /// The review's first row.
    scroll: usize,
}

impl Form {
    pub fn new(info: &Info) -> Form {
        let (answers, custom) = initial_values(&info.fields);
        let message = info
            .metadata
            .as_ref()
            .and_then(|m| m["message"].as_str())
            .map(str::to_string);
        let mut f = Form {
            title: info.title.clone(),
            message,
            fields: info.fields.clone(),
            tab: 0,
            answers,
            custom,
            ready: BTreeSet::new(),
            selected: 0,
            editing: false,
            error: String::new(),
            editor: Editor::default(),
            scroll: 0,
        };
        f.select_tab(0);
        f
    }

    /// The question tool's form for `prompts`.
    pub fn questions(prompts: &[Prompt]) -> Form {
        Form::new(&Info {
            id: String::new(),
            session_id: String::new(),
            title: "Questions".into(),
            metadata: None,
            fields: prompts
                .iter()
                .enumerate()
                .map(|(i, p)| question_field(p, i))
                .collect(),
        })
    }

    /// The fields whose conditions hold (external ones always).
    pub fn active(&self) -> Vec<&Field> {
        let mut seen = Answer::new();
        self.fields
            .iter()
            .filter(|f| {
                let Some(b) = f.base() else {
                    return true;
                };
                let on = codesmith_core::form::is_active(b, &seen);
                if on {
                    if let Some(v) = self.answers.get(&b.key) {
                        seen.insert(b.key.clone(), v.clone());
                    }
                }
                on
            })
            .collect()
    }

    fn field(&self) -> Option<&Field> {
        self.active().get(self.tab).copied()
    }

    fn answer_field(&self) -> Option<Field> {
        self.field().filter(|f| f.base().is_some()).cloned()
    }

    fn external(&self) -> Option<(String, String)> {
        match self.field() {
            Some(Field::External { key, url, .. }) => Some((key.clone(), url.clone())),
            _ => None,
        }
    }

    pub fn single(&self) -> bool {
        let a = self.active();
        a.len() == 1
            && matches!(
                a[0],
                Field::Boolean { .. }
                    | Field::String {
                        options: Some(_),
                        ..
                    }
            )
    }

    fn tabs(&self) -> usize {
        if self.single() {
            1
        } else {
            self.active().len() + 1
        }
    }

    pub fn confirm(&self) -> bool {
        !self.single() && self.tab >= self.active().len()
    }

    fn rows(&self) -> Vec<Row> {
        let Some(f) = self.answer_field() else {
            return vec![];
        };
        let mut rows = rows(Some(&f));
        if let (Field::Multiselect { .. }, Some(Value::Array(v))) = (&f, self.answers.get(f.key()))
        {
            let mine = self.custom.get(f.key());
            for i in v {
                let s = plain(i);
                if !rows.iter().any(|r| r.value == *i) && mine != Some(&s) {
                    rows.push(Row {
                        value: i.clone(),
                        label: s,
                        description: None,
                    });
                }
            }
        }
        rows
    }

    fn textual(&self) -> bool {
        !self.confirm() && textual(self.answer_field().as_ref())
    }

    fn has_custom(&self) -> bool {
        custom(self.answer_field().as_ref())
    }

    fn multi(&self) -> bool {
        matches!(self.answer_field(), Some(Field::Multiselect { .. }))
    }

    fn other(&self) -> bool {
        self.has_custom() && self.selected == self.rows().len()
    }

    fn input(&self) -> String {
        self.answer_field()
            .and_then(|f| self.custom.get(f.key()).cloned())
            .unwrap_or_default()
    }

    fn custom_picked(&self) -> bool {
        let v = self.input();
        if v.is_empty() {
            return false;
        }
        let key = self.answer_field().map(|f| f.key().to_string());
        match key.and_then(|k| self.answers.get(&k)) {
            Some(Value::Array(a)) => a.iter().any(|i| i.as_str() == Some(&v)),
            Some(a) => a.as_str() == Some(&v),
            None => false,
        }
    }

    fn completed(&self, f: &Field) -> bool {
        match self.answers.get(f.key()) {
            None => false,
            Some(v) if f.base().is_none() => *v == Value::Bool(true),
            Some(v) => validate(f, Some(v)).is_none(),
        }
    }

    fn set_answer(&mut self, key: &str, value: Option<Value>) {
        let optional_multi = self.active().iter().any(
            |f| matches!(f, Field::Multiselect { base, .. } if base.key == key && !base.required),
        );
        let value =
            value.filter(|v| !(optional_multi && v.as_array().is_some_and(|a| a.is_empty())));
        match value {
            Some(v) => {
                self.answers.insert(key.into(), v);
            }
            None => {
                self.answers.remove(key);
            }
        }
        self.error.clear();
    }

    fn select_tab(&mut self, i: usize) {
        self.tab = i;
        let next = self.active().get(i).map(|f| (*f).clone());
        self.selected = next
            .as_ref()
            .filter(|f| f.base().is_some())
            .map_or(0, |f| selected(Some(f), self.answers.get(f.key())));
        self.editing = false;
        self.error.clear();
        self.scroll = 0;
        // A text field's editor starts with its answer.
        if let Some(f) = next.filter(|f| textual(Some(f))) {
            let typed = self.custom.get(f.key()).cloned().unwrap_or_default();
            let text = if typed.is_empty() {
                display(&f, self.answers.get(f.key()), "(none)")
            } else {
                typed
            };
            self.editor.set(text);
        }
    }

    fn step(&mut self, by: isize) {
        let n = self.tabs() as isize;
        self.select_tab(((self.tab as isize + by + n) % n) as usize);
    }

    fn update_custom(&mut self, value: &str) {
        let Some(f) = self.answer_field() else {
            return;
        };
        let key = f.key().to_string();
        let previous = self.custom.get(&key).cloned();
        if previous.as_deref() == Some(value) {
            return;
        }
        self.custom.insert(key.clone(), value.into());
        let v = if matches!(f, Field::Multiselect { .. }) {
            Some(list(set_custom(
                self.answers.get(&key),
                previous.as_deref(),
                value,
            )))
        } else {
            (!value.is_empty()).then(|| Value::String(value.into()))
        };
        self.set_answer(&key, v);
    }

    fn start_editing(&mut self) {
        self.editing = true;
        self.editor.set(self.input());
    }

    fn pick(&mut self, value: Value) -> Outcome {
        let Some(f) = self.answer_field() else {
            return Outcome::Nothing;
        };
        if let Some(e) = validate(&f, Some(&value)) {
            self.error = e;
            return Outcome::Nothing;
        }
        self.set_answer(f.key(), Some(value.clone()));
        if self.single() {
            return Outcome::Reply([(f.key().to_string(), value)].into());
        }
        self.select_tab(self.tab + 1);
        Outcome::Nothing
    }

    fn select_option(&mut self) -> Outcome {
        let Some(f) = self.answer_field() else {
            return Outcome::Nothing;
        };
        if self.other() {
            let v = self.input();
            if self.multi() && !v.is_empty() && self.custom_picked() {
                let t = toggle(self.answers.get(f.key()), &v);
                self.set_answer(f.key(), Some(list(t)));
            } else {
                self.start_editing();
            }
            return Outcome::Nothing;
        }
        let Some(row) = self.rows().get(self.selected).cloned() else {
            return Outcome::Nothing;
        };
        if self.multi() {
            let t = toggle(self.answers.get(f.key()), &plain(&row.value));
            self.set_answer(f.key(), Some(list(t)));
            return Outcome::Nothing;
        }
        self.pick(row.value)
    }

    /// Commit the editor's text as the field's answer.
    fn commit(&mut self, text: &str) -> bool {
        let Some(f) = self.answer_field() else {
            return false;
        };
        let key = f.key().to_string();
        let textual = self.textual();
        let multi = self.multi();
        if text.is_empty() {
            let v = (!textual && multi).then(|| {
                list(set_custom(
                    self.answers.get(&key),
                    self.custom.get(&key).map(String::as_str),
                    "",
                ))
            });
            if textual || !multi {
                if let Some(e) = validate(&f, v.as_ref()) {
                    self.error = e;
                    return false;
                }
            }
            self.set_answer(&key, v);
            self.custom.insert(key, String::new());
            self.editing = false;
            return true;
        }
        let value = match &f {
            Field::Number { .. } | Field::Integer { .. } if textual => {
                let n = text.parse::<f64>().unwrap_or(f64::NAN);
                serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number)
            }
            Field::Multiselect { .. } => list(set_custom(
                self.answers.get(&key),
                self.custom.get(&key).map(String::as_str),
                text,
            )),
            _ => Value::String(text.into()),
        };
        if !multi {
            let checked = if value.is_null() {
                Some("Expected a number".to_string())
            } else {
                validate(&f, Some(&value))
            };
            if let Some(e) = checked {
                self.error = e;
                return false;
            }
        }
        self.set_answer(&key, Some(value));
        let configured = matches!(&f, Field::String { options: Some(o), .. } if o.iter().any(|c| c.value == text));
        self.custom.insert(
            key,
            if configured {
                String::new()
            } else {
                text.into()
            },
        );
        self.editing = false;
        true
    }

    fn submit_input(&mut self, text: &str, dir: isize) {
        if !self.commit(text) {
            if dir < 0 {
                self.step(dir);
            }
            return;
        }
        if !self.single() {
            self.step(dir);
        }
    }

    fn acknowledge(&mut self) -> Outcome {
        let Some((key, url)) = self.external() else {
            return Outcome::Nothing;
        };
        if self.answers.get(&key) == Some(&Value::Bool(true)) {
            self.select_tab(self.tab + 1);
            return Outcome::Nothing;
        }
        if !self.ready.contains(&key) {
            self.error.clear();
            self.ready.insert(key);
            return Outcome::Open(url);
        }
        self.set_answer(&key, Some(Value::Bool(true)));
        self.select_tab(self.tab + 1);
        Outcome::Nothing
    }

    fn copy(&mut self) -> Outcome {
        match self.external() {
            Some((key, url)) => {
                self.ready.insert(key);
                Outcome::Copy(url)
            }
            None => Outcome::Nothing,
        }
    }

    fn submit(&mut self) -> Outcome {
        let active: Vec<Field> = self.active().into_iter().cloned().collect();
        if let Some(f) = active
            .iter()
            .find(|f| f.base().is_none() && self.answers.get(f.key()) != Some(&Value::Bool(true)))
        {
            self.error = format!("External action must be acknowledged: {}", label(f));
            return Outcome::Nothing;
        }
        if let Some(e) = active
            .iter()
            .find_map(|f| validate(f, self.answers.get(f.key())))
        {
            self.error = e;
            return Outcome::Nothing;
        }
        Outcome::Reply(
            active
                .iter()
                .filter_map(|f| {
                    self.answers
                        .get(f.key())
                        .map(|v| (f.key().to_string(), v.clone()))
                })
                .collect(),
        )
    }

    /// Text pasted (from the terminal, or with the paste key).
    pub fn paste(&mut self, s: &str) -> Outcome {
        let text = super::app::strip_escapes(s)
            .replace("\r\n", "\n")
            .replace('\r', "\n");
        if self.editing || self.textual() {
            self.editor.insert_str(&text);
            self.edited();
            return Outcome::Nothing;
        }
        if self.answer_field().is_none() || !self.has_custom() || self.confirm() {
            return Outcome::Nothing;
        }
        self.selected = self.rows().len();
        let next = self.input() + &text;
        self.update_custom(&next);
        self.start_editing();
        Outcome::Nothing
    }

    /// The editor changed: a custom answer follows it as it is typed.
    fn edited(&mut self) {
        if self.editing && !self.textual() {
            let t = self.editor.text().to_string();
            self.update_custom(&t);
        }
    }

    pub fn key(&mut self, k: &KeyEvent, km: &Keymap) -> Outcome {
        let st = keymap::stroke(k);
        let bound = |id: &'static str| {
            st.as_ref()
                .is_some_and(|s| !km.lookup(&[id], std::slice::from_ref(s)).exact.is_empty())
        };
        let mods = k.modifiers;
        let plain_key =
            !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER);
        let ch = match k.code {
            KeyCode::Char(c) if plain_key => Some(c),
            _ => None,
        };
        let submit = bound("dialog.prompt.submit");
        let confirm = self.confirm();
        if (self.editing || self.textual()) && !confirm {
            if submit {
                let text = self.editor.text().trim().to_string();
                let Some(f) = self.answer_field() else {
                    return Outcome::Nothing;
                };
                if self.textual() {
                    self.submit_input(&text, 1);
                    return Outcome::Nothing;
                }
                let was_multi = self.multi();
                if !self.commit(&text) || was_multi || text.is_empty() {
                    return Outcome::Nothing;
                }
                if self.single() {
                    return Outcome::Reply([(f.key().to_string(), Value::String(text))].into());
                }
                self.select_tab(self.tab + 1);
                return Outcome::Nothing;
            }
            if bound("prompt.clear") {
                if self.editor.is_empty() {
                    if self.textual() {
                        return Outcome::Cancel;
                    }
                    self.editing = false;
                } else {
                    self.editor.clear();
                    self.edited();
                }
                return Outcome::Nothing;
            }
            match k.code {
                KeyCode::Esc => {
                    if self.textual() {
                        return Outcome::Cancel;
                    }
                    self.editing = false;
                }
                KeyCode::Tab | KeyCode::BackTab => {
                    let text = self.editor.text().trim().to_string();
                    let dir = if k.code == KeyCode::BackTab { -1 } else { 1 };
                    self.submit_input(&text, dir);
                }
                KeyCode::Up => {
                    if !self.editor.up() && !self.textual() && self.selected > 0 {
                        self.editing = false;
                        self.selected -= 1;
                    }
                }
                KeyCode::Down => {
                    self.editor.down();
                }
                KeyCode::Left => self.editor.left(),
                KeyCode::Right => self.editor.right(),
                KeyCode::Home => self.editor.home(),
                KeyCode::End => self.editor.end(),
                KeyCode::Backspace => {
                    self.editor.backspace();
                    self.edited();
                }
                KeyCode::Delete => {
                    self.editor.delete();
                    self.edited();
                }
                KeyCode::Enter if bound("input.newline") => {
                    self.editor.insert('\n');
                    self.edited();
                }
                _ => {
                    if let Some(c) = ch {
                        self.editor.insert(c);
                        self.edited();
                    }
                }
            }
            return Outcome::Nothing;
        }
        // Typing on the custom row starts its answer.
        if !self.textual() && self.other() && !confirm {
            if let Some(c) = ch.filter(|c| *c != ' ' && !c.is_control()) {
                let next = format!("{}{c}", self.input());
                self.update_custom(&next);
                self.start_editing();
                return Outcome::Nothing;
            }
        }
        if bound("app.exit") {
            return Outcome::Cancel;
        }
        match (k.code, ch) {
            (KeyCode::Left, _) | (_, Some('h')) | (KeyCode::BackTab, _) => {
                self.step(-1);
                return Outcome::Nothing;
            }
            (KeyCode::Right, _) | (_, Some('l')) | (KeyCode::Tab, _) => {
                self.step(1);
                return Outcome::Nothing;
            }
            (KeyCode::Esc, _) => return Outcome::Cancel,
            _ => {}
        }
        if self.external().is_some() {
            if submit {
                return self.acknowledge();
            }
            if ch == Some('c') {
                return self.copy();
            }
            return Outcome::Nothing;
        }
        if confirm {
            if submit {
                return self.submit();
            }
            match (k.code, ch) {
                (KeyCode::Up, _) | (_, Some('k')) => self.scroll = self.scroll.saturating_sub(1),
                (KeyCode::Down, _) | (_, Some('j')) => self.scroll += 1,
                _ => {}
            }
            return Outcome::Nothing;
        }
        let total = self.rows().len() + usize::from(self.has_custom());
        if let Some(d) = ch.and_then(|c| c.to_digit(10)).map(|d| d as usize) {
            if (1..=total.min(9)).contains(&d) {
                self.selected = d - 1;
                return self.select_option();
            }
        }
        if submit || (ch == Some(' ') && self.multi()) {
            return self.select_option();
        }
        if total > 0 {
            match (k.code, ch) {
                (KeyCode::Up, _) | (_, Some('k')) => {
                    self.selected = (self.selected + total - 1) % total
                }
                (KeyCode::Down, _) | (_, Some('j')) => self.selected = (self.selected + 1) % total,
                _ => {}
            }
        }
        Outcome::Nothing
    }

    fn action_label(&self) -> &'static str {
        if self.confirm() {
            return "submit";
        }
        if let Some((key, _)) = self.external() {
            if self.answers.get(&key) == Some(&Value::Bool(true)) {
                return "continue";
            }
            return if self.ready.contains(&key) {
                "I finished"
            } else {
                "open link"
            };
        }
        if self.multi() {
            if self.other() && self.editing {
                return "done";
            }
            if self.other() && self.input().is_empty() {
                return "edit";
            }
            return "toggle";
        }
        if self.single() {
            "submit"
        } else {
            "confirm"
        }
    }

    /// How many rows the dialog's text takes at `width` columns.
    pub fn height(&self, width: usize, term: (u16, u16), km: &Keymap) -> usize {
        wrapped(&self.lines(width, term, km), width)
    }

    /// The form's text, `width` columns wide on a terminal of `term`
    /// columns and rows. Text from the model or a server is shown escaped.
    pub fn lines(&self, width: usize, term: (u16, u16), km: &Keymap) -> Vec<Line<'static>> {
        let p = theme::current();
        let fg = |c| Style::default().fg(c);
        let safe = |s: &str| super::view::display_safe(s);
        let mut out: Vec<Line<'static>> = Vec::new();
        if let Some(m) = &self.message {
            out.push(Line::from(safe(m)));
            out.push(Line::default());
        }
        let active = self.active();
        let single = self.single();
        let confirm = self.confirm();
        let tabbed = active
            .iter()
            .map(|f| truncate(&label(f), 24).chars().count() + 3)
            .sum::<usize>()
            + "Submit".len()
            + 3
            <= (term.0 as usize).saturating_sub(8);
        if !single && !tabbed {
            let head = if confirm {
                "Review".to_string()
            } else {
                format!(
                    "Field {} of {}",
                    self.tab.min(active.len().saturating_sub(1)) + 1,
                    active.len()
                )
            };
            let done = active.iter().filter(|f| self.completed(f)).count();
            out.push(Line::from(Span::styled(
                format!("{head}   · {done}/{} completed", active.len()),
                fg(p.dialog_muted),
            )));
            out.push(Line::default());
        } else if !single {
            let mut spans = Vec::new();
            for (i, f) in active.iter().enumerate() {
                let style = if i == self.tab {
                    fg(p.dialog_text).add_modifier(Modifier::BOLD)
                } else {
                    fg(p.dialog_muted)
                };
                spans.push(Span::styled(safe(&truncate(&label(f), 24)), style));
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(
                "Submit",
                if confirm {
                    fg(p.dialog_text).add_modifier(Modifier::BOLD)
                } else {
                    fg(p.dialog_muted)
                },
            ));
            out.push(Line::from(spans));
            out.push(Line::default());
        }
        let focused = fg(p.accent);
        match self.field() {
            _ if confirm => {
                let mut review: Vec<Line<'static>> = Vec::new();
                for f in &active {
                    let name = format!("{}:", safe(&truncate(&label(f), 40)));
                    let v = self.answers.get(f.key());
                    let (text, style) = if f.base().is_none() {
                        if v == Some(&Value::Bool(true)) {
                            ("Acknowledged".to_string(), fg(p.success))
                        } else {
                            ("(acknowledgement required)".to_string(), fg(p.error))
                        }
                    } else if let Some(e) = validate(f, v) {
                        (e, fg(p.error))
                    } else if v.is_some() {
                        (safe(&display(f, v, "(none)")), fg(p.dialog_text))
                    } else if f.base().is_some_and(|b| b.required) {
                        ("(required)".into(), fg(p.error))
                    } else {
                        ("(not answered)".into(), fg(p.dialog_muted))
                    };
                    review.push(Line::from(vec![
                        Span::styled(name, fg(p.dialog_muted)),
                        Span::raw(" "),
                        Span::styled(text, style),
                    ]));
                }
                let max = (term.1 as usize).saturating_sub(14).max(3);
                let tall = wrapped(&review, width) > max;
                let top = if tall {
                    self.scroll.min(review.len().saturating_sub(1))
                } else {
                    0
                };
                let mut used = 0;
                for l in review.into_iter().skip(top) {
                    used += wrapped(std::slice::from_ref(&l), width);
                    if used > max {
                        break;
                    }
                    out.push(l);
                }
                out.push(Line::default());
                return self.footer(out, tall, km);
            }
            Some(Field::External {
                key,
                url,
                title,
                description,
            }) => {
                if let Some(t) = title {
                    out.push(Line::from(safe(t)));
                }
                if let Some(d) = description {
                    out.push(Line::from(Span::styled(safe(d), fg(p.dialog_muted))));
                }
                out.push(Line::from(Span::styled(safe(url), fg(p.primary))));
                let status = if self.answers.get(key) == Some(&Value::Bool(true)) {
                    Span::styled("✓ Acknowledged", fg(p.success))
                } else if self.ready.contains(key) {
                    Span::styled(
                        "Complete the external action, then press enter to confirm.",
                        fg(p.dialog_muted),
                    )
                } else {
                    Span::styled(
                        "Open or copy the URL, complete the external action, then confirm.",
                        fg(p.dialog_muted),
                    )
                };
                out.push(Line::from(status));
            }
            Some(f) => {
                let f = f.clone();
                let b = f.base().unwrap();
                out.push(Line::from(Span::styled(
                    safe(b.description.as_deref().unwrap_or(&label(&f))),
                    fg(p.dialog_text).add_modifier(Modifier::BOLD),
                )));
                out.push(Line::default());
                if textual(Some(&f)) {
                    let mut spans = vec![Span::styled("> ", focused)];
                    spans.extend(self.editor_spans(&placeholder(&f), fg(p.dialog_text)));
                    out.push(Line::from(spans));
                } else {
                    self.choice_lines(&mut out);
                }
            }
            None => {}
        }
        out.push(Line::default());
        self.footer(out, false, km)
    }

    fn editor_spans(&self, placeholder: &str, style: Style) -> Vec<Span<'static>> {
        let p = theme::current();
        let text = self.editor.text();
        let cur = Style::default().add_modifier(Modifier::REVERSED);
        if text.is_empty() {
            let mut c = placeholder.chars();
            let first = c.next().map(String::from).unwrap_or_else(|| " ".into());
            return vec![
                Span::styled(first, cur.fg(p.dialog_muted)),
                Span::styled(c.collect::<String>(), Style::default().fg(p.dialog_muted)),
            ];
        }
        let at = self.editor.cursor();
        let (a, b) = text.split_at(at);
        let mut it = b.chars();
        let under = it.next();
        let shown = |s: &str| super::view::escape_controls(s);
        vec![
            Span::styled(shown(a), style),
            Span::styled(
                match under {
                    Some('\n') | None => " ".to_string(),
                    Some(c) => shown(&c.to_string()),
                },
                cur,
            ),
            Span::styled(
                format!(
                    "{}{}",
                    if under == Some('\n') { "\n" } else { "" },
                    shown(it.as_str())
                ),
                style,
            ),
        ]
    }

    fn choice_lines(&self, out: &mut Vec<Line<'static>>) {
        let p = theme::current();
        let fg = |c| Style::default().fg(c);
        let safe = |s: &str| super::view::display_safe(s);
        let multi = self.multi();
        let key = self
            .answer_field()
            .map(|f| f.key().to_string())
            .unwrap_or_default();
        let answer = self.answers.get(&key);
        let rows = self.rows();
        for (i, r) in rows.iter().enumerate() {
            let active = i == self.selected;
            let picked = match answer {
                Some(Value::Array(a)) => a.iter().any(|v| plain(v) == plain(&r.value)),
                Some(v) => *v == r.value,
                None => false,
            };
            let num = if active {
                fg(p.accent)
            } else {
                fg(p.dialog_muted)
            };
            let text = if active {
                fg(p.accent)
            } else {
                fg(p.dialog_text)
            };
            let mut spans = vec![Span::styled(format!("{}. ", i + 1), num)];
            if multi {
                spans.push(Span::styled(
                    format!("[{}] ", if picked { "✓" } else { " " }),
                    if active {
                        fg(p.accent)
                    } else if picked {
                        fg(p.success)
                    } else {
                        fg(p.dialog_muted)
                    },
                ));
            }
            spans.push(Span::styled(safe(&r.label), text));
            if !multi && picked {
                spans.push(Span::styled(" ✓", fg(p.success)));
            }
            out.push(Line::from(spans));
            if let Some(d) = &r.description {
                let pad = if multi { 7 } else { 3 };
                out.push(Line::from(Span::styled(
                    format!("{}{}", " ".repeat(pad), safe(d)),
                    fg(p.dialog_muted),
                )));
            }
        }
        if self.has_custom() {
            let other = self.other();
            let checked = self.custom_picked() || (multi && other && self.editing);
            let style = if other {
                fg(p.accent)
            } else {
                fg(p.dialog_text)
            };
            let mut spans = vec![Span::styled(
                format!("{}. ", rows.len() + 1),
                if other {
                    fg(p.accent)
                } else {
                    fg(p.dialog_muted)
                },
            )];
            if multi {
                spans.push(Span::styled(
                    format!("[{}] ", if checked { "✓" } else { " " }),
                    if other {
                        fg(p.accent)
                    } else if checked {
                        fg(p.success)
                    } else {
                        fg(p.dialog_muted)
                    },
                ));
            }
            if self.editing {
                spans.extend(self.editor_spans("Type your own answer", style));
            } else {
                let input = self.input();
                let shown = if input.is_empty() {
                    "Type your own answer".to_string()
                } else {
                    safe(&input)
                };
                spans.push(Span::styled(shown, style));
                if !multi && self.custom_picked() {
                    spans.push(Span::styled(" ✓", fg(p.success)));
                }
            }
            out.push(Line::from(spans));
        }
    }

    fn footer(&self, mut out: Vec<Line<'static>>, scroll: bool, km: &Keymap) -> Vec<Line<'static>> {
        let p = theme::current();
        let key = |k: &str| Span::styled(k.to_string(), Style::default().fg(p.dialog_text));
        let what = |w: &str| Span::styled(format!(" {w}   "), Style::default().fg(p.dialog_muted));
        let mut spans = Vec::new();
        if !self.single() {
            spans.extend([key("⇆"), what("tab")]);
        }
        if !self.confirm() && !self.textual() && self.external().is_none() && !self.editing {
            spans.extend([key("↑↓"), what("select")]);
        }
        if scroll {
            spans.extend([key("↑↓"), what("scroll")]);
        }
        let enter = match km.hint("dialog.prompt.submit") {
            h if h.is_empty() => "enter".to_string(),
            h => h,
        };
        spans.extend([key(&enter), what(self.action_label())]);
        if self.external().is_some() {
            spans.extend([key("c"), what("copy")]);
        }
        spans.extend([
            key("esc"),
            what(if self.editing && !self.textual() {
                "close"
            } else {
                "dismiss"
            }),
        ]);
        out.push(Line::from(spans));
        if !self.error.is_empty() {
            out.push(Line::from(Span::styled(
                super::view::display_safe(&self.error),
                Style::default().fg(p.error),
            )));
        }
        explode(out)
    }
}

/// `lines` with the line breaks inside their spans made into lines.
fn explode(lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    let mut out = Vec::with_capacity(lines.len());
    for l in lines {
        if !l.spans.iter().any(|s| s.content.contains('\n')) {
            out.push(l);
            continue;
        }
        let mut cur: Vec<Span<'static>> = Vec::new();
        for s in l.spans {
            let mut parts = s.content.split('\n').peekable();
            while let Some(part) = parts.next() {
                if !part.is_empty() {
                    cur.push(Span::styled(part.to_string(), s.style));
                }
                if parts.peek().is_some() {
                    out.push(Line::from(std::mem::take(&mut cur)));
                }
            }
        }
        out.push(Line::from(cur));
    }
    out
}

fn placeholder(f: &Field) -> String {
    match f {
        Field::String { format, .. } => match format.as_deref() {
            Some("email") => "name@example.com",
            Some("uri") => "https://example.com",
            Some("date") => "YYYY-MM-DD",
            Some("date-time") => "YYYY-MM-DDTHH:MM:SSZ",
            _ => "Type your answer",
        }
        .into(),
        Field::Number {
            minimum, maximum, ..
        }
        | Field::Integer {
            minimum, maximum, ..
        } => match (minimum, maximum) {
            (Some(a), Some(b)) => format!("{}-{}", fmt_num(*a), fmt_num(*b)),
            (Some(a), None) => format!("at least {}", fmt_num(*a)),
            (None, Some(b)) => format!("at most {}", fmt_num(*b)),
            _ => "Type your answer".into(),
        },
        _ => "Type your answer".into(),
    }
}

/// Rows `lines` take wrapped at `width` (by words, as the dialog wraps).
pub fn wrapped(lines: &[Line], width: usize) -> usize {
    use unicode_width::UnicodeWidthStr;
    let width = width.max(1);
    lines
        .iter()
        .map(|l| {
            let s: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
            s.split('\n')
                .map(|part| {
                    if part.width() <= width {
                        1
                    } else {
                        textwrap::wrap(part, width).len().max(1)
                    }
                })
                .sum::<usize>()
        })
        .sum()
}
