//! OpenCode's theme tests, ported: `tui/test/theme/v2/{resolve, v1-migrate,
//! select, component}`, `tui/test/theme`, `tui/test/cli/tui/theme-mode` and
//! `tui/test/context/theme`. Each `oc_*` test names the OpenCode test it
//! ports; "(adapted)" ones note what differs.

use super::color::{rgb_to_oklch, Rgba};
use super::doc::{self, Mode, Resolved, C};
use super::system::{self, TerminalColors};
use super::*;
use serde_json::{json, Value};

fn opencode_v1() -> Value {
    serde_json::from_str(OPENCODE_V1).unwrap()
}

fn selected(mode: Mode) -> Value {
    doc::select(opencode(), Some(mode)).unwrap().0
}

/// `light` and `dark` of `resolve.test.ts`: OpenCode's theme with blue
/// semantic hues and two categorical colors.
fn def(mode: Mode) -> Value {
    let mut d = selected(mode);
    d["categorical"] = json!(["blue", "purple"]);
    d["hue"]["accent"] = json!("$hue.blue");
    d["hue"]["interactive"] = json!("$hue.blue");
    d["hue"]["neutral"] = json!("$hue.gray");
    d
}

fn over(base: &Value, value: Value) -> Value {
    doc::merge(&[base, &value])
}

fn complete(mode: Mode, value: Value) -> Value {
    let mut d = over(&def(mode), value);
    let hue = d.as_object_mut().unwrap().remove("hue").unwrap();
    let mut doc = serde_json::Map::new();
    doc.insert("base".into(), d);
    doc.insert(mode.key().into(), json!({ "hue": hue }));
    Value::Object(doc)
}

fn resolve_source(source: &Value, mode: Option<Mode>, name: &str) -> Result<Resolved, String> {
    let d = parse(source, name)?;
    doc::resolve(&d, mode)
}

fn ints(t: &Resolved, c: C) -> [u8; 4] {
    let x = t.rgba(c);
    [x.r, x.g, x.b, x.a]
}

fn hue(t: &Resolved, name: &str) -> [C; 9] {
    t.hue(name).unwrap_or_else(|| panic!("no hue {name}"))
}

fn at(scale: [C; 9], step: u32) -> C {
    scale[doc::STEPS.iter().position(|s| *s == step).unwrap()]
}

fn lum(t: &Resolved, c: C) -> f64 {
    t.rgba(c).luminance()
}

// ----- theme/v2/resolve ----------------------------------------------------

#[test]
fn oc_orders_light_hues_dark_to_light_and_dark_hues_light_to_dark() {
    let l = doc::resolve_definition(&def(Mode::Light)).unwrap();
    let d = doc::resolve_definition(&def(Mode::Dark)).unwrap();
    for name in ["gray", "red", "orange", "green", "cyan", "blue", "purple"] {
        assert!(
            lum(&l, at(hue(&l, name), 100)) < lum(&l, at(hue(&l, name), 900)),
            "{name}"
        );
        assert!(
            lum(&d, at(hue(&d, name), 100)) > lum(&d, at(hue(&d, name), 900)),
            "{name}"
        );
    }
}

#[test]
fn oc_resolves_complete_one_mode_documents_in_the_available_mode() {
    let l = resolve_source(&complete(Mode::Light, json!({})), Some(Mode::Dark), "theme").unwrap();
    let d = resolve_source(&complete(Mode::Dark, json!({})), Some(Mode::Light), "theme").unwrap();
    let rl = doc::resolve_definition(&def(Mode::Light)).unwrap();
    let rd = doc::resolve_definition(&def(Mode::Dark)).unwrap();
    assert_eq!(l.rgba(l.background.base), rl.rgba(rl.background.base));
    assert_eq!(d.rgba(d.background.base), rd.rgba(rd.background.base));
    assert!(!l.categorical().is_empty() && !d.categorical().is_empty());
}

#[test]
fn oc_rejects_theme_documents_without_a_mode() {
    let e = resolve_source(&json!({}), None, "theme").unwrap_err();
    assert!(e.contains("Invalid theme"), "{e}");
}

#[test]
fn oc_validates_and_resolves_categorical_hues_in_configured_order() {
    let src = complete(
        Mode::Light,
        json!({ "categorical": ["accent", "red", "interactive"] }),
    );
    let t = resolve_source(&src, Some(Mode::Light), "theme").unwrap();
    let cat = t.categorical();
    assert_eq!(cat[0], hue(&t, "accent"));
    assert_eq!(cat[1], hue(&t, "red"));
    assert_eq!(cat[2], hue(&t, "interactive"));
    assert_eq!(t.surface("dialog").categorical(), cat);
    for bad in [json!([]), json!(["magenta"])] {
        let mut src = complete(Mode::Light, json!({}));
        src["base"]["categorical"] = bad;
        let e = resolve_source(&src, Some(Mode::Light), "theme").unwrap_err();
        assert!(e.contains("Invalid theme"), "{e}");
    }
}

#[test]
fn oc_resolves_arbitrary_hue_names_across_aliases_categorical_colors_and_token_references() {
    let light = def(Mode::Light);
    let ocean = light["hue"]["blue"].clone();
    assert!(ocean.is_object());
    let mut d = light.clone();
    d["hue"]["brand.ocean"] = ocean;
    d["hue"]["accent"] = json!("$hue.brand.ocean");
    d["categorical"] = json!(["brand.ocean"]);
    d["syntax"]["keyword"] = json!("$hue.brand.ocean.200");
    let t = doc::resolve_definition(&d).unwrap();
    let ocean = hue(&t, "brand.ocean");
    assert_eq!(t.rgba(t.step("accent", 200)), t.rgba(at(ocean, 200)));
    assert_eq!(t.categorical()[0], ocean);
    assert_eq!(t.syntax("keyword"), at(ocean, 200));
    assert_eq!(t.source(t.syntax("keyword")), Some(("brand.ocean", 200)));
}

#[test]
fn oc_validates_hue_relationships_in_every_provided_mode_while_parsing() {
    let mut broken = opencode().clone();
    broken["dark"]["hue"]["accent"] = json!("$hue.missing");
    let mut missing = opencode().clone();
    missing["light"]["hue"]
        .as_object_mut()
        .unwrap()
        .remove("neutral");
    let e = parse(&broken, "broken-dark").unwrap_err();
    assert!(e.contains("Invalid theme: broken-dark"), "{e}");
    let e = parse(&missing, "missing-semantic").unwrap_err();
    assert!(e.contains("Invalid theme: missing-semantic"), "{e}");
}

