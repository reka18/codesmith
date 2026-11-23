use super::*;
use crate::tui::models::Model;
use serde_json::json;

// Provider ids no real data folder has, so a kept live list never leaks in.
const P: &str = "zz-pick-test";
const Q: &str = "zz-pick-other";

fn cfg() -> Config {
    serde_json::from_value(json!({"providers": {
        P: {"name": "Pick Test", "models": {
            "alpha": {"name": "Alpha", "limit": {"context": 128000, "output": 8000}},
            "beta": {}
        }},
        Q: {"name": "Other", "models": {"gamma": {}}}
    }}))
    .unwrap()
}

fn choice(id: &str, name: &str, state: State) -> Choice {
    Choice {
        id: id.into(),
        name: name.into(),
        state,
        methods: vec![],
        key_page: None,
        note: None,
    }
}

// ------------------------------------------------------------ provider_of

#[test]
fn provider_of_takes_the_part_before_the_first_slash() {
    assert_eq!(provider_of("anthropic/claude-opus-4-5"), "anthropic");
    assert_eq!(provider_of("openrouter/anthropic/claude"), "openrouter");
    assert_eq!(provider_of("lmstudio/qwen#high"), "lmstudio");
    assert_eq!(provider_of(" groq /llama"), "groq");
    assert_eq!(provider_of("no-slash"), "");
    assert_eq!(provider_of(""), "");
}

// ------------------------------------------------------------ model picker

#[test]
fn the_model_picker_lists_only_the_current_providers_models() {
    let items = model_items(&cfg(), &format!("{P}/alpha"), P);
    let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
    assert!(ids.contains(&"zz-pick-test/alpha"), "{ids:?}");
    assert!(ids.contains(&"zz-pick-test/beta"), "{ids:?}");
    assert!(
        !ids.iter().any(|i| i.starts_with(Q)),
        "no other provider's models: {ids:?}"
    );
}

#[test]
fn the_model_picker_marks_the_current_model_through_its_variant() {
    let items = model_items(&cfg(), &format!("{P}/alpha#high"), P);
    let current: Vec<&str> = items
        .iter()
        .filter(|i| i.current)
        .map(|i| i.id.as_str())
        .collect();
    assert_eq!(current, ["zz-pick-test/alpha"]);
}

#[test]
fn the_model_picker_shows_names_and_context() {
    let items = model_items(&cfg(), "", P);
    let alpha = items.iter().find(|i| i.id.ends_with("/alpha")).unwrap();
    assert_eq!(alpha.detail, "Alpha · 128K context");
    let beta = items.iter().find(|i| i.id.ends_with("/beta")).unwrap();
    assert!(!beta.detail.contains("context"), "{}", beta.detail);
}

#[test]
fn a_current_model_the_list_lacks_stays_choosable() {
    let items = model_items(&cfg(), &format!("{P}/retired-model"), P);
    assert_eq!(items[0].id, "zz-pick-test/retired-model");
    assert!(items[0].current);
    assert_eq!(items[0].detail, "current");
}

#[test]
fn another_providers_current_model_is_not_added() {
    let items = model_items(&cfg(), &format!("{Q}/gamma"), P);
    assert!(items.iter().all(|i| !i.current));
    assert!(items.iter().all(|i| provider_of(&i.id) == P));
}

#[test]
fn an_unknown_provider_lists_nothing() {
    assert!(model_items(&cfg(), "", "zz-no-such-provider").is_empty());
}

#[test]
fn the_switch_row_opens_the_provider_picker() {
    let row = other_providers_item("anthropic", "Anthropic");
    assert_eq!(row.id, OTHER_PROVIDERS);
    assert!(row.detail.contains("Anthropic"));
    assert!(!row.current);
}

// ------------------------------------------------------------ provider list

#[test]
fn providers_carry_their_state_and_skip_disabled_ones() {
    let mut c = cfg();
    c.disabled_providers = vec!["zz-off".into()];
    let choices = vec![
        choice(P, "Pick Test", State::Config),
        choice(Q, "Other", State::None),
        choice("zz-off", "Off", State::Config),
    ];
    let list = providers(&c, &choices);
    let p = list.iter().find(|x| x.id == P).unwrap();
    assert_eq!(p.status, Status::Ready("configured".into()));
    assert_eq!(p.models, 2);
    let q = list.iter().find(|x| x.id == Q).unwrap();
    assert_eq!(q.status, Status::SignIn);
    assert_eq!(
        q.models, 0,
        "no models are counted for a provider that is not ready"
    );
    assert!(list.iter().all(|x| x.id != "zz-off"));
}

#[test]
fn a_configured_provider_missing_from_the_sign_in_list_is_ready() {
    // P is configured but absent from the choices (it needs no key).
    let list = providers(&cfg(), &[choice(Q, "Other", State::None)]);
    let p = list
        .iter()
        .find(|x| x.id == P)
        .expect("configured provider listed");
    assert_eq!(p.status, Status::Ready("configured".into()));
    assert_eq!(p.models, 2);
    assert_eq!(
        list.iter().filter(|x| x.id == Q).count(),
        1,
        "no duplicates"
    );
}

