//! OpenCode's V2 theme documents (`@opencode/theme/tui`): a `base` of
//! semantic tokens and a `light` and/or `dark` mode, each with hue scales.
//! A document is validated, a mode selected and merged over the base, its
//! state fallbacks expanded, and every token resolved to a color.
//!
//! Resolved colors live in one arena and tokens hold their index, so
//! "the same color" (OpenCode's object identity, which `source`,
//! `increase` and `decrease` rely on) is the same index, and equal values
//! from different places stay distinct.

use super::color::{is_hex, Rgba};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::Arc;

pub const STEPS: [u32; 9] = [100, 200, 300, 400, 500, 600, 700, 800, 900];
pub const STATES: [&str; 5] = ["disabled", "pressed", "focused", "selected", "hovered"];
pub const VARIANTS: [&str; 3] = ["primary", "secondary", "destructive"];
pub const FEEDBACK: [&str; 4] = ["error", "warning", "success", "info"];
pub const SYNTAX: [&str; 9] = [
    "comment",
    "keyword",
    "function",
    "variable",
    "string",
    "number",
    "type",
    "operator",
    "punctuation",
];
pub const MARKDOWN: [&str; 14] = [
    "text",
    "heading",
    "link",
    "linkText",
    "code",
    "blockQuote",
    "emphasis",
    "strong",
    "horizontalRule",
    "listItem",
    "listEnumeration",
    "image",
    "imageText",
    "codeBlock",
];
const SEMANTIC: [&str; 3] = ["accent", "interactive", "neutral"];
/// The order hue scales resolve in: OpenCode's files list the chromatic
/// hues first and the semantic aliases last.
const HUE_ORDER: [&str; 8] = [
    "gray", "red", "orange", "yellow", "green", "cyan", "blue", "purple",
];

/// A light or dark mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    Light,
    Dark,
}

impl Mode {
    pub fn key(self) -> &'static str {
        match self {
            Mode::Light => "light",
            Mode::Dark => "dark",
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "light" => Some(Mode::Light),
            "dark" => Some(Mode::Dark),
            _ => None,
        }
    }

    pub fn other(self) -> Mode {
        match self {
            Mode::Light => Mode::Dark,
            Mode::Dark => Mode::Light,
        }
    }
}

/// A resolved color: an index into its theme's arena.
pub type C = u32;

/// A color with one value per interaction state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stateful {
    pub base: C,
    pub disabled: C,
    pub pressed: C,
    pub focused: C,
    pub selected: C,
    pub hovered: C,
}

/// Which interaction states apply.
#[cfg(test)]
#[derive(Debug, Clone, Copy, Default)]
pub struct States {
    pub disabled: bool,
    pub pressed: bool,
    pub focused: bool,
    pub selected: bool,
    pub hovered: bool,
}

