//! V1 themes as V2 documents (OpenCode's `migrateV1`): each mode's colors
//! are resolved, hue scales are generated around the theme's semantic
//! colors and a gray built from its surfaces and text, and every token
//! that matches a generated hue color references it.

use super::color::{oklch_to_hex, Oklch, Rgba};
use super::doc::{Mode, STEPS};
use super::v1::{self, Theme};
use serde_json::{json, Map, Value};

const CHROMATIC: [&str; 7] = ["red", "orange", "yellow", "green", "cyan", "blue", "purple"];
const CATEGORICAL_TOKENS: [&str; 7] = [
    "secondary",
    "accent",
    "success",
    "warning",
    "primary",
    "error",
    "info",
];
const MINIMUM_CHROMA: f64 = 0.03;
const LIGHT_THRESHOLD: f64 = 0.6;
const LIGHT_REFS: [&str; 7] = [
    "#fca5a5", "#fdba74", "#fde047", "#86efac", "#67e8f9", "#93c5fd", "#d8b4fe",
];
const DARK_REFS: [&str; 7] = [
    "#b91c1c", "#c2410c", "#a16207", "#15803d", "#0e7490", "#1d4ed8", "#7e22ce",
];

fn angles(refs: &[&str; 7]) -> [f64; 7] {
    refs.map(|h| Rgba::from_hex(h).unwrap_or_default().oklch().h)
}

/// Light text on a dark page is a dark theme.
pub fn detect_mode(t: &Theme) -> Mode {
    if t.get("text").luminance() > t.get("background").luminance() {
        Mode::Dark
    } else {
        Mode::Light
    }
}

/// Migrate a V1 theme to a V2 document.
pub fn migrate(theme: &Value) -> Result<Value, String> {
    let light = v1::resolve(theme, "light")?;
    let dark = v1::resolve(theme, "dark")?;
    let (lb, db) = (light.get("background"), dark.get("background"));
    if lb.a > 0 && db.a > 0 && lb == db {
        let (lm, dm) = (detect_mode(&light), detect_mode(&dark));
        if lm == dm {
            let mut def = migrate_mode(if lm == Mode::Light { &light } else { &dark }, lm);
            let hue = def.remove("hue").unwrap_or(Value::Null);
            let mut doc = Map::new();
            doc.insert("base".into(), Value::Object(def));
            doc.insert(lm.key().into(), json!({ "hue": hue }));
            return Ok(Value::Object(doc));
        }
    }
    let mut light_def = migrate_mode(&light, Mode::Light);
    let dark_def = migrate_mode(&dark, Mode::Dark);
    let light_hue = light_def.remove("hue").unwrap_or(Value::Null);
    Ok(json!({
        "base": light_def,
        "light": { "hue": light_hue },
        "dark": dark_def,
    }))
}

/// The selected-row text for a row of `background`.
fn selected_foreground(t: &Theme, background: Rgba) -> Rgba {
    if t.has_selected_list_item_text {
        return t.get("selectedListItemText");
    }
    if t.get("background").a != 0 {
        return t.get("background");
    }
    if background.luminance() > 0.5 {
        Rgba::rgb(0, 0, 0)
    } else {
        Rgba::rgb(255, 255, 255)
    }
}

/// Dialogs float over the session, so a see-through panel would show what
/// is underneath: they take the first visible surface color, opaque.
fn dialog_background(t: &Theme) -> String {
    if t.get("backgroundPanel").a == 255 {
        return "$background.raised.base".into();
    }
    let visible = [
        "backgroundPanel",
        "backgroundMenu",
        "backgroundElement",
        "background",
    ]
    .iter()
    .map(|k| t.get(k))
    .find(|c| c.a > 0);
    match visible {
        Some(c) => Rgba::rgb(c.r, c.g, c.b).hex(),
        None if t.get("text").luminance() > 0.5 => "#000000".into(),
        None => "#ffffff".into(),
    }
}

fn hex_of(t: &Theme, key: &str) -> Value {
    Value::String(t.get(key).hex())
}

