//! The model variant `variant.cycle` (and `/variant` with no name) moves
//! to, as OpenCode's `cycleVariant` (`mini/variant.shared`): the first
//! variant from the default (or one the model does not have), then each
//! in turn, then back to the default (`None`).

pub fn cycle(current: Option<&str>, variants: &[String]) -> Option<String> {
    match current.and_then(|c| variants.iter().position(|v| v == c)) {
        None => variants.first().cloned(),
        Some(i) => variants.get(i + 1).cloned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // mini/variant.shared.test.ts: "cycles through variants and back to
    // default".
    #[test]
    fn oc_cycles_through_variants_and_back_to_default() {
        let v = vec!["low".to_string(), "high".to_string()];
        assert_eq!(cycle(None, &v).as_deref(), Some("low"));
        assert_eq!(cycle(Some("default"), &v).as_deref(), Some("low"));
        assert_eq!(cycle(Some("low"), &v).as_deref(), Some("high"));
        assert_eq!(cycle(Some("high"), &v), None);
        assert_eq!(cycle(None, &[]), None);
    }
}