#[cfg(test)]
impl Stateful {
    /// The color of the first state that applies, in OpenCode's order
    /// (disabled, pressed, focused, selected, hovered), else the base.
    pub fn state(&self, s: States) -> C {
        [
            (s.disabled, self.disabled),
            (s.pressed, self.pressed),
            (s.focused, self.focused),
            (s.selected, self.selected),
            (s.hovered, self.hovered),
        ]
        .into_iter()
        .find_map(|(on, c)| on.then_some(c))
        .unwrap_or(self.base)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Feedback {
    pub base: C,
    pub muted: C,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Text {
    pub base: C,
    pub muted: C,
    pub faint: C,
    /// primary, secondary, destructive.
    pub action: [Stateful; 3],
    pub formfield: Stateful,
    /// error, warning, success, info.
    pub feedback: [Feedback; 4],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Raised {
    pub base: C,
    pub high: C,
    pub max: C,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Background {
    pub base: C,
    pub raised: Raised,
    pub action: [Stateful; 3],
    pub formfield: Stateful,
    pub feedback: [C; 4],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Diff {
    pub added: C,
    pub removed: C,
    pub context: C,
    pub hunk_header: C,
    pub added_bg: C,
    pub removed_bg: C,
    pub context_bg: C,
    pub highlight_added: C,
    pub highlight_removed: C,
    pub line_number: C,
    pub line_number_added_bg: C,
    pub line_number_removed_bg: C,
}

/// One surface's tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tokens {
    pub text: Text,
    pub background: Background,
    pub border: C,
    pub scrollbar: C,
    pub diff: Diff,
    /// In [`SYNTAX`] order.
    pub syntax: [C; 9],
    /// In [`MARKDOWN`] order.
    pub markdown: [C; 14],
}

#[cfg(test)]
impl Tokens {
    pub fn syntax(&self, t: &str) -> C {
        self.syntax[SYNTAX.iter().position(|x| *x == t).unwrap_or(0)]
    }
}

#[derive(Debug)]
struct Shared {
    colors: Vec<Rgba>,
    hue_names: Vec<String>,
    hues: Vec<[C; 9]>,
    categorical: Vec<usize>,
    /// Color → (hue, step position).
    #[cfg(test)]
    index: HashMap<C, (usize, usize)>,
    /// base, dialog, dialog.footer.
    views: Vec<Tokens>,
    footer: usize,
}

/// A resolved theme seen from one surface. Cloning is cheap.
#[derive(Debug, Clone)]
pub struct Resolved {
    shared: Arc<Shared>,
    view: usize,
}

impl std::ops::Deref for Resolved {
    type Target = Tokens;
    fn deref(&self) -> &Tokens {
        &self.shared.views[self.view]
    }
}

impl Resolved {
    /// The color's value (`u32::MAX` or another index this theme did not
    /// make is black).
    pub fn rgba(&self, c: C) -> Rgba {
        self.shared
            .colors
            .get(c as usize)
            .copied()
            .unwrap_or(Rgba::rgb(0, 0, 0))
    }

    /// A hue scale by name (`accent`, `neutral`, `blue`, …).
    pub fn hue(&self, name: &str) -> Option<[C; 9]> {
        let i = self.shared.hue_names.iter().position(|n| n == name)?;
        Some(self.shared.hues[i])
    }

    /// One step of a hue (`step` is 100…900).
    pub fn step(&self, name: &str, step: u32) -> C {
        let pos = STEPS.iter().position(|s| *s == step).unwrap_or(1);
        self.hue(name).map(|h| h[pos]).unwrap_or(u32::MAX)
    }

    pub fn categorical(&self) -> Vec<[C; 9]> {
        self.shared
            .categorical
            .iter()
            .map(|&i| self.shared.hues[i])
            .collect()
    }

    /// The hue and step a color came from, when it is one of the theme's
    /// hue colors.
    #[cfg(test)]
    pub fn source(&self, c: C) -> Option<(&str, u32)> {
        let (h, p) = *self.shared.index.get(&c)?;
        Some((self.shared.hue_names[h].as_str(), STEPS[p]))
    }

    #[cfg(test)]
    fn shift(&self, c: C, amount: i64) -> C {
        let Some(&(h, p)) = self.shared.index.get(&c) else {
            return c;
        };
        let pos = (p as i64 + amount).clamp(0, STEPS.len() as i64 - 1) as usize;
        self.shared.hues[h][pos]
    }

    /// The next step(s) of the color's hue (darker in light themes);
    /// colors that are not hue steps come back unchanged.
    #[cfg(test)]
    pub fn increase(&self, c: C, amount: i64) -> C {
        self.shift(c, amount)
    }

    #[cfg(test)]
    pub fn decrease(&self, c: C, amount: i64) -> C {
        self.shift(c, -amount)
    }

    /// The same theme on a raised surface: `dialog` or `dialog.footer`.
    /// Surfaces are absolute: a surface's surfaces are the base theme's.
    pub fn surface(&self, name: &str) -> Resolved {
        let view = match name {
            "dialog" => 1,
            "dialog.footer" => self.shared.footer,
            _ => 0,
        };
        Resolved {
            shared: self.shared.clone(),
            view,
        }
    }

    /// Whether two views are the same surface of the same resolution.
    #[cfg(test)]
    pub fn same(&self, other: &Resolved) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared) && self.view == other.view
    }
}

// ----- errors ---------------------------------------------------------------

/// A schema failure: the offending value, when there is one.
struct Decode(Option<String>);

/// OpenCode's message for a theme that does not decode.
pub fn invalid(name: &str, got: Option<&str>) -> String {
    format!(
        "Invalid theme: {name} {} is an invalid value",
        got.unwrap_or("value")
    )
}

fn got(v: &Value) -> Decode {
    Decode(Some(v.to_string()))
}

type D = Result<(), Decode>;

// ----- schema ---------------------------------------------------------------

fn obj(v: &Value) -> Result<&Map<String, Value>, Decode> {
    v.as_object().ok_or_else(|| got(v))
}

fn color_value(v: &Value) -> D {
    match v.as_str() {
        Some(s) if is_hex(s) || s == "transparent" || (s.starts_with('$') && s.len() > 1) => Ok(()),
        _ => Err(got(v)),
    }
}

fn is_hue_step_ref(s: &str) -> bool {
    s.strip_prefix("$hue.")
        .and_then(|r| r.rsplit_once('.'))
        .is_some_and(|(name, step)| !name.is_empty() && STEPS.iter().any(|x| x.to_string() == step))
}

fn hue_color_value(v: &Value) -> D {
    match v.as_str() {
        Some(s) if is_hex(s) || is_hue_step_ref(s) => Ok(()),
        _ => Err(got(v)),
    }
}

fn field<'a>(
    o: &'a Map<String, Value>,
    k: &str,
    required: bool,
) -> Result<Option<&'a Value>, Decode> {
    match o.get(k) {
        Some(v) => Ok(Some(v)),
        None if required => Err(Decode(None)),
        None => Ok(None),
    }
}

fn opt_color(o: &Map<String, Value>, k: &str, required: bool) -> D {
    match field(o, k, required)? {
        Some(v) => color_value(v),
        None => Ok(()),
    }
}

fn stateful(v: &Value, complete: bool) -> D {
    let o = obj(v)?;
    opt_color(o, "base", complete)?;
    for s in STATES {
        opt_color(o, &format!("${s}"), false)?;
    }
    Ok(())
}

fn group<'a>(
    o: &'a Map<String, Value>,
    k: &str,
    complete: bool,
) -> Result<Option<&'a Map<String, Value>>, Decode> {
    match field(o, k, complete)? {
        Some(v) => obj(v).map(Some),
        None => Ok(None),
    }
}