// (adapted) OpenTUI's syntax style is a syntect theme here: one
// categorical hue is enough to build it and the palette.
#[test]
fn oc_generates_syntax_with_one_categorical_hue() {
    let src = complete(Mode::Light, json!({ "categorical": ["red"] }));
    let t = resolve_source(&src, Some(Mode::Light), "theme").unwrap();
    let p = Palette::new(&t);
    assert!(!p.syntect().scopes.is_empty());
    assert_eq!(
        p.agent,
        ratatui::style::Color::Rgb(
            t.rgba(at(hue(&t, "red"), 200)).r,
            t.rgba(at(hue(&t, "red"), 200)).g,
            t.rgba(at(hue(&t, "red"), 200)).b
        )
    );
}

#[test]
fn oc_rejects_incomplete_themes_instead_of_merging_defaults() {
    let light = def(Mode::Light);
    let e = resolve_source(
        &json!({ "light": { "hue": light["hue"] } }),
        Some(Mode::Light),
        "theme",
    )
    .unwrap_err();
    assert!(e.contains("Invalid theme"));
    let mut partial = light.clone();
    partial.as_object_mut().unwrap().remove("categorical");
    let e = resolve_source(&json!({ "light": partial }), Some(Mode::Light), "theme").unwrap_err();
    assert!(e.contains("Invalid theme"));
}

#[test]
fn oc_resolves_independent_definitions_and_hue_aliases() {
    let l = doc::resolve_definition(&def(Mode::Light)).unwrap();
    let d = doc::resolve_definition(&def(Mode::Dark)).unwrap();
    let (blue, gray, red) = (hue(&l, "blue"), hue(&l, "gray"), hue(&l, "red"));
    assert_ne!(hue(&l, "accent"), blue);
    assert_eq!(l.rgba(l.step("accent", 500)), l.rgba(at(blue, 500)));
    assert_ne!(hue(&l, "interactive"), blue);
    assert_eq!(l.rgba(l.step("interactive", 500)), l.rgba(at(blue, 500)));
    assert_ne!(hue(&l, "neutral"), gray);
    assert_eq!(l.rgba(l.step("neutral", 500)), l.rgba(at(gray, 500)));
    assert_eq!(l.categorical()[0], blue);
    assert_eq!(l.source(at(blue, 500)), Some(("blue", 500)));
    assert_eq!(l.source(l.step("neutral", 200)), Some(("neutral", 200)));
    assert_eq!(l.source(l.background.raised.base), Some(("neutral", 700)));
    assert_eq!(l.increase(at(red, 100), 1), at(red, 200));
    assert_eq!(l.decrease(at(red, 200), 1), at(red, 100));
    assert_eq!(l.surface("dialog").increase(at(red, 100), 1), at(red, 200));
    let dred = hue(&d, "red");
    assert_eq!(d.decrease(at(dred, 200), 1), at(dred, 100));
    assert_eq!(l.background.raised.base, l.step("neutral", 700));
    assert_eq!(l.background.raised.high, l.step("neutral", 600));
    assert_eq!(l.text.action[0].base, l.text.base);
    // Surfaces re-resolve the palette after their overrides.
    let dialog = l.surface("dialog");
    assert_eq!(dialog.background.base, l.background.raised.base);
    assert_eq!(dialog.background.formfield.base, l.background.raised.base);
    assert_eq!(dialog.background.feedback[0], l.background.raised.base);
    assert_eq!(
        dialog.background.action[0].hovered,
        l.background.raised.high
    );
    assert_eq!(
        dialog.rgba(dialog.background.action[0].base),
        l.rgba(l.background.action[0].base)
    );
    assert_eq!(
        dialog.rgba(dialog.background.action[0].focused),
        l.rgba(l.background.action[0].focused)
    );
    assert_eq!(
        dialog.rgba(dialog.text.action[0].base),
        l.rgba(l.text.action[0].base)
    );
    assert!(dialog.surface("dialog").same(&dialog));
    assert_eq!(
        d.surface("dialog").background.base,
        d.background.raised.base
    );
}

#[test]
fn oc_resolves_base_hue_aliases_and_rejects_circular_hue_aliases() {
    let mut aliased = def(Mode::Light);
    aliased["hue"]["blue"] = json!("$hue.red");
    aliased["hue"]["purple"] = json!("$hue.blue");
    let a = doc::resolve_definition(&aliased).unwrap();
    let mut o = def(Mode::Light)["hue"].clone();
    o["blue"] = json!("$hue.red");
    let ov = resolve_source(
        &complete(Mode::Light, json!({ "hue": o })),
        Some(Mode::Light),
        "t",
    )
    .unwrap();
    assert_ne!(hue(&a, "blue"), hue(&a, "red"));
    assert_eq!(a.rgba(a.step("blue", 500)), a.rgba(a.step("red", 500)));
    assert_ne!(hue(&a, "purple"), hue(&a, "blue"));
    assert_eq!(a.rgba(a.step("purple", 500)), a.rgba(a.step("red", 500)));
    assert_ne!(hue(&ov, "blue"), hue(&ov, "red"));
    assert_eq!(ov.rgba(ov.step("blue", 500)), ov.rgba(ov.step("red", 500)));
    assert_eq!(a.source(a.step("red", 500)), Some(("red", 500)));
    assert_eq!(a.source(a.step("blue", 500)), Some(("blue", 500)));
    assert_eq!(a.source(a.step("purple", 500)), Some(("purple", 500)));
    let mut circular = def(Mode::Light);
    circular["hue"]["red"] = json!("$hue.blue");
    circular["hue"]["blue"] = json!("$hue.red");
    let e = doc::resolve_definition(&circular).unwrap_err();
    assert!(
        e.contains("Circular hue reference: red -> blue -> red"),
        "{e}"
    );
}

#[test]
fn oc_steps_by_hue_source_when_adjacent_colors_have_equal_values() {
    let mut d = def(Mode::Light);
    d["hue"]["gray"]["200"] = json!("#eee8d5");
    d["hue"]["gray"]["300"] = json!("#eee8d5");
    d["hue"]["gray"]["400"] = json!("#d3d7c6");
    d["hue"]["neutral"] = json!("$hue.gray");
    let t = doc::resolve_definition(&d).unwrap();
    let (n2, n3, n4) = (
        t.step("neutral", 200),
        t.step("neutral", 300),
        t.step("neutral", 400),
    );
    assert_ne!(n2, n3);
    assert_eq!(t.rgba(n2), t.rgba(n3));
    assert_eq!(t.source(n2), Some(("neutral", 200)));
    assert_eq!(t.source(n3), Some(("neutral", 300)));
    assert_eq!(t.increase(n2, 1), n3);
    assert_eq!(t.increase(n3, 1), n4);
}