fn prov(id: &str, name: &str, status: Status, models: usize) -> Provider {
    Provider {
        id: id.into(),
        name: name.into(),
        status,
        models,
    }
}

#[test]
fn provider_rows_put_the_current_then_ready_then_local_then_sign_in() {
    let list = vec![
        prov("needs", "Needs Key", Status::SignIn, 0),
        prov("lm", "LM Studio", Status::Local(3), 3),
        prov("off", "Ollama", Status::Local(0), 0),
        prov("b", "Beta Cloud", Status::Ready("$B_KEY".into()), 12),
        prov("a", "Alpha Cloud", Status::Ready(String::new()), 1),
        prov("cur", "Current", Status::SignIn, 0),
    ];
    let items = provider_items(&list, "cur");
    let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(ids, ["cur", "a", "b", "lm", "off", "needs"]);
    assert!(items[0].current);
    let detail = |id: &str| items.iter().find(|i| i.id == id).unwrap().detail.clone();
    assert_eq!(detail("a"), "ready · 1 model · untested");
    assert_eq!(detail("b"), "ready · $B_KEY · 12 models · untested");
    assert_eq!(detail("lm"), "local · 3 models · untested");
    assert_eq!(detail("off"), "local · not answering · untested");
    assert_eq!(detail("needs"), "sign in · untested");
}

#[test]
fn only_the_tested_providers_carry_no_untested_tag() {
    let list = vec![
        prov("anthropic", "Anthropic", Status::Ready(String::new()), 3),
        prov(
            "claude-agent",
            "Claude Code",
            Status::Ready(String::new()),
            3,
        ),
        prov("fireworks", "Fireworks", Status::SignIn, 0),
        prov("groq", "Groq", Status::Ready(String::new()), 3),
        prov("openai", "OpenAI", Status::SignIn, 0),
        prov("ollama", "Ollama", Status::Local(2), 2),
        prov("my-endpoint", "Mine", Status::Ready("configured".into()), 1),
    ];
    for item in provider_items(&list, "anthropic") {
        let tested = ["anthropic", "claude-agent", "fireworks"].contains(&item.id.as_str());
        assert_eq!(
            !item.detail.ends_with(" · untested"),
            tested,
            "{}: {}",
            item.id,
            item.detail
        );
        assert_eq!(
            item.detail.matches("untested").count(),
            usize::from(!tested)
        );
    }
}

#[test]
fn provider_rows_with_no_providers_are_empty() {
    assert!(provider_items(&[], "anything").is_empty());
}

// ------------------------------------------------------------ choosing

#[test]
fn choosing_a_provider_returns_to_its_model_used_last() {
    let recent = vec![
        Model::new(Q, "gamma"),
        Model::new(P, "beta"),
        Model::new(P, "alpha"),
    ];
    assert_eq!(
        model_for(&cfg(), P, &recent).as_deref(),
        Some("zz-pick-test/beta")
    );
}

#[test]
fn a_recent_model_the_provider_no_longer_lists_is_passed_over() {
    let recent = vec![Model::new(P, "retired")];
    let got = model_for(&cfg(), P, &recent).unwrap();
    assert!(
        got == "zz-pick-test/alpha" || got == "zz-pick-test/beta",
        "{got}"
    );
}

#[test]
fn a_provider_without_models_gives_none() {
    assert_eq!(model_for(&cfg(), "zz-no-such-provider", &[]), None);
}

// ------------------------------------------------------------ sign-in focus

#[test]
fn the_sign_in_picker_opens_on_the_chosen_provider() {
    use crate::tui::login_view::{LoginView, Stage};
    use codesmith_core::providers::signin::{Kind, Method};
    let mut with_methods = choice("anthropic", "Anthropic", State::None);
    with_methods.methods = vec![Method {
        kind: Kind::Key,
        label: "API key".into(),
        fields: vec![],
    }];
    let choices = vec![choice("first", "First", State::None), with_methods];
    let mut v = LoginView::new(choices.clone(), Default::default());
    assert!(v.focus("anthropic"));
    assert!(
        matches!(v.stage, Stage::Methods { choice: 1, .. }),
        "{:?}",
        v.stage
    );
    // A provider with nothing to sign in with is selected, not opened.
    let mut v = LoginView::new(choices, Default::default());
    assert!(v.focus("first"));
    assert_eq!(v.sel, 0);
    assert!(matches!(v.stage, Stage::List));
    assert!(!v.focus("zz-missing"));
}

// ------------------------------------------------------------ stress

#[test]
fn a_provider_with_thousands_of_models_lists_quickly() {
    let models: serde_json::Map<String, Value> = (0..3000)
        .map(|i| (format!("m{i:04}"), json!({"limit": {"context": 1000 + i}})))
        .collect();
    let c: Config =
        serde_json::from_value(json!({"providers": {P: {"name": "Big", "models": models}}}))
            .unwrap();
    let started = std::time::Instant::now();
    let items = model_items(&c, &format!("{P}/m0042"), P);
    assert_eq!(items.len(), 3000);
    assert_eq!(items.iter().filter(|i| i.current).count(), 1);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "took {:?}",
        started.elapsed()
    );
}

use serde_json::Value;