fn actions(o: &Map<String, Value>, complete: bool) -> D {
    if let Some(a) = group(o, "action", complete)? {
        for v in VARIANTS {
            if let Some(x) = field(a, v, complete)? {
                stateful(x, complete)?;
            }
        }
    }
    if let Some(x) = field(o, "formfield", complete)? {
        stateful(x, complete)?;
    }
    Ok(())
}

fn record(o: &Map<String, Value>, k: &str, keys: &[&str], complete: bool) -> D {
    if let Some(r) = group(o, k, complete)? {
        for key in keys {
            if let Some(v) = field(r, key, complete)? {
                hue_color_value(v)?;
            }
        }
    }
    Ok(())
}

/// The seven token groups: all of them, completely, when `complete`.
fn tokens_schema(o: &Map<String, Value>, complete: bool) -> D {
    if let Some(t) = group(o, "text", complete)? {
        opt_color(t, "base", complete)?;
        opt_color(t, "muted", complete)?;
        opt_color(t, "faint", false)?;
        actions(t, complete)?;
        if let Some(f) = group(t, "feedback", complete)? {
            for k in FEEDBACK {
                if let Some(x) = group(f, k, complete)? {
                    opt_color(x, "base", complete)?;
                    opt_color(x, "muted", false)?;
                }
            }
        }
    }
    if let Some(b) = group(o, "background", complete)? {
        opt_color(b, "base", complete)?;
        if let Some(r) = group(b, "raised", complete)? {
            for k in ["base", "high", "max"] {
                opt_color(r, k, complete)?;
            }
        }
        actions(b, complete)?;
        if let Some(f) = group(b, "feedback", complete)? {
            for k in FEEDBACK {
                if let Some(x) = group(f, k, complete)? {
                    opt_color(x, "base", complete)?;
                }
            }
        }
    }
    for k in ["border", "scrollbar"] {
        if let Some(x) = group(o, k, complete)? {
            opt_color(x, "base", complete)?;
        }
    }
    if let Some(d) = group(o, "diff", complete)? {
        if let Some(t) = group(d, "text", complete)? {
            for k in ["added", "removed", "context", "hunkHeader"] {
                opt_color(t, k, complete)?;
            }
        }
        if let Some(t) = group(d, "background", complete)? {
            for k in ["added", "removed", "context"] {
                opt_color(t, k, complete)?;
            }
        }
        if let Some(t) = group(d, "highlight", complete)? {
            for k in ["added", "removed"] {
                opt_color(t, k, complete)?;
            }
        }
        if let Some(t) = group(d, "lineNumber", complete)? {
            opt_color(t, "text", complete)?;
            if let Some(b) = group(t, "background", complete)? {
                for k in ["added", "removed"] {
                    opt_color(b, k, complete)?;
                }
            }
        }
    }
    record(o, "syntax", &SYNTAX, complete)?;
    record(o, "markdown", &MARKDOWN, complete)?;
    Ok(())
}