#[test]
fn oc_resolves_complete_light_and_dark_definitions_independently() {
    let mut l = def(Mode::Light);
    l["text"]["action"]["secondary"] = json!({ "base": "#123456", "$hovered": "#234567" });
    let hue = l.as_object_mut().unwrap().remove("hue").unwrap();
    let document = json!({ "base": l, "light": { "hue": hue }, "dark": def(Mode::Dark) });
    let lt = resolve_source(&document, Some(Mode::Light), "t").unwrap();
    let dt = resolve_source(&document, Some(Mode::Dark), "t").unwrap();
    assert_eq!(ints(&lt, lt.text.action[1].base), [18, 52, 86, 255]);
    assert_eq!(ints(&lt, lt.text.action[1].hovered), [35, 69, 103, 255]);
    assert_eq!(dt.text.action[1].base, dt.text.muted);
    assert_eq!(dt.text.action[1].hovered, dt.text.base);
}

#[test]
fn oc_expands_structural_state_fallbacks_within_a_complete_theme() {
    let mut d = def(Mode::Light);
    d["background"]["action"]["primary"] = json!({ "base": "#123456" });
    let t = doc::resolve_definition(&d).unwrap();
    assert_eq!(ints(&t, t.background.action[0].pressed), [18, 52, 86, 255]);
}

#[test]
fn oc_resolves_matched_action_variants_and_states() {
    let t = doc::resolve_definition(&def(Mode::Light)).unwrap();
    assert_eq!(t.text.action[1].base, t.text.muted);
    assert_eq!(t.text.action[1].hovered, t.text.base);
    // Every state resolves to a color of the theme.
    for c in [
        t.text.action[0].pressed,
        t.text.action[0].hovered,
        t.text.action[0].selected,
        t.background.action[0].pressed,
        t.background.action[0].hovered,
        t.background.action[0].selected,
        t.background.action[2].disabled,
        t.background.formfield.hovered,
    ] {
        assert_ne!(c, u32::MAX);
    }
}

#[test]
fn oc_defaults_faint_text_to_muted_unless_a_theme_sets_it() {
    let fallback = doc::resolve_definition(&def(Mode::Light)).unwrap();
    let explicit = doc::resolve_definition(&over(
        &def(Mode::Light),
        json!({ "text": { "faint": "#123456" } }),
    ))
    .unwrap();
    assert_eq!(fallback.text.faint, fallback.text.muted);
    let d = fallback.surface("dialog");
    assert_eq!(d.text.faint, d.text.muted);
    assert_eq!(ints(&explicit, explicit.text.faint), [18, 52, 86, 255]);
}

#[test]
fn oc_resolves_dialog_surfaces_from_direct_colors() {
    let src = complete(
        Mode::Light,
        json!({ "background": { "raised": { "base": "#123456", "high": "#234567" } } }),
    );
    let t = resolve_source(&src, Some(Mode::Light), "t").unwrap();
    let d = t.surface("dialog");
    assert_eq!(ints(&d, d.background.base), [18, 52, 86, 255]);
    assert_eq!(ints(&d, d.background.action[0].hovered), [35, 69, 103, 255]);
}

#[test]
fn oc_resolves_transparent_colors() {
    let src = complete(
        Mode::Light,
        json!({ "background": { "formfield": { "base": "transparent" } } }),
    );
    let t = resolve_source(&src, None, "t").unwrap();
    assert_eq!(ints(&t, t.background.formfield.base), [0, 0, 0, 0]);
}

#[test]
fn oc_reports_theme_decoding_failures_as_native_errors() {
    let src = complete(Mode::Light, json!({ "text": { "base": "opaque" } }));
    let e = resolve_source(&src, Some(Mode::Light), "custom").unwrap_err();
    assert_eq!(e, "Invalid theme: custom \"opaque\" is an invalid value");
}

#[test]
fn oc_surface_overrides_rewire_references_and_reset_action_states_from_their_base() {
    let t = doc::resolve_definition(&over(
        &def(Mode::Light),
        json!({
            "text": { "base": "#111111", "action": { "primary": { "base": "$text.base", "$pressed": "#222222" } } },
            "@dialog": { "text": { "base": "#333333", "action": { "primary": { "base": "#444444", "$focused": "#555555" } } } },
        }),
    ))
    .unwrap();
    let r = t.surface("dialog");
    assert_eq!(ints(&r, r.text.base), [51, 51, 51, 255]);
    assert_eq!(ints(&r, r.text.action[0].pressed), [68, 68, 68, 255]);
    assert_eq!(ints(&r, r.text.action[0].focused), [85, 85, 85, 255]);
}

#[test]
fn oc_dialog_footer_overrides_layer_on_the_dialog_surface() {
    let t = doc::resolve_definition(&over(
        &def(Mode::Light),
        json!({ "@dialog": { "text": { "base": "#333333" }, "@footer": { "text": { "muted": "#666666" } } } }),
    ))
    .unwrap();
    let f = t.surface("dialog.footer");
    assert_eq!(ints(&f, f.text.muted), [102, 102, 102, 255]);
    assert_eq!(ints(&f, f.text.base), [51, 51, 51, 255]);
    let d = t.surface("dialog");
    assert_ne!(ints(&d, d.text.muted), [102, 102, 102, 255]);
    let plain = doc::resolve_definition(&def(Mode::Light)).unwrap();
    assert!(plain
        .surface("dialog.footer")
        .same(&plain.surface("dialog")));
}

#[test]
fn oc_rejects_missing_and_circular_references() {
    let e = doc::resolve_definition(&over(
        &def(Mode::Light),
        json!({ "text": { "base": "$missing" } }),
    ))
    .unwrap_err();
    assert!(
        e.contains("Theme reference \"$missing\" was not found"),
        "{e}"
    );
    let e = doc::resolve_definition(&over(
        &def(Mode::Light),
        json!({ "text": { "base": "$text.muted", "muted": "$text.base" } }),
    ))
    .unwrap_err();
    assert!(e.contains("Circular theme reference"), "{e}");
}