fn migrate_mode(t: &Theme, mode: Mode) -> Map<String, Value> {
    let c = |k: &str| hex_of(t, k);
    let selected = selected_foreground(t, t.get("primary")).hex();
    let destructive = selected_foreground(t, t.get("error")).hex();
    let hues = infer_hues(t);
    let mut categorical: Vec<String> = Vec::new();
    for token in CATEGORICAL_TOKENS {
        if let Some(h) = hues.by_token(token) {
            if !categorical.iter().any(|x| x == h) {
                categorical.push(h.to_string());
            }
        }
    }
    if categorical.is_empty() {
        categorical.push("neutral".into());
    }
    let (text, muted, primary) = (
        "$hue.neutral.200",
        "$hue.neutral.400",
        "$hue.interactive.200",
    );
    let (background, panel, menu, max) = (
        "$hue.neutral.800",
        "$hue.neutral.700",
        "$hue.neutral.600",
        "$hue.neutral.500",
    );
    let mut hue = Map::new();
    hue.insert("gray".into(), scale_value(&neutral_scale(t)));
    for (i, name) in CHROMATIC.iter().enumerate() {
        if let Some(color) = hues.by_hue[i] {
            hue.insert(name.to_string(), scale_value(&hue_scale(color.0, mode)));
        }
    }
    let alias = |token: &str| match hues.by_token(token) {
        Some(h) => format!("$hue.{h}"),
        None => "$hue.gray".into(),
    };
    hue.insert("accent".into(), json!(alias("accent")));
    hue.insert("interactive".into(), json!(alias("primary")));
    hue.insert("neutral".into(), json!("$hue.gray"));

    let def = json!({
        "hue": hue,
        "categorical": categorical,
        "text": {
            "base": text,
            "muted": muted,
            "action": {
                "primary": { "base": "$text.base", "$disabled": muted, "$focused": selected, "$selected": primary },
                "secondary": { "base": "$text.muted", "$hovered": "$text.base" },
                "destructive": { "base": destructive, "$disabled": muted },
            },
            "formfield": {
                "base": text, "$hovered": primary, "$focused": primary, "$pressed": primary,
                "$disabled": muted, "$selected": primary,
            },
            "feedback": {
                "error": { "base": c("error") },
                "warning": { "base": c("warning") },
                "success": { "base": c("success") },
                "info": { "base": c("info") },
            },
        },
        "background": {
            "base": background,
            "raised": { "base": panel, "high": menu, "max": max },
            "action": {
                "primary": { "base": "transparent", "$hovered": panel, "$focused": primary, "$selected": "transparent" },
                "secondary": { "base": "transparent" },
                "destructive": { "base": c("error") },
            },
            "formfield": { "base": "$background.base" },
            "feedback": {
                "error": { "base": "$background.base" },
                "warning": { "base": "$background.base" },
                "success": { "base": "$background.base" },
                "info": { "base": "$background.base" },
            },
        },
        "border": { "base": c("border") },
        "scrollbar": { "base": c("borderActive") },
        "diff": {
            "text": {
                "added": c("diffAdded"), "removed": c("diffRemoved"),
                "context": c("diffContext"), "hunkHeader": c("diffHunkHeader"),
            },
            "background": {
                "added": c("diffAddedBg"), "removed": c("diffRemovedBg"), "context": c("diffContextBg"),
            },
            "highlight": { "added": c("diffHighlightAdded"), "removed": c("diffHighlightRemoved") },
            "lineNumber": {
                "text": c("diffLineNumber"),
                "background": { "added": c("diffAddedLineNumberBg"), "removed": c("diffRemovedLineNumberBg") },
            },
        },
        "syntax": {
            "comment": c("syntaxComment"), "keyword": c("syntaxKeyword"), "function": c("syntaxFunction"),
            "variable": c("syntaxVariable"), "string": c("syntaxString"), "number": c("syntaxNumber"),
            "type": c("syntaxType"), "operator": c("syntaxOperator"), "punctuation": c("syntaxPunctuation"),
        },
        "markdown": {
            "text": c("markdownText"), "heading": c("markdownHeading"), "link": c("markdownLink"),
            "linkText": c("markdownLinkText"), "code": c("markdownCode"), "blockQuote": c("markdownBlockQuote"),
            "emphasis": c("markdownEmph"), "strong": c("markdownStrong"),
            "horizontalRule": c("markdownHorizontalRule"), "listItem": c("markdownListItem"),
            "listEnumeration": c("markdownListEnumeration"), "image": c("markdownImage"),
            "imageText": c("markdownImageText"), "codeBlock": c("markdownCodeBlock"),
        },
        "@dialog": {
            "background": {
                "base": dialog_background(t),
                "action": { "primary": { "$hovered": "$background.raised.high" } },
            },
        },
    });
    match reference_hues(def) {
        Value::Object(o) => o,
        _ => Map::new(),
    }
}

