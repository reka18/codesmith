//! OpenCode's first theme format: `defs` plus a flat `theme` of named
//! colors, each a hex value, a reference, an ANSI number or a
//! `{"dark", "light"}` pair (`@opencode/theme/tui/v1`).

use super::color::{ansi, Rgba};
use serde_json::Value;

/// The colors a V1 theme names, in OpenCode's order.
pub const KEYS: [&str; 52] = [
    "primary",
    "secondary",
    "accent",
    "error",
    "warning",
    "success",
    "info",
    "text",
    "textMuted",
    "selectedListItemText",
    "background",
    "backgroundPanel",
    "backgroundElement",
    "backgroundMenu",
    "border",
    "borderActive",
    "borderSubtle",
    "diffAdded",
    "diffRemoved",
    "diffContext",
    "diffHunkHeader",
    "diffHighlightAdded",
    "diffHighlightRemoved",
    "diffAddedBg",
    "diffRemovedBg",
    "diffContextBg",
    "diffLineNumber",
    "diffAddedLineNumberBg",
    "diffRemovedLineNumberBg",
    "markdownText",
    "markdownHeading",
    "markdownLink",
    "markdownLinkText",
    "markdownCode",
    "markdownBlockQuote",
    "markdownEmph",
    "markdownStrong",
    "markdownHorizontalRule",
    "markdownListItem",
    "markdownListEnumeration",
    "markdownImage",
    "markdownImageText",
    "markdownCodeBlock",
    "syntaxComment",
    "syntaxKeyword",
    "syntaxFunction",
    "syntaxVariable",
    "syntaxString",
    "syntaxNumber",
    "syntaxType",
    "syntaxOperator",
    "syntaxPunctuation",
];

/// A V1 theme resolved for one mode.
#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    colors: std::collections::BTreeMap<String, Rgba>,
    pub has_selected_list_item_text: bool,
    pub thinking_opacity: f64,
}

impl Theme {
    /// The named color (black when a custom theme leaves it out).
    pub fn get(&self, key: &str) -> Rgba {
        self.colors.get(key).copied().unwrap_or(Rgba::rgb(0, 0, 0))
    }

    pub fn has(&self, key: &str) -> bool {
        self.colors.contains_key(key)
    }
}

/// Whether `v` is a V1 theme (it has a `theme` object).
pub fn is_v1(v: &Value) -> bool {
    v.get("theme").is_some()
}

/// Resolve a V1 theme for `mode` (`"dark"` or `"light"`), following
/// references through `defs` and the theme itself.
pub fn resolve(theme: &Value, mode: &str) -> Result<Theme, String> {
    let empty = serde_json::Map::new();
    let defs = theme
        .get("defs")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let colors = theme
        .get("theme")
        .and_then(Value::as_object)
        .ok_or("Invalid V1 theme color")?;

    fn color(
        v: &Value,
        chain: &mut Vec<String>,
        mode: &str,
        defs: &serde_json::Map<String, Value>,
        colors: &serde_json::Map<String, Value>,
    ) -> Result<Rgba, String> {
        match v {
            Value::String(s) if s == "transparent" || s == "none" => Ok(Rgba::TRANSPARENT),
            Value::String(s) if s.starts_with('#') => {
                Rgba::from_hex(s).ok_or_else(|| format!("Invalid color \"{s}\""))
            }
            Value::String(s) => {
                if chain.contains(s) {
                    let mut c = chain.clone();
                    c.push(s.clone());
                    return Err(format!("Circular color reference: {}", c.join(" -> ")));
                }
                let next = defs
                    .get(s)
                    .or_else(|| colors.get(s))
                    .ok_or_else(|| format!("Color reference \"{s}\" not found in defs or theme"))?;
                chain.push(s.clone());
                let r = color(next, chain, mode, defs, colors);
                chain.pop();
                r
            }
            Value::Number(n) => Ok(ansi(n.as_u64().unwrap_or(0))),
            Value::Object(o) if o.contains_key(mode) => color(&o[mode], chain, mode, defs, colors),
            _ => Err("Invalid V1 theme color".into()),
        }
    }

    let mut out = std::collections::BTreeMap::new();
    for (k, v) in colors {
        if matches!(
            k.as_str(),
            "selectedListItemText" | "backgroundMenu" | "thinkingOpacity"
        ) {
            continue;
        }
        out.insert(k.clone(), color(v, &mut vec![], mode, defs, colors)?);
    }
    let has_selected = colors.contains_key("selectedListItemText");
    let selected = match colors.get("selectedListItemText") {
        Some(v) => color(v, &mut vec![], mode, defs, colors)?,
        None => out.get("background").copied().unwrap_or_default(),
    };
    out.insert("selectedListItemText".into(), selected);
    let menu = match colors.get("backgroundMenu") {
        Some(v) => color(v, &mut vec![], mode, defs, colors)?,
        None => out.get("backgroundElement").copied().unwrap_or_default(),
    };
    out.insert("backgroundMenu".into(), menu);
    Ok(Theme {
        colors: out,
        has_selected_list_item_text: has_selected,
        thinking_opacity: colors
            .get("thinkingOpacity")
            .and_then(Value::as_f64)
            .unwrap_or(0.6),
    })
}