#[test]
fn oc_validates_complete_hues_resolved_groups_and_hue_only_syntax() {
    let mut d = def(Mode::Light);
    d["hue"]["accent"] = json!("$hue.missing");
    assert!(doc::resolve_definition(&d)
        .unwrap_err()
        .contains("$hue.missing"));
    let mut d = def(Mode::Light);
    d["syntax"]["keyword"] = json!("$text.base");
    assert!(doc::resolve_definition(&d)
        .unwrap_err()
        .contains("$text.base"));
}

// ----- theme/v2/v1-migrate --------------------------------------------------

fn v1(t: &Value, mode: &str) -> v1::Theme {
    v1::resolve(t, mode).unwrap()
}

fn rgba4(c: Rgba) -> [u8; 4] {
    [c.r, c.g, c.b, c.a]
}

#[test]
fn oc_migrates_resolved_v1_modes_into_v2_tokens() {
    let src = opencode_v1();
    let m = migrate::migrate(&src).unwrap();
    let (base, light, dark) = (&m["base"], &m["light"], &m["dark"]);
    assert!(light.is_object() && dark.is_object());
    let legacy = v1(&src, "light");
    let r = doc::resolve(&m, Some(Mode::Light)).unwrap();
    assert!(!base["categorical"].as_array().unwrap().is_empty());
    assert!(!dark["categorical"].as_array().unwrap().is_empty());
    let alias = |v: &Value| {
        v.as_str()
            .is_some_and(|s| s.starts_with("$hue.") && !s[5..].contains('.'))
    };
    assert!(alias(&light["hue"]["accent"]) && alias(&light["hue"]["interactive"]));
    assert_eq!(base["text"]["base"], "$hue.neutral.200");
    assert_eq!(base["text"]["muted"], "$hue.neutral.400");
    assert_eq!(
        base["background"]["action"]["primary"]["base"],
        "transparent"
    );
    assert_eq!(base["background"]["base"], "$hue.neutral.800");
    assert_eq!(base["background"]["raised"]["base"], "$hue.neutral.700");
    assert_eq!(base["background"]["raised"]["high"], "$hue.neutral.600");
    assert_eq!(dark["background"]["base"], "$hue.neutral.800");
    assert_eq!(dark["background"]["raised"]["base"], "$hue.neutral.700");
    assert_eq!(dark["background"]["raised"]["high"], "$hue.neutral.600");
    assert_eq!(base["text"]["action"]["primary"]["base"], "$text.base");
    assert_eq!(base["text"]["action"]["secondary"]["base"], "$text.muted");
    assert_eq!(
        base["text"]["action"]["secondary"]["$hovered"],
        "$text.base"
    );
    assert_eq!(
        base["background"]["action"]["primary"]["$selected"],
        "transparent"
    );
    let eq = |c: C, key: &str| assert_eq!(rgba4(r.rgba(c)), rgba4(legacy.get(key)), "{key}");
    eq(r.background.raised.base, "backgroundPanel");
    eq(r.background.raised.high, "backgroundElement");
    eq(r.background.formfield.selected, "background");
    eq(r.background.formfield.focused, "background");
    eq(r.text.formfield.base, "text");
    eq(r.text.formfield.selected, "primary");
    eq(r.text.formfield.focused, "primary");
    eq(r.step("accent", 200), "accent");
    eq(r.step("interactive", 200), "primary");
    assert_eq!(ints(&r, r.background.action[0].selected), [0, 0, 0, 0]);
    eq(r.text.action[0].selected, "primary");
    eq(r.text.action[1].base, "textMuted");
    eq(r.text.action[1].hovered, "text");
    eq(r.background.feedback[0], "background");
    let d = r.surface("dialog");
    assert_eq!(
        rgba4(d.rgba(d.background.base)),
        rgba4(legacy.get("backgroundPanel"))
    );
    assert_eq!(ints(&d, d.background.action[0].base), [0, 0, 0, 0]);
    assert_eq!(
        rgba4(d.rgba(d.text.action[0].base)),
        rgba4(legacy.get("text"))
    );
}

#[test]
fn oc_references_generated_hues_from_matching_token_colors() {
    let mut src = opencode_v1();
    src["theme"]["border"] = src["theme"]["primary"].clone();
    src["theme"]["borderActive"] = src["theme"]["accent"].clone();
    src["theme"]["syntaxKeyword"] = src["theme"]["error"].clone();
    src["theme"]["markdownEmph"] = json!("#123456");
    let m = migrate::migrate(&src).unwrap();
    assert_eq!(m["base"]["border"]["base"], "$hue.interactive.200");
    assert_eq!(m["base"]["scrollbar"]["base"], "$hue.accent.200");
    let k = m["base"]["syntax"]["keyword"].as_str().unwrap();
    assert!(
        k.starts_with("$hue.") && k.ends_with(".200") && k.matches('.').count() == 2,
        "{k}"
    );
    assert_eq!(m["base"]["markdown"]["emphasis"], "#123456");
}

#[test]
fn oc_infers_and_emits_only_chromatic_hues_represented_by_v1_colors() {
    let mut src = opencode_v1();
    let gray = json!({ "light": "#808080", "dark": "#808080" });
    for k in ["accent", "warning", "primary", "error", "info"] {
        src["theme"][k] = gray.clone();
    }
    src["theme"]["secondary"] = json!("transparent");
    src["theme"]["success"] = json!({ "light": "#ff6666", "dark": "#450000" });
    let m = migrate::migrate(&src).unwrap();
    let (lr, dr) = (&m["light"]["hue"]["red"], &m["dark"]["hue"]["red"]);
    assert_eq!(lr["200"], "#ff6666");
    assert_eq!(dr["200"], "#450000");
    assert_ne!(lr["100"], lr["200"]);
    assert_ne!(dr["100"], dr["200"]);
    for h in ["orange", "yellow", "green", "cyan", "blue", "purple"] {
        assert!(m["light"]["hue"].get(h).is_none(), "{h}");
    }
    assert_eq!(m["light"]["hue"]["accent"], "$hue.gray");
    assert_eq!(m["light"]["hue"]["interactive"], "$hue.gray");
    doc::resolve(&m, Some(Mode::Light)).unwrap();
    doc::resolve(&m, Some(Mode::Dark)).unwrap();
}