fn scale_value(scale: &[String; 9]) -> Value {
    Value::Object(
        STEPS
            .iter()
            .zip(scale)
            .map(|(s, c)| (s.to_string(), Value::String(c.clone())))
            .collect(),
    )
}

/// Replace every token color that is a step of a generated hue with a
/// reference to it, preferring the semantic hues.
fn reference_hues(mut def: Value) -> Value {
    let hue = def["hue"].as_object().cloned().unwrap_or_default();
    fn scale_of<'a>(
        name: &str,
        hue: &'a Map<String, Value>,
        depth: usize,
    ) -> Option<&'a Map<String, Value>> {
        match hue.get(name)? {
            Value::Object(o) => Some(o),
            Value::String(s) if depth < 8 => scale_of(s.strip_prefix("$hue.")?, hue, depth + 1),
            _ => None,
        }
    }
    let mut refs: std::collections::HashMap<String, String> = Default::default();
    let mut index = |name: &str, overwrite: bool| {
        let Some(scale) = scale_of(name, &hue, 0) else {
            return;
        };
        for step in STEPS {
            let Some(c) = scale.get(&step.to_string()).and_then(Value::as_str) else {
                continue;
            };
            let k = c.to_lowercase();
            if !overwrite && refs.contains_key(&k) {
                continue;
            }
            refs.insert(k, format!("$hue.{name}.{step}"));
        }
    };
    for n in CHROMATIC {
        index(n, false);
    }
    index("gray", false);
    index("accent", true);
    index("interactive", true);
    index("neutral", true);

    fn replace(v: &mut Value, refs: &std::collections::HashMap<String, String>) {
        match v {
            Value::String(s) => {
                if let Some(r) = refs.get(&s.to_lowercase()) {
                    *s = r.clone();
                }
            }
            Value::Object(o) => o.values_mut().for_each(|x| replace(x, refs)),
            _ => {}
        }
    }
    if let Value::Object(o) = &mut def {
        for (k, v) in o.iter_mut() {
            if k != "hue" && k != "categorical" {
                replace(v, &refs);
            }
        }
    }
    def
}

/// Which chromatic hue each semantic V1 color is closest to, and the
/// color each hue is generated from.
pub struct Inferred {
    /// Per [`CHROMATIC`] hue: the color and its distance.
    by_hue: [Option<(Rgba, f64)>; 7],
    by_token: Vec<(&'static str, &'static str)>,
}

impl Inferred {
    fn by_token(&self, token: &str) -> Option<&'static str> {
        self.by_token
            .iter()
            .rev()
            .find(|(t, _)| *t == token)
            .map(|(_, h)| *h)
    }
}

