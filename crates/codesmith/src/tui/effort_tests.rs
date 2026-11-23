use super::*;

fn v(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| s.to_string()).collect()
}

#[test]
fn levels_are_the_effort_variants_lowest_first() {
    assert_eq!(
        levels(&v(&["max", "fast", "low", "high", "medium", "xhigh"])),
        ["low", "medium", "high", "xhigh", "max"]
    );
    assert_eq!(levels(&v(&["none", "minimal"])), ["none", "minimal"]);
}

#[test]
fn variants_that_are_not_efforts_are_left_out() {
    assert!(levels(&v(&["fast", "creative", "High", "ultra"])).is_empty());
    assert!(levels(&[]).is_empty());
}

#[test]
fn the_picker_starts_with_the_default_and_marks_the_current_level() {
    let l = v(&["low", "medium", "high"]);
    let items = items(&l, Some("medium"));
    let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(ids, ["default", "low", "medium", "high"]);
    let current: Vec<&str> = items
        .iter()
        .filter(|i| i.current)
        .map(|i| i.id.as_str())
        .collect();
    assert_eq!(current, ["medium"]);
    assert_eq!(items[3].detail, "more thorough");
}

#[test]
fn no_level_or_a_variant_that_is_not_one_marks_the_default() {
    let l = v(&["low", "high"]);
    for current in [None, Some("fast"), Some("max")] {
        let items = items(&l, current);
        assert!(items[0].current, "{current:?}");
        assert_eq!(items.iter().filter(|i| i.current).count(), 1);
    }
}

#[test]
fn choosing_a_level_puts_it_on_the_model() {
    let l = v(&["low", "medium", "high", "max"]);
    assert_eq!(
        choose("anthropic/claude-x", &l, "high").unwrap(),
        "anthropic/claude-x#high"
    );
    assert_eq!(
        choose("anthropic/claude-x", &l, "  MAX ").unwrap(),
        "anthropic/claude-x#max"
    );
    assert_eq!(
        choose("anthropic/claude-x", &l, "default").unwrap(),
        "anthropic/claude-x"
    );
}

#[test]
fn a_level_the_model_lacks_is_refused_with_the_ones_it_has() {
    let l = v(&["low", "medium", "high"]);
    let e = choose("openai/gpt-x", &l, "xhigh").unwrap_err();
    assert!(e.contains("\"xhigh\""), "{e}");
    assert!(e.contains("low, medium, high, or default"), "{e}");
    // A spec suffix or path is not a level.
    for bad in ["high#x", "../high", "high max", ""] {
        assert!(choose("openai/gpt-x", &l, bad).is_err(), "{bad:?}");
    }
}

#[test]
fn a_model_without_levels_says_so_for_every_argument() {
    for arg in ["high", "default", ""] {
        let e = choose("groq/llama", &[], arg).unwrap_err();
        assert!(e.contains("groq/llama has no effort levels"), "{e}");
    }
}

#[test]
fn many_variants_are_sorted_quickly() {
    let mut names: Vec<String> = (0..20_000).map(|i| format!("custom-{i}")).collect();
    names.extend(v(&["high", "low"]));
    let started = std::time::Instant::now();
    assert_eq!(levels(&names), ["low", "high"]);
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
}