#[test]
fn oc_orders_categorical_hues_by_v1_semantic_color_mapping() {
    let mut src = opencode_v1();
    let pair = |l: &str, d: &str| json!({ "light": l, "dark": d });
    src["theme"]["secondary"] = pair("#d8b4fe", "#7e22ce");
    src["theme"]["accent"] = pair("#fdba74", "#c2410c");
    src["theme"]["success"] = pair("#86efac", "#15803d");
    src["theme"]["warning"] = pair("#fde047", "#a16207");
    src["theme"]["primary"] = pair("#93c5fd", "#1d4ed8");
    src["theme"]["error"] = pair("#fca5a5", "#b91c1c");
    src["theme"]["info"] = pair("#67e8f9", "#0e7490");
    let m = migrate::migrate(&src).unwrap();
    let want = json!(["purple", "orange", "green", "yellow", "blue", "red", "cyan"]);
    assert_eq!(m["base"]["categorical"], want);
    assert_eq!(m["dark"]["categorical"], want);
    src["theme"]["accent"] = src["theme"]["secondary"].clone();
    assert_eq!(
        migrate::migrate(&src).unwrap()["base"]["categorical"],
        json!(["purple", "green", "yellow", "blue", "red", "cyan"])
    );
}

#[test]
fn oc_gives_accent_and_primary_ownership_of_their_inferred_hues() {
    let ol = doc::resolve(opencode(), Some(Mode::Light)).unwrap();
    let hex = |name: &str, step: u32| ol.rgba(ol.step(name, step)).hex();
    let mut src = opencode_v1();
    src["theme"]["success"] = json!(hex("orange", 700));
    src["theme"]["accent"] = json!(hex("orange", 600));
    src["theme"]["info"] = json!(hex("blue", 700));
    src["theme"]["primary"] = json!(hex("blue", 600));
    let m = migrate::migrate(&src).unwrap();
    assert_eq!(m["light"]["hue"]["orange"]["200"], src["theme"]["accent"]);
    assert_eq!(m["light"]["hue"]["blue"]["200"], src["theme"]["primary"]);
    assert_eq!(m["light"]["hue"]["accent"], "$hue.orange");
    assert_eq!(m["light"]["hue"]["interactive"], "$hue.blue");
    src["theme"]["primary"] = json!(hex("orange", 500));
    let c = migrate::migrate(&src).unwrap();
    assert_eq!(c["light"]["hue"]["orange"]["200"], src["theme"]["primary"]);
    assert_eq!(c["light"]["hue"]["accent"], "$hue.orange");
    assert_eq!(c["light"]["hue"]["interactive"], "$hue.orange");
}

#[test]
fn oc_uses_the_semantic_neutral_hue_when_v1_categorical_colors_are_ambiguous() {
    let mut src = opencode_v1();
    for k in [
        "secondary",
        "accent",
        "success",
        "warning",
        "primary",
        "error",
        "info",
    ] {
        src["theme"][k] = json!("transparent");
    }
    let m = migrate::migrate(&src).unwrap();
    assert_eq!(m["base"]["categorical"], json!(["neutral"]));
    assert_eq!(m["dark"]["categorical"], json!(["neutral"]));
}

#[test]
fn oc_builds_and_extrapolates_gray_from_v1_surfaces_and_text_without_using_menus_or_borders() {
    let mut src = opencode_v1();
    let pair = |l: &str, d: &str| json!({ "light": l, "dark": d });
    src["theme"]["background"] = pair("#eeeeee", "#111111");
    src["theme"]["backgroundPanel"] = pair("#dddddd", "#222222");
    src["theme"]["backgroundElement"] = pair("#cccccc", "#333333");
    src["theme"]["textMuted"] = pair("#777777", "#999999");
    src["theme"]["text"] = pair("#333333", "#dddddd");
    src["theme"]["backgroundMenu"] = pair("#ededed", "#252525");
    let m = migrate::migrate(&src).unwrap();
    for (mode, g) in [
        ("light", &m["light"]["hue"]["gray"]),
        ("dark", &m["dark"]["hue"]["gray"]),
    ] {
        let t = v1(&src, mode);
        assert_ne!(g["100"], g["200"]);
        assert_eq!(g["200"], t.get("text").hex());
        assert_eq!(g["400"], t.get("textMuted").hex());
        assert_eq!(g["600"], t.get("backgroundElement").hex());
        assert_eq!(g["700"], t.get("backgroundPanel").hex());
        assert_eq!(g["800"], t.get("background").hex());
        assert_ne!(g["900"], g["800"]);
    }
    src["theme"]["borderSubtle"] = json!("#ff00ff");
    src["theme"]["border"] = json!("#00ff00");
    src["theme"]["borderActive"] = json!("#00ffff");
    let b = migrate::migrate(&src).unwrap();
    assert_eq!(b["light"]["hue"]["gray"], m["light"]["hue"]["gray"]);
    assert_eq!(b["dark"]["hue"]["gray"], m["dark"]["hue"]["gray"]);
}

#[test]
fn oc_uses_the_base_text_reference_for_primary_actions_on_transparent_backgrounds() {
    let mut src = opencode_v1();
    src["theme"]["background"] = json!("transparent");
    src["theme"]["primary"] = json!({ "light": "#ffffff", "dark": "#000000" });
    src["theme"]
        .as_object_mut()
        .unwrap()
        .remove("selectedListItemText");
    let m = migrate::migrate(&src).unwrap();
    assert_eq!(m["base"]["text"]["action"]["primary"]["base"], "$text.base");
    assert_eq!(m["dark"]["text"]["action"]["primary"]["base"], "$text.base");
}

fn circular_v1() -> Value {
    let mut src = opencode_v1();
    src["defs"]["one"] = json!("two");
    src["defs"]["two"] = json!("one");
    src["theme"]["primary"] = json!("one");
    src
}

#[test]
fn oc_retains_v1_circular_reference_errors() {
    let e = migrate::migrate(&circular_v1()).unwrap_err();
    assert!(
        e.contains("Circular color reference: one -> two -> one"),
        "{e}"
    );
}

#[test]
fn oc_migrates_every_built_in_v1_theme_in_its_supported_modes() {
    for (name, src) in default_themes() {
        let m = migrate::migrate(src).unwrap_or_else(|e| panic!("{name}: {e}"));
        for mode in doc::modes(&m) {
            doc::resolve(&m, Some(mode)).unwrap_or_else(|e| panic!("{name} {mode:?}: {e}"));
        }
    }
}