fn dialog_schema(o: &Map<String, Value>) -> D {
    if let Some(d) = group(o, "@dialog", false)? {
        tokens_schema(d, false)?;
        if let Some(f) = group(d, "@footer", false)? {
            tokens_schema(f, false)?;
        }
    }
    Ok(())
}

fn categorical_schema(o: &Map<String, Value>, required: bool) -> D {
    if let Some(v) = field(o, "categorical", required)? {
        let a = v.as_array().ok_or_else(|| got(v))?;
        if a.is_empty() {
            return Err(got(v));
        }
        for x in a {
            if !x.as_str().is_some_and(|s| !s.is_empty()) {
                return Err(got(x));
            }
        }
    }
    Ok(())
}

fn hue_schema(o: &Map<String, Value>) -> D {
    let h = obj(field(o, "hue", true)?.unwrap_or(&Value::Null))?;
    for v in h.values() {
        match v {
            Value::String(s) if s.starts_with("$hue.") && s.len() > 5 => {}
            Value::Object(scale) => {
                for step in STEPS {
                    let x = scale.get(&step.to_string()).ok_or(Decode(None))?;
                    if !x.as_str().is_some_and(is_hex) {
                        return Err(got(x));
                    }
                }
            }
            other => return Err(got(other)),
        }
    }
    if SEMANTIC.iter().any(|s| !h.contains_key(*s)) {
        return Err(Decode(None));
    }
    Ok(())
}

/// A complete theme: hues, categorical colors and every token.
#[cfg(test)]
fn definition_schema(v: &Value) -> D {
    let o = obj(v)?;
    hue_schema(o)?;
    categorical_schema(o, true)?;
    tokens_schema(o, true)?;
    dialog_schema(o)
}

fn mode_schema(v: &Value) -> D {
    let o = obj(v)?;
    hue_schema(o)?;
    categorical_schema(o, false)?;
    tokens_schema(o, false)?;
    dialog_schema(o)
}

fn document_schema(v: &Value) -> D {
    let o = obj(v)?;
    if let Some(s) = o.get("$schema") {
        if !s.is_string() {
            return Err(got(s));
        }
    }
    let base = obj(field(o, "base", true)?.unwrap_or(&Value::Null))?;
    categorical_schema(base, true)?;
    tokens_schema(base, true)?;
    dialog_schema(base)?;
    if !o.contains_key("light") && !o.contains_key("dark") {
        return Err(Decode(None));
    }
    for m in ["light", "dark"] {
        if let Some(x) = o.get(m) {
            mode_schema(x)?;
        }
    }
    Ok(())
}

// ----- selection and expansion ----------------------------------------------

/// The modes a document provides, light first.
pub fn modes(doc: &Value) -> Vec<Mode> {
    [Mode::Light, Mode::Dark]
        .into_iter()
        .filter(|m| doc.get(m.key()).is_some())
        .collect()
}

#[cfg(test)]
pub fn supports(doc: &Value, mode: Mode) -> bool {
    modes(doc).contains(&mode)
}

/// The mode's definition merged over the base, and the mode used: the
/// requested one when the document has it (light by default), else the
/// one it has.
pub fn select(doc: &Value, mode: Option<Mode>) -> Result<(Value, Mode), String> {
    let all = modes(doc);
    let want = mode.unwrap_or(Mode::Light);
    let chosen = if all.contains(&want) {
        want
    } else {
        *all.first()
            .ok_or_else(|| "Theme must provide at least one mode".to_string())?
    };
    let merged = merge(&[doc.get("base").unwrap_or(&Value::Null), &doc[chosen.key()]]);
    Ok((merged, chosen))
}

/// Deep-merge objects left to right (OpenCode's `mergeTheme`).
pub fn merge(values: &[&Value]) -> Value {
    let mut out = Map::new();
    for v in values {
        let Some(o) = v.as_object() else { continue };
        for (k, item) in o {
            let next = if item.is_object() {
                merge(&[out.get(k).unwrap_or(&Value::Null), item])
            } else {
                item.clone()
            };
            out.insert(k.clone(), next);
        }
    }
    Value::Object(out)
}

