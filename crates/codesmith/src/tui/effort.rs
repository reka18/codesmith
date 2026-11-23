//! `/effort`: the current model's reasoning effort levels, as its provider
//! lists them (from the provider's API where it says, see
//! `codesmith_core::providers::hosted`), offered only for models that have
//! them.

use super::app::PickItem;

/// Variant names that are effort levels, lowest first (OpenCode's
/// `ReasoningEfforts`).
pub const LEVELS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// The picker row that goes back to the provider's default effort.
pub const DEFAULT: &str = "default";

/// The effort levels among a model's variants, lowest first.
pub fn levels(variants: &[String]) -> Vec<String> {
    LEVELS
        .iter()
        .filter(|l| variants.iter().any(|v| v == *l))
        .map(|l| l.to_string())
        .collect()
}

/// What each level means, for its picker row.
fn describe(level: &str) -> &'static str {
    match level {
        "none" => "no reasoning",
        "minimal" => "the least reasoning",
        "low" => "quick answers",
        "medium" => "balanced",
        "high" => "more thorough",
        "xhigh" => "very thorough",
        "max" => "the most reasoning",
        _ => "",
    }
}

/// The effort picker: the provider's default, then each level; `current`
/// (the spec's variant) marked, the default when it is none of them.
pub fn items(levels: &[String], current: Option<&str>) -> Vec<PickItem> {
    let current = current.filter(|c| levels.iter().any(|l| l == c));
    let mut out = vec![PickItem {
        id: DEFAULT.into(),
        label: DEFAULT.into(),
        detail: "the provider's default".into(),
        current: current.is_none(),
    }];
    out.extend(levels.iter().map(|l| PickItem {
        id: l.clone(),
        label: l.clone(),
        detail: describe(l).into(),
        current: current == Some(l.as_str()),
    }));
    out
}

/// The model spec `/effort ARG` switches `base` to, or why it can't.
pub fn choose(base: &str, levels: &[String], arg: &str) -> Result<String, String> {
    let arg = arg.trim().to_ascii_lowercase();
    if levels.is_empty() {
        return Err(no_levels(base));
    }
    if arg == DEFAULT {
        return Ok(base.to_string());
    }
    if levels.contains(&arg) {
        return Ok(format!("{base}#{arg}"));
    }
    Err(format!(
        "{base} has no effort {arg:?}; it offers {}, or default",
        levels.join(", ")
    ))
}

/// Why `/effort` does nothing for a model.
pub fn no_levels(base: &str) -> String {
    format!("{base} has no effort levels: its provider doesn't offer reasoning effort for it")
}

#[cfg(test)]
#[path = "effort_tests.rs"]
mod tests;