#[test]
fn oc_collapses_identical_v1_backgrounds_when_both_variants_infer_one_mode() {
    let mut dark = opencode_v1();
    dark["theme"]["background"] = json!("#111111");
    dark["theme"]["text"] = json!("#eeeeee");
    let md = migrate::migrate(&dark).unwrap();
    assert!(md.get("light").is_none() && md.get("dark").is_some());
    assert_eq!(doc::modes(&md), [Mode::Dark]);
    assert_eq!(doc::select(&md, Some(Mode::Light)).unwrap().1, Mode::Dark);
    let mut light = opencode_v1();
    light["theme"]["background"] = json!("#eeeeee");
    light["theme"]["text"] = json!("#111111");
    let ml = migrate::migrate(&light).unwrap();
    assert!(ml.get("light").is_some() && ml.get("dark").is_none());
    assert_eq!(doc::modes(&ml), [Mode::Light]);
    assert_eq!(doc::select(&ml, Some(Mode::Dark)).unwrap().1, Mode::Light);
}

#[test]
fn oc_gives_dialogs_an_opaque_background_when_v1_panels_are_see_through() {
    let lucent = &default_themes()["lucent-orng"];
    for mode in [Mode::Dark, Mode::Light] {
        let legacy = v1(lucent, mode.key());
        let r = doc::resolve(&migrate::migrate(lucent).unwrap(), Some(mode)).unwrap();
        let d = r.surface("dialog");
        assert_eq!(legacy.get("backgroundPanel").a, 0);
        let menu = legacy.get("backgroundMenu");
        assert!(menu.a < 255);
        assert_eq!(ints(&d, d.background.base), [menu.r, menu.g, menu.b, 255]);
    }
    let mut src = opencode_v1();
    for k in [
        "background",
        "backgroundPanel",
        "backgroundElement",
        "backgroundMenu",
    ] {
        src["theme"][k] = json!("transparent");
    }
    let m = migrate::migrate(&src).unwrap();
    let dialog_hex = |mode| {
        let r = doc::resolve(&m, Some(mode)).unwrap();
        let d = r.surface("dialog");
        d.rgba(d.background.base).hex()
    };
    assert_eq!(dialog_hex(Mode::Dark), "#000000");
    assert_eq!(dialog_hex(Mode::Light), "#ffffff");
    let o = doc::resolve(&migrate::migrate(&opencode_v1()).unwrap(), Some(Mode::Dark)).unwrap();
    let d = o.surface("dialog");
    assert_eq!(
        d.rgba(d.background.base).hex(),
        o.rgba(o.background.raised.base).hex()
    );
}

#[test]
fn oc_keeps_both_modes_when_a_shared_background_has_different_contrast() {
    let mut src = opencode_v1();
    src["theme"]["background"] = json!("#808080");
    src["theme"]["text"] = json!({ "light": "#111111", "dark": "#eeeeee" });
    assert_eq!(
        doc::modes(&migrate::migrate(&src).unwrap()),
        [Mode::Light, Mode::Dark]
    );
}

// ----- theme ----------------------------------------------------------------

#[test]
fn oc_rejects_unrecognized_theme_structures() {
    assert!(parse(&json!({}), "theme").is_err());
    assert!(parse(&json!({ "version": 3 }), "theme")
        .unwrap_err()
        .contains("Invalid theme"));
}

#[test]
fn oc_registers_opencode_as_a_native_v2_theme() {
    let t = Themes::new(None, None, None, Default::default());
    assert_eq!(&t.all()["opencode"], opencode());
    assert!(parse(opencode(), "opencode").unwrap().get("base").is_some());
}

#[test]
fn oc_keeps_the_experimental_opencode2_theme_out_of_the_global_theme_list() {
    let t = Themes::new(None, None, None, Default::default());
    assert!(!t.all().contains_key("opencode2"));
    assert!(!t.has("opencode2"));
}

// (adapted) Parsing is pure, so there is no cache to hit: parsing the same
// source again gives the same document.
#[test]
fn oc_detects_v1_themes_from_their_theme_field_and_caches_migrations() {
    let unversioned = opencode_v1();
    let mut explicit = opencode_v1();
    explicit["version"] = json!(1);
    let first = parse(&unversioned, "unversioned").unwrap();
    let second = parse(&explicit, "explicit").unwrap();
    assert!(first.get("base").is_some() && second.get("base").is_some());
    assert_eq!(parse(&unversioned, "unversioned").unwrap(), first);
    assert_eq!(parse(&explicit, "explicit").unwrap(), second);
}

// (adapted) as above: no cache.
#[test]
fn oc_decodes_native_v2_themes_lazily_once() {
    let mut light = opencode()["light"].clone();
    light["categorical"] = json!(["red"]);
    let source = json!({ "base": opencode()["base"], "light": light });
    let d = parse(&source, "theme").unwrap();
    assert_eq!(d["light"]["categorical"], json!(["red"]));
    assert_eq!(parse(&source, "theme").unwrap(), d);
}

#[test]
fn oc_rejects_invalid_v2_themes_when_parsing() {
    let e = parse(&json!({ "light": { "categorical": [] } }), "invalid-v2").unwrap_err();
    assert!(e.contains("Invalid theme: invalid-v2"), "{e}");
}

#[test]
fn oc_rejects_invalid_v1_themes_when_parsing() {
    assert!(parse(&circular_v1(), "theme")
        .unwrap_err()
        .contains("Circular color reference"));
}

// (adapted) no cache: different sources give different documents.
#[test]
fn oc_replacement_sources_receive_independent_parse_caches() {
    let first = opencode_v1();
    let mut second = opencode_v1();
    second["theme"]["primary"] = json!("#123456");
    let a = parse(&first, "t").unwrap();
    let b = parse(&second, "t").unwrap();
    assert_ne!(a, b);
    assert_eq!(parse(&second, "t").unwrap(), b);
}

#[test]
fn oc_has_theme_checks_theme_presence() {
    let t = Themes::new(None, None, None, Default::default());
    assert!(!t.has("missing-theme"));
    assert!(t.has("opencode"));
}

#[test]
fn oc_resolve_theme_rejects_circular_color_refs() {
    assert!(v1::resolve(&circular_v1(), "dark")
        .unwrap_err()
        .contains("Circular color reference"));
}

// (adapted) OpenTUI's color `intent` is not modelled: an ANSI number
// resolves to its RGB value.
#[test]
fn oc_resolve_theme_preserves_full_theme_numeric_color_and_marker_semantics() {
    let mut item = opencode_v1();
    item["theme"]["primary"] = json!(6);
    item["theme"]
        .as_object_mut()
        .unwrap()
        .remove("selectedListItemText");
    let t = v1::resolve(&item, "dark").unwrap();
    assert_eq!(t.get("primary"), Rgba::rgb(0, 128, 128));
    assert_eq!(t.get("selectedListItemText"), t.get("background"));
    assert!(!t.has_selected_list_item_text);
}