fn s(v: &str) -> Value {
    Value::String(v.to_string())
}

fn expand_states(def: &Value, path: &str) -> Value {
    let Some(o) = def.as_object() else {
        return def.clone();
    };
    if !o.contains_key("base") {
        return def.clone();
    }
    let mut out = o.clone();
    for st in STATES {
        let k = format!("${st}");
        if !out.contains_key(&k) {
            out.insert(k, s(&format!("${path}.base")));
        }
    }
    Value::Object(out)
}

fn expand_actions(def: Option<&Value>, path: &str) -> Option<Value> {
    let o = def?.as_object()?;
    Some(Value::Object(
        o.iter()
            .map(|(variant, v)| {
                (
                    variant.clone(),
                    expand_states(v, &format!("{path}.{variant}")),
                )
            })
            .collect(),
    ))
}

fn expand_text(def: &Value) -> Value {
    let Some(o) = def.as_object() else {
        return def.clone();
    };
    let mut out = o.clone();
    let (base, muted) = (o.contains_key("base"), o.contains_key("muted"));
    if !muted && base {
        out.insert("muted".into(), s("$text.base"));
    }
    if !o.contains_key("faint") && (muted || base) {
        out.insert("faint".into(), s("$text.muted"));
    }
    if let Some(a) = expand_actions(o.get("action"), "text.action") {
        out.insert("action".into(), a);
    }
    if let Some(f) = o.get("formfield") {
        out.insert("formfield".into(), expand_states(f, "text.formfield"));
    }
    if let Some(Value::Object(fb)) = o.get("feedback") {
        let mut next = Map::new();
        for (kind, v) in fb {
            let mut v = v.clone();
            if let Value::Object(x) = &mut v {
                if !x.contains_key("muted") && x.contains_key("base") {
                    x.insert("muted".into(), s(&format!("$text.feedback.{kind}.base")));
                }
            }
            next.insert(kind.clone(), v);
        }
        out.insert("feedback".into(), Value::Object(next));
    }
    Value::Object(out)
}

fn expand_background(def: &Value) -> Value {
    let Some(o) = def.as_object() else {
        return def.clone();
    };
    let mut out = o.clone();
    if let Some(a) = expand_actions(o.get("action"), "background.action") {
        out.insert("action".into(), a);
    }
    if let Some(f) = o.get("formfield") {
        out.insert("formfield".into(), expand_states(f, "background.formfield"));
    }
    Value::Object(out)
}

fn expand_tokens(def: &Value) -> Value {
    let mut out = def.as_object().cloned().unwrap_or_default();
    if let Some(t) = out.get("text").map(expand_text) {
        out.insert("text".into(), t);
    }
    if let Some(b) = out.get("background").map(expand_background) {
        out.insert("background".into(), b);
    }
    Value::Object(out)
}

/// Fill in the state fallbacks a theme leaves out (OpenCode's
/// `expandTheme`), in the definition and its dialog surfaces.
pub fn expand(def: &Value) -> Value {
    let mut out = expand_tokens(def);
    if let Some(d) = def.get("@dialog") {
        let mut dialog = expand_tokens(d);
        if let Some(f) = d.get("@footer") {
            dialog["@footer"] = expand_tokens(f);
        }
        out["@dialog"] = dialog;
    }
    out
}

const TOKEN_KEYS: [&str; 7] = [
    "text",
    "background",
    "border",
    "scrollbar",
    "diff",
    "syntax",
    "markdown",
];

fn tokens_of(def: &Value) -> Value {
    Value::Object(
        TOKEN_KEYS
            .iter()
            .filter_map(|k| def.get(*k).map(|v| (k.to_string(), v.clone())))
            .collect(),
    )
}

fn contextual_actions(base: Option<&Value>, surface: Option<&Value>) -> Value {
    let mut out = Map::new();
    for variant in VARIANTS {
        let b = base.and_then(|x| x.get(variant));
        let sf = surface.and_then(|x| x.get(variant));
        let mut states = Map::new();
        for key in std::iter::once("base".to_string()).chain(STATES.iter().map(|s| format!("${s}")))
        {
            let pick = (key != "base")
                .then(|| sf.and_then(|x| x.get(&key)))
                .flatten()
                .or_else(|| sf.and_then(|x| x.get("base")))
                .or_else(|| {
                    (key != "base")
                        .then(|| b.and_then(|x| x.get(&key)))
                        .flatten()
                })
                .or_else(|| b.and_then(|x| x.get("base")));
            if let Some(v) = pick {
                states.insert(key, v.clone());
            }
        }
        out.insert(variant.to_string(), Value::Object(states));
    }
    Value::Object(out)
}