fn infer_hues(t: &Theme) -> Inferred {
    let mut out = Inferred {
        by_hue: [None; 7],
        by_token: Vec::new(),
    };
    for token in [
        "accent",
        "success",
        "warning",
        "primary",
        "error",
        "info",
        "secondary",
    ] {
        let color = t.get(token);
        let Some((i, distance)) = infer_hue(color) else {
            continue;
        };
        if !out.by_hue[i].is_some_and(|(_, d)| d <= distance) {
            out.by_hue[i] = Some((color, distance));
        }
        out.by_token.push((token, CHROMATIC[i]));
    }
    // Accent and primary own the hues they fall in.
    for token in ["accent", "primary"] {
        let color = t.get(token);
        if let Some((i, distance)) = infer_hue(color) {
            out.by_hue[i] = Some((color, distance));
            out.by_token.push((token, CHROMATIC[i]));
        }
    }
    out
}

fn infer_hue(color: Rgba) -> Option<(usize, f64)> {
    let v = color.oklch();
    if color.a == 0 || v.c < MINIMUM_CHROMA {
        return None;
    }
    let refs = if v.l >= LIGHT_THRESHOLD {
        angles(&LIGHT_REFS)
    } else {
        angles(&DARK_REFS)
    };
    let mut best = (0, f64::INFINITY);
    for (i, a) in refs.iter().enumerate() {
        let d = (v.h - a).abs();
        let d = d.min(360.0 - d);
        if d < best.1 {
            best = (i, d);
        }
    }
    Some(best)
}

fn alpha_suffix(hex: String, a: u8) -> String {
    if a == 255 {
        hex
    } else {
        format!("{hex}{a:02x}")
    }
}

/// Nine steps around `color` at 200, toward white (light) or black (dark).
fn hue_scale(color: Rgba, mode: Mode) -> [String; 9] {
    let v = color.oklch();
    let end = match mode {
        Mode::Light => v.l.max(0.97),
        Mode::Dark => v.l.min(0.18),
    };
    STEPS.map(|step| {
        if step == 200 {
            return color.hex();
        }
        let p = (step as f64 - 200.0) / 700.0;
        let generated = oklch_to_hex(Oklch {
            l: v.l + (end - v.l) * p,
            c: v.c * (1.0 - p * 0.5),
            h: v.h,
        });
        alpha_suffix(generated, color.a)
    })
}

/// A gray from the theme's text and surfaces: text at 200, muted text at
/// 400, element, panel and page at 600, 700 and 800, the rest
/// interpolated (and extrapolated) in OKLCH.
fn neutral_scale(t: &Theme) -> [String; 9] {
    let anchors: [(u32, Rgba); 5] = [
        (200, t.get("text")),
        (400, t.get("textMuted")),
        (600, t.get("backgroundElement")),
        (700, t.get("backgroundPanel")),
        (800, t.get("background")),
    ];
    STEPS.map(|step| {
        if let Some((_, c)) = anchors.iter().find(|(s, _)| *s == step) {
            return c.hex();
        }
        let (lo, hi) = if step < anchors[0].0 {
            (anchors[0], anchors[1])
        } else if step > anchors[4].0 {
            (anchors[3], anchors[4])
        } else {
            let lo = *anchors.iter().rev().find(|(s, _)| *s < step).unwrap();
            let hi = *anchors.iter().find(|(s, _)| *s > step).unwrap();
            (lo, hi)
        };
        interpolate(
            lo.1,
            hi.1,
            (step as f64 - lo.0 as f64) / (hi.0 as f64 - lo.0 as f64),
        )
    })
}

fn interpolate(first: Rgba, second: Rgba, amount: f64) -> String {
    let (a, b) = (first.oklch(), second.oklch());
    let start = if a.h.is_finite() {
        a.h
    } else if b.h.is_finite() {
        b.h
    } else {
        0.0
    };
    let end = if b.h.is_finite() { b.h } else { start };
    let turn = (((end - start) % 360.0) + 540.0) % 360.0 - 180.0;
    let generated = oklch_to_hex(Oklch {
        l: a.l + (b.l - a.l) * amount,
        c: a.c + (b.c - a.c) * amount,
        h: start + turn * amount,
    });
    let alpha = (first.a as f64 + (second.a as f64 - first.a as f64) * amount)
        .round()
        .clamp(0.0, 255.0) as u8;
    alpha_suffix(generated, alpha)
}