fn colors(bg: Option<&str>, palette: &[Option<&str>]) -> TerminalColors {
    TerminalColors {
        palette: palette.iter().map(|c| c.and_then(Rgba::from_hex)).collect(),
        foreground: None,
        background: bg.and_then(Rgba::from_hex),
    }
}

#[test]
fn oc_terminal_mode_derives_mode_from_refreshed_background() {
    assert_eq!(
        system::terminal_mode(&colors(Some("#fbf1c7"), &[])),
        Some(Mode::Light)
    );
    assert_eq!(
        system::terminal_mode(&colors(Some("#1a1b26"), &[])),
        Some(Mode::Dark)
    );
}

#[test]
fn oc_terminal_mode_does_not_derive_mode_from_ansi_slot_zero() {
    assert_eq!(
        system::terminal_mode(&colors(None, &[Some("#000000")])),
        None
    );
}

#[test]
fn oc_custom_theme_precedence_follows_directory_order() {
    let tmp = tempfile::tempdir().unwrap();
    let (global, project) = (tmp.path().join("global"), tmp.path().join("project"));
    for (d, s) in [(&global, "global"), (&project, "project")] {
        std::fs::create_dir_all(d.join("themes")).unwrap();
        std::fs::write(
            d.join("themes/custom.json"),
            json!({ "source": s }).to_string(),
        )
        .unwrap();
    }
    let (found, errors) = discover(&[global, project]);
    assert!(errors.is_empty());
    assert_eq!(
        found.into_iter().collect::<Vec<_>>(),
        [("custom".to_string(), json!({ "source": "project" }))]
    );
}