/// A surface's tokens: its overrides merged over `base`, with action
/// states reset from the surface's own base color.
fn contextualize(base: &Value, over: &Value) -> Value {
    let mut out = merge(&[base, over]);
    for g in ["text", "background"] {
        let actions = contextual_actions(
            base.get(g).and_then(|x| x.get("action")),
            over.get(g).and_then(|x| x.get("action")),
        );
        if let Some(o) = out.get_mut(g).and_then(Value::as_object_mut) {
            o.insert("action".into(), actions);
        } else {
            out[g] = serde_json::json!({ "action": actions });
        }
    }
    out
}

// ----- resolution -----------------------------------------------------------

struct Hues {
    names: Vec<String>,
    scales: Vec<[C; 9]>,
}

impl Hues {
    fn get(&self, name: &str) -> Option<[C; 9]> {
        self.names
            .iter()
            .position(|n| n == name)
            .map(|i| self.scales[i])
    }
}

fn hue_order(h: &Map<String, Value>) -> Vec<String> {
    let mut names: Vec<String> = HUE_ORDER
        .iter()
        .filter(|n| h.contains_key(**n))
        .map(|n| n.to_string())
        .collect();
    for k in h.keys() {
        if !HUE_ORDER.contains(&k.as_str()) && !SEMANTIC.contains(&k.as_str()) {
            names.push(k.clone());
        }
    }
    names.extend(
        SEMANTIC
            .iter()
            .filter(|n| h.contains_key(**n))
            .map(|n| n.to_string()),
    );
    names
}

fn resolve_hues(def: &Value, colors: &mut Vec<Rgba>) -> Result<Hues, String> {
    let empty = Map::new();
    let h = def.get("hue").and_then(Value::as_object).unwrap_or(&empty);
    let mut done: HashMap<String, [C; 9]> = HashMap::new();

    fn one(
        name: &str,
        stack: &mut Vec<String>,
        h: &Map<String, Value>,
        done: &mut HashMap<String, [C; 9]>,
        colors: &mut Vec<Rgba>,
    ) -> Result<[C; 9], String> {
        if let Some(x) = done.get(name) {
            return Ok(*x);
        }
        if stack.iter().any(|s| s == name) {
            let mut chain = stack.clone();
            chain.push(name.into());
            return Err(format!("Circular hue reference: {}", chain.join(" -> ")));
        }
        let scale = match h.get(name) {
            Some(Value::String(v)) => {
                let target = v
                    .strip_prefix("$hue.")
                    .filter(|t| !t.is_empty())
                    .ok_or_else(|| format!("Hue alias \"{v}\" must reference a hue scale"))?;
                if !h.contains_key(target) {
                    return Err(format!("Hue alias \"{v}\" references a missing hue"));
                }
                stack.push(name.into());
                let t = one(target, stack, h, done, colors);
                stack.pop();
                let t = t?;
                // A copy: the alias's colors are its own.
                t.map(|c| {
                    colors.push(colors[c as usize]);
                    colors.len() as C - 1
                })
            }
            Some(Value::Object(scale)) => {
                let mut out = [0; 9];
                for (i, step) in STEPS.iter().enumerate() {
                    let c = scale
                        .get(&step.to_string())
                        .and_then(Value::as_str)
                        .and_then(Rgba::from_hex)
                        .ok_or_else(|| format!("Invalid hue color at \"hue.{name}.{step}\""))?;
                    colors.push(c);
                    out[i] = colors.len() as C - 1;
                }
                if let Some(k) = scale
                    .keys()
                    .find(|k| !STEPS.iter().any(|s| s.to_string() == **k))
                {
                    return Err(format!("Unknown hue step at \"hue.{name}.{k}\""));
                }
                out
            }
            _ => return Err(format!("Hue \"{name}\" was not found")),
        };
        done.insert(name.into(), scale);
        Ok(scale)
    }

    let names = hue_order(h);
    let mut scales = Vec::new();
    for n in &names {
        scales.push(one(n, &mut vec![], h, &mut done, colors)?);
    }
    Ok(Hues { names, scales })
}

struct Resolver<'a> {
    src: &'a Value,
    hues: &'a Hues,
    colors: &'a mut Vec<Rgba>,
    cache: HashMap<String, C>,
}

impl Resolver<'_> {
    fn push(&mut self, c: Rgba) -> C {
        self.colors.push(c);
        self.colors.len() as C - 1
    }

    fn color(&mut self, value: &str, path: &str, stack: &mut Vec<String>) -> Result<C, String> {
        if value == "transparent" {
            return Ok(self.push(Rgba::TRANSPARENT));
        }
        if is_hex(value) {
            let c = Rgba::from_hex(value).unwrap_or_default();
            return Ok(self.push(c));
        }
        let Some(target) = value.strip_prefix('$') else {
            return Err(format!("Invalid color \"{value}\" at \"{path}\""));
        };
        if let Some(c) = self.cache.get(target) {
            return Ok(*c);
        }
        if stack.iter().any(|s| s == target) {
            let mut chain = stack.clone();
            chain.push(target.into());
            return Err(format!("Circular theme reference: {}", chain.join(" -> ")));
        }
        let c = if let Some((name, step)) = target
            .strip_prefix("hue.")
            .and_then(|r| r.rsplit_once('.'))
            .filter(|(_, st)| STEPS.iter().any(|x| x.to_string() == *st))
        {
            let pos = STEPS
                .iter()
                .position(|x| x.to_string() == step)
                .unwrap_or(0);
            self.hues
                .get(name)
                .map(|h| h[pos])
                .ok_or_else(|| format!("Theme reference \"${target}\" was not found"))?
        } else if let Some(name) = target.strip_prefix("hue.") {
            if self.hues.get(name).is_none() {
                return Err(format!("Theme reference \"${target}\" was not found"));
            }
            return Err(format!(
                "Theme reference \"{value}\" at \"{path}\" is not a color"
            ));
        } else {
            let mut cur = self.src;
            for k in target.split('.') {
                cur = cur
                    .get(k)
                    .ok_or_else(|| format!("Theme reference \"${target}\" was not found"))?;
            }
            match cur {
                Value::String(next) => {
                    stack.push(target.into());
                    let r = self.color(&next.clone(), target, stack);
                    stack.pop();
                    r?
                }
                _ => {
                    return Err(format!(
                        "Theme reference \"{value}\" at \"{path}\" is not a color"
                    ))
                }
            }
        };
        self.cache.insert(target.into(), c);
        Ok(c)
    }

    fn at(&mut self, path: &str) -> Result<C, String> {
        let mut cur = self.src;
        for k in path.split('.') {
            cur = cur
                .get(k)
                .ok_or_else(|| format!("Invalid theme value at \"theme.{path}\""))?;
        }
        match cur {
            Value::String(v) => {
                let v = v.clone();
                self.color(&v, &format!("theme.{path}"), &mut vec![])
            }
            _ => Err(format!("Invalid theme value at \"theme.{path}\"")),
        }
    }

    fn stateful(&mut self, path: &str) -> Result<Stateful, String> {
        Ok(Stateful {
            base: self.at(&format!("{path}.base"))?,
            disabled: self.at(&format!("{path}.$disabled"))?,
            pressed: self.at(&format!("{path}.$pressed"))?,
            focused: self.at(&format!("{path}.$focused"))?,
            selected: self.at(&format!("{path}.$selected"))?,
            hovered: self.at(&format!("{path}.$hovered"))?,
        })
    }

    fn tokens(&mut self) -> Result<Tokens, String> {
        let mut t = Tokens::default();
        t.text.base = self.at("text.base")?;
        t.text.muted = self.at("text.muted")?;
        t.text.faint = self.at("text.faint")?;
        for (i, v) in VARIANTS.iter().enumerate() {
            t.text.action[i] = self.stateful(&format!("text.action.{v}"))?;
        }
        t.text.formfield = self.stateful("text.formfield")?;
        for (i, k) in FEEDBACK.iter().enumerate() {
            t.text.feedback[i] = Feedback {
                base: self.at(&format!("text.feedback.{k}.base"))?,
                muted: self.at(&format!("text.feedback.{k}.muted"))?,
            };
        }
        t.background.base = self.at("background.base")?;
        t.background.raised = Raised {
            base: self.at("background.raised.base")?,
            high: self.at("background.raised.high")?,
            max: self.at("background.raised.max")?,
        };
        for (i, v) in VARIANTS.iter().enumerate() {
            t.background.action[i] = self.stateful(&format!("background.action.{v}"))?;
        }
        t.background.formfield = self.stateful("background.formfield")?;
        for (i, k) in FEEDBACK.iter().enumerate() {
            t.background.feedback[i] = self.at(&format!("background.feedback.{k}.base"))?;
        }
        t.border = self.at("border.base")?;
        t.scrollbar = self.at("scrollbar.base")?;
        t.diff = Diff {
            added: self.at("diff.text.added")?,
            removed: self.at("diff.text.removed")?,
            context: self.at("diff.text.context")?,
            hunk_header: self.at("diff.text.hunkHeader")?,
            added_bg: self.at("diff.background.added")?,
            removed_bg: self.at("diff.background.removed")?,
            context_bg: self.at("diff.background.context")?,
            highlight_added: self.at("diff.highlight.added")?,
            highlight_removed: self.at("diff.highlight.removed")?,
            line_number: self.at("diff.lineNumber.text")?,
            line_number_added_bg: self.at("diff.lineNumber.background.added")?,
            line_number_removed_bg: self.at("diff.lineNumber.background.removed")?,
        };
        for (i, k) in SYNTAX.iter().enumerate() {
            t.syntax[i] = self.at(&format!("syntax.{k}"))?;
        }
        for (i, k) in MARKDOWN.iter().enumerate() {
            t.markdown[i] = self.at(&format!("markdown.{k}"))?;
        }
        Ok(t)
    }
}