#[test]
fn theme_directories_are_the_global_folder_then_per_repo_folders_never_the_repository() {
    let tmp = tempfile::tempdir().unwrap();
    let global = tmp.path().join("global");
    let per_repo = global.join("projects/app");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(global.join("themes")).unwrap();
    std::fs::create_dir_all(per_repo.join("themes")).unwrap();
    std::fs::create_dir_all(repo.join(".codesmith/themes")).unwrap();
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::write(global.join("themes/global.json"), r#"{"source":"global"}"#).unwrap();
    std::fs::write(global.join("themes/both.json"), r#"{"source":"global"}"#).unwrap();
    std::fs::write(
        per_repo.join("themes/both.json"),
        r#"{"source":"per-repo"}"#,
    )
    .unwrap();
    std::fs::write(
        repo.join(".codesmith/themes/evil.json"),
        r#"{"source":"repo"}"#,
    )
    .unwrap();
    let dirs = theme_dirs(&global, std::slice::from_ref(&per_repo));
    assert_eq!(dirs, [global.clone(), per_repo.clone()]);
    let (found, _) = discover(&dirs);
    assert_eq!(found["global"], json!({ "source": "global" }));
    assert_eq!(
        found["both"],
        json!({ "source": "per-repo" }),
        "per-repo wins"
    );
    assert!(!found.contains_key("evil"), "a repository's theme was read");
    // The same folder twice is read once.
    assert_eq!(theme_dirs(&global, std::slice::from_ref(&global)), [global]);
}

// ----- theme/v2/select ------------------------------------------------------

#[test]
fn oc_selects_complete_light_and_dark_themes_independently() {
    let light = selected(Mode::Light);
    let dark = selected(Mode::Dark);
    assert_eq!(doc::select(opencode(), None).unwrap().0, light);
    assert_eq!(light["hue"], opencode()["light"]["hue"]);
    assert_eq!(dark["text"], opencode()["base"]["text"]);
    assert_eq!(dark["hue"], opencode()["dark"]["hue"]);
    assert_eq!(
        doc::select(opencode(), Some(Mode::Dark)).unwrap(),
        (dark, Mode::Dark)
    );
}

#[test]
fn oc_selects_the_available_mode_when_the_requested_mode_is_missing() {
    let light_only = json!({ "base": opencode()["base"], "light": opencode()["light"] });
    let dark_only = json!({ "base": opencode()["base"], "dark": opencode()["dark"] });
    assert_eq!(doc::modes(&light_only), [Mode::Light]);
    assert_eq!(doc::modes(&dark_only), [Mode::Dark]);
    assert!(doc::supports(&light_only, Mode::Light));
    assert!(!doc::supports(&light_only, Mode::Dark));
    assert_eq!(
        doc::select(&light_only, Some(Mode::Dark)).unwrap(),
        (selected(Mode::Light), Mode::Light)
    );
    assert_eq!(
        doc::select(&dark_only, Some(Mode::Light)).unwrap(),
        (selected(Mode::Dark), Mode::Dark)
    );
}

// ----- theme/v2/component ---------------------------------------------------

// (adapted) No Solid signals: the operations are checked on the light
// theme and the dialog surface on the dark one.
#[test]
fn oc_provides_reactive_properties_states_surfaces_and_color_operations() {
    let t = doc::resolve_definition(&selected(Mode::Light)).unwrap();
    let raised = t.background.raised.base;
    assert_eq!(t.increase(raised, 1), t.step("neutral", 800));
    assert_eq!(t.decrease(raised, 1), t.step("neutral", 600));
    assert_eq!(t.decrease(t.step("accent", 300), 2), t.step("accent", 100));
    assert_eq!(t.increase(t.step("accent", 900), 3), t.step("accent", 900));
    assert_eq!(t.decrease(t.step("accent", 100), 3), t.step("accent", 100));
    assert_eq!(t.source(raised), Some(("neutral", 700)));
    // A color that is not one of the theme's steps.
    let outside = u32::MAX;
    assert_eq!(t.source(outside), None);
    assert_eq!(t.increase(outside, 1), outside);
    let a = t.background.action[0];
    let all = doc::States {
        disabled: true,
        pressed: true,
        focused: true,
        selected: true,
        hovered: true,
    };
    assert_eq!(a.state(all), a.disabled);
    assert_eq!(
        a.state(doc::States {
            disabled: false,
            ..all
        }),
        a.pressed
    );
    assert_eq!(
        a.state(doc::States {
            disabled: false,
            pressed: false,
            ..all
        }),
        a.focused
    );
    assert_eq!(
        a.state(doc::States {
            selected: true,
            hovered: true,
            ..Default::default()
        }),
        a.selected
    );
    assert_eq!(
        a.state(doc::States {
            hovered: true,
            ..Default::default()
        }),
        a.hovered
    );
    assert_eq!(a.state(doc::States::default()), a.base);
    assert_eq!(
        t.text.formfield.state(doc::States {
            focused: true,
            selected: true,
            ..Default::default()
        }),
        t.text.formfield.focused
    );
    let current = t.surface("dialog");
    assert!(t.surface("dialog").same(&current));
    assert!(current.surface("dialog").same(&current));
    assert_eq!(current.categorical(), t.categorical());
    assert_eq!(current.background.base, t.background.raised.base);
    assert_eq!(
        current.background.action[0].hovered,
        t.background.raised.high
    );
    let d = doc::resolve_definition(&selected(Mode::Dark)).unwrap();
    let dc = d.surface("dialog");
    assert_eq!(dc.background.base, d.background.raised.base);
    assert_eq!(
        dc.decrease(dc.background.raised.base, 1),
        d.step("neutral", 600)
    );
}

// ----- cli/tui/theme-mode ---------------------------------------------------

#[test]
fn oc_uses_an_available_mode_while_retaining_the_pinned_preference() {
    let base = opencode_v1();
    let mut light_only = base.clone();
    light_only["theme"]["background"] = json!("#eeeeee");
    light_only["theme"]["text"] = json!("#111111");
    let mut dual = base.clone();
    dual["theme"]["background"] = json!({ "light": "#eeeeee", "dark": "#111111" });
    dual["theme"]["text"] = json!({ "light": "#111111", "dark": "#eeeeee" });
    let mut dark_only = base.clone();
    dark_only["theme"]["background"] = json!("#111111");
    dark_only["theme"]["text"] = json!("#eeeeee");
    let mut native_base = opencode()["base"].clone();
    native_base["text"]["base"] = json!("#abcdef");
    let native = json!({ "base": native_base, "dark": { "hue": opencode()["dark"]["hue"] } });
    let custom = [
        ("light-only", light_only),
        ("dark-only", dark_only),
        ("dual", dual),
        ("native", native),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    let mut t = Themes::new(Some("light-only"), Some(ModeSetting::Dark), None, custom);
    assert_eq!(t.mode(), Mode::Light);
    assert_eq!(t.modes(), [Mode::Light]);
    assert!(!t.supports(Mode::Dark));
    assert!(!t.set_mode(Mode::Dark));
    assert!(t.set("dark-only"));
    assert_eq!(t.mode(), Mode::Dark);
    assert_eq!(t.modes(), [Mode::Dark]);
    assert!(t.set("light-only"));
    assert_eq!(t.mode(), Mode::Light);
    assert!(t.set("dual"));
    assert_eq!(t.mode(), Mode::Dark);
    assert_eq!(t.modes(), [Mode::Light, Mode::Dark]);
    assert!(t.set("native"));
    assert_eq!(t.selected(), "native");
    assert_eq!(t.modes(), [Mode::Dark]);
    let c = &t.current().theme;
    assert_eq!(c.rgba(c.text.base), Rgba::from_hex("#abcdef").unwrap());
}

fn falls_back(source: Value) {
    let custom = [("invalid".to_string(), source)].into_iter().collect();
    let t = Themes::new(Some("invalid"), None, Some(Mode::Dark), custom);
    assert_eq!(t.selected(), "opencode");
    let (name, error) = &t.errors[0];
    assert_eq!(name, "invalid");
    assert!(!error.is_empty());
}

#[test]
fn oc_falls_back_to_opencode_when_configured_v2_theme_schema_is_invalid() {
    falls_back(json!({ "base": {}, "light": {} }));
}

#[test]
fn oc_falls_back_to_opencode_when_configured_v2_theme_partial_mode_is_invalid() {
    falls_back(json!({ "base": { "text": { "base": "#ffffff" } }, "light": {} }));
}

#[test]
fn oc_falls_back_to_opencode_when_configured_v2_theme_token_reference_is_invalid() {
    let mut base = opencode()["base"].clone();
    base["text"]["base"] = json!("$missing");
    falls_back(json!({ "base": base, "light": opencode()["light"] }));
}

#[test]
fn oc_dialog_surfaces_are_absolute_and_can_be_inherited_through_the_theme_context() {
    let t = Themes::new(
        Some("opencode"),
        Some(ModeSetting::Dark),
        None,
        Default::default(),
    );
    let theme = &t.current().theme;
    let dialog = theme.surface("dialog");
    assert!(theme.surface("dialog").same(&dialog));
    assert!(dialog.surface("dialog").same(&dialog));
    assert_eq!(dialog.background.base, theme.background.raised.base);
    assert_eq!(
        dialog.decrease(dialog.background.raised.base, 1),
        theme.step("neutral", 600)
    );
}

// ----- context/theme ---------------------------------------------------------

// (adapted) The palette reply is applied as it arrives; there is no
// renderer event.
#[test]
fn oc_the_system_theme_follows_a_palette_reply_that_arrives_after_the_query() {
    let mut t = Themes::new(Some("system"), None, Some(Mode::Dark), Default::default());
    let reply = |fg: &str| TerminalColors {
        palette: vec![],
        foreground: Rgba::from_hex(fg),
        background: Rgba::from_hex("#000000"),
    };
    t.set_palette(reply("#aaaaaa"));
    let c = &t.current().theme;
    assert_eq!(c.rgba(c.text.base), Rgba::from_hex("#aaaaaa").unwrap());
    t.set_palette(reply("#123456"));
    let c = &t.current().theme;
    assert_eq!(c.rgba(c.text.base), Rgba::from_hex("#123456").unwrap());
}

// Beyond OpenCode's tests: every built-in theme makes a palette in both
// modes, and the system theme keeps the terminal's own background.
#[test]
fn every_theme_makes_a_palette() {
    let mut t = Themes::new(None, None, None, Default::default());
    for name in t.names() {
        assert!(t.set(&name), "{name}");
        assert!(t.errors.is_empty(), "{name}: {:?}", t.errors);
        let _ = t.palette().syntect();
    }
    t.set_palette(TerminalColors {
        palette: vec![],
        foreground: Rgba::from_hex("#cccccc"),
        background: Rgba::from_hex("#101010"),
    });
    assert!(t.set("system"));
    assert_eq!(t.palette().bg, ratatui::style::Color::Reset);
    let l = rgb_to_oklch(1.0, 1.0, 1.0).l;
    assert!((l - 1.0).abs() < 1e-3);
}