fn resolve_expanded(def: &Value) -> Result<Resolved, String> {
    let mut colors = Vec::new();
    let hues = resolve_hues(def, &mut colors)?;
    let mut categorical = Vec::new();
    for name in def
        .get("categorical")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let name = name.as_str().unwrap_or_default();
        let i = hues
            .names
            .iter()
            .position(|n| n == name)
            .ok_or_else(|| format!("Categorical hue \"{name}\" was not found"))?;
        categorical.push(i);
    }
    #[cfg(test)]
    let mut index = HashMap::new();
    #[cfg(test)]
    for (h, scale) in hues.scales.iter().enumerate() {
        for (p, c) in scale.iter().enumerate() {
            index.insert(*c, (h, p));
        }
    }
    let base = tokens_of(def);
    let dialog = def.get("@dialog");
    let dialog_tokens = match dialog {
        Some(d) => contextualize(&base, &tokens_of(d)),
        None => base.clone(),
    };
    let footer = dialog.and_then(|d| d.get("@footer"));
    let mut views = Vec::new();
    for src in [Some(&base), Some(&dialog_tokens)] {
        let src = src.unwrap();
        let mut r = Resolver {
            src,
            hues: &hues,
            colors: &mut colors,
            cache: HashMap::new(),
        };
        views.push(r.tokens()?);
    }
    let footer = match footer {
        Some(f) => {
            let src = contextualize(&dialog_tokens, f);
            let mut r = Resolver {
                src: &src,
                hues: &hues,
                colors: &mut colors,
                cache: HashMap::new(),
            };
            views.push(r.tokens()?);
            2
        }
        None => 1,
    };
    Ok(Resolved {
        shared: Arc::new(Shared {
            colors,
            hue_names: hues.names,
            hues: hues.scales,
            categorical,
            #[cfg(test)]
            index,
            views,
            footer,
        }),
        view: 0,
    })
}

/// Resolve a complete theme definition (OpenCode's `resolveTheme`).
#[cfg(test)]
pub fn resolve_definition(def: &Value) -> Result<Resolved, String> {
    definition_schema(def).map_err(|Decode(g)| invalid("theme", g.as_deref()))?;
    resolve_expanded(&expand(def))
}

/// Resolve a document in `mode` (or the mode it has).
pub fn resolve(doc: &Value, mode: Option<Mode>) -> Result<Resolved, String> {
    let (def, _) = select(doc, mode)?;
    resolve_expanded(&expand(&def))
}

/// Validate a V2 document, resolving each of its modes once (OpenCode's
/// `parseThemeDocument`). Errors name the theme.
pub fn parse_document(doc: &Value, name: &str) -> Result<Value, String> {
    document_schema(doc).map_err(|Decode(g)| invalid(name, g.as_deref()))?;
    for m in modes(doc) {
        resolve(doc, Some(m)).map_err(|_| invalid(name, None))?;
    }
    Ok(doc.clone())
}
