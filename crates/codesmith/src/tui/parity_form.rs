//! OpenCode's question form tests, ported: `tui/test/util/form` (the field
//! helpers) and `tui/test/cli/tui/form` (the form). OpenCode's form takes
//! the composer's place; CodeSmith's is the question dialog, drawn over
//! the session, so "the composer is restored" means the dialog closed.

use super::app::{App, Modal, Pick, PickItem, Picker};
use super::bridge::Ev;
use super::form::{self, Outcome};
use codesmith_core::form::{Answer, Field, Info};
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use serde_json::{json, Value};
use tokio::sync::oneshot::{self, Receiver};

fn fields(v: Value) -> Vec<Field> {
    serde_json::from_value(v).unwrap()
}

fn field(v: Value) -> Field {
    serde_json::from_value(v).unwrap()
}

// ----- tui/test/util/form ---------------------------------------------------

fn option() -> Value {
    json!({ "key": "choice", "type": "string", "options": [{ "value": "one", "label": "One" }], "custom": true })
}

fn selection() -> Field {
    field(json!({
        "key": "tags", "type": "multiselect", "custom": true,
        "options": [{ "value": "one", "label": "One" }, { "value": "two", "label": "Two" }],
    }))
}

fn with(base: Value, extra: Value) -> Value {
    let mut b = base;
    for (k, v) in extra.as_object().unwrap() {
        b[k] = v.clone();
    }
    b
}

// form.test.ts: "initializes configured and custom defaults"
#[test]
fn oc_initializes_configured_and_custom_defaults() {
    let (answers, custom) = form::initial_values(&fields(json!([
        { "key": "mode", "type": "string", "options": [{ "value": "fast", "label": "Fast" }], "default": "fast" },
        with(option(), json!({ "key": "note", "default": "detailed" })),
        with(option(), json!({ "key": "configured", "default": "one" })),
        { "key": "count", "type": "number", "default": 0 },
        { "key": "authorize", "type": "external", "url": "https://example.com" },
    ])));
    assert_eq!(
        serde_json::to_value(&answers).unwrap(),
        json!({ "mode": "fast", "note": "detailed", "configured": "one", "count": 0.0 })
    );
    assert_eq!(
        serde_json::to_value(&custom).unwrap(),
        json!({ "note": "detailed" })
    );
}

// form.test.ts: "validates every supported field constraint"
#[test]
fn oc_validates_every_supported_field_constraint() {
    let check = |f: Value, v: Option<Value>, want: Option<&str>| {
        assert_eq!(
            form::validate(&field(f.clone()), v.as_ref()).as_deref(),
            want,
            "{f} {v:?}"
        );
    };
    let s = |extra: Value| with(json!({ "key": "value", "type": "string" }), extra);
    let m = |extra: Value| {
        with(
            json!({ "key": "value", "type": "multiselect", "options": [] }),
            extra,
        )
    };
    check(
        s(json!({ "required": true })),
        None,
        Some("Answer required"),
    );
    check(
        m(json!({ "required": true })),
        Some(json!([])),
        Some("Select at least one option"),
    );
    check(s(json!({})), Some(json!(true)), Some("Expected text"));
    check(
        s(json!({ "minLength": 3 })),
        Some(json!("ab")),
        Some("Must be at least 3 characters"),
    );
    check(
        s(json!({ "maxLength": 2 })),
        Some(json!("abc")),
        Some("Must be at most 2 characters"),
    );
    check(
        s(json!({ "pattern": "^a+$" })),
        Some(json!("bbb")),
        Some("Must match pattern: ^a+$"),
    );
    check(
        s(json!({ "pattern": "[" })),
        Some(json!("value")),
        Some("Invalid pattern: ["),
    );
    check(
        s(json!({ "format": "email" })),
        Some(json!("invalid")),
        Some("Expected an email address"),
    );
    check(
        s(json!({ "format": "uri" })),
        Some(json!("not a URL")),
        Some("Expected a URL"),
    );
    check(
        s(json!({ "format": "date" })),
        Some(json!("2025-02-29")),
        Some("Expected a date (YYYY-MM-DD)"),
    );
    check(
        s(json!({ "format": "date-time" })),
        Some(json!("not a date")),
        Some("Expected a date and time"),
    );
    check(
        s(json!({ "options": [{ "value": "yes", "label": "Yes" }] })),
        Some(json!("no")),
        Some("Select an available option"),
    );
    // (adapted) JSON has no NaN: a non-number stands in for it.
    check(
        json!({ "key": "value", "type": "number" }),
        Some(json!("NaN")),
        Some("Expected a number"),
    );
    check(
        json!({ "key": "value", "type": "integer" }),
        Some(json!(1.5)),
        Some("Expected an integer"),
    );
    check(
        json!({ "key": "value", "type": "number", "minimum": 2 }),
        Some(json!(1)),
        Some("Must be at least 2"),
    );
    check(
        json!({ "key": "value", "type": "number", "maximum": 2 }),
        Some(json!(3)),
        Some("Must be at most 2"),
    );
    check(
        json!({ "key": "value", "type": "boolean" }),
        Some(json!("yes")),
        Some("Expected yes or no"),
    );
    check(
        m(json!({})),
        Some(json!("yes")),
        Some("Expected selections"),
    );
    check(
        m(json!({ "minItems": 2 })),
        Some(json!(["one"])),
        Some("Select at least 2"),
    );
    check(
        m(json!({ "maxItems": 1 })),
        Some(json!(["one", "two"])),
        Some("Select at most 1"),
    );
    check(
        m(json!({ "options": [{ "value": "one", "label": "One" }] })),
        Some(json!(["two"])),
        Some("Select only available options"),
    );
    check(m(json!({ "custom": true })), Some(json!(["custom"])), None);
}

// form.test.ts: "shares field classification, rows, selection, and display"
#[test]
fn oc_shares_field_classification_rows_selection_and_display() {
    let text = field(json!({ "key": "name", "type": "string", "title": "Name" }));
    let external =
        field(json!({ "key": "authorize", "type": "external", "url": "https://example.com" }));
    let option = field(option());
    assert_eq!(
        [text.base().is_some(), external.base().is_some()],
        [true, false]
    );
    assert_eq!(
        [form::label(&text), form::label(&external)],
        ["Name", "https://example.com"]
    );
    assert_eq!(
        [
            form::textual(Some(&text)),
            form::textual(Some(&option)),
            form::custom(Some(&option))
        ],
        [true, false, true]
    );
    let labels = |f: Value| {
        form::rows(Some(&field(f)))
            .into_iter()
            .map(|r| (r.value, r.label, r.description))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        labels(json!({ "key": "value", "type": "boolean" })),
        [
            (json!(true), "Yes".to_string(), None),
            (json!(false), "No".to_string(), None)
        ]
    );
    assert_eq!(
        labels(with(
            self::option(),
            json!({ "options": [{ "value": "one", "label": "One", "description": "First" }] })
        )),
        [(json!("one"), "One".to_string(), Some("First".to_string()))]
    );
    assert!(labels(json!({ "key": "value", "type": "number" })).is_empty());
    let sel = selection();
    assert_eq!(
        [
            form::selected(Some(&sel), Some(&json!("two"))),
            form::selected(Some(&sel), Some(&json!("custom"))),
            form::selected(Some(&sel), None),
        ],
        [1, 2, 0]
    );
    assert_eq!(
        form::display(&sel, Some(&json!(["one", "custom"])), "(none)"),
        "One, custom"
    );
    assert_eq!(
        [
            form::display(&sel, Some(&json!([])), ""),
            form::display(&sel, Some(&json!([])), "(none)")
        ],
        ["", "(none)"]
    );
}

// form.test.ts: "updates multiselects without mutating their source"
#[test]
fn oc_updates_multiselects_without_mutating_their_source() {
    let source = json!(["one", "custom"]);
    assert_eq!(form::toggle(Some(&source), "one"), ["custom"]);
    assert_eq!(form::toggle(Some(&source), "two"), ["one", "custom", "two"]);
    assert_eq!(
        form::set_custom(Some(&source), Some("custom"), "replacement"),
        ["one", "replacement"]
    );
    assert_eq!(source, json!(["one", "custom"]));
}

// ----- tui/test/cli/tui/form ------------------------------------------------

struct T {
    app: App,
    rx: Receiver<Option<Answer>>,
    w: u16,
    h: u16,
}

fn info(fields: Value) -> Info {
    Info {
        id: "frm_test".into(),
        session_id: "ses_test".into(),
        title: "Authorization required".into(),
        metadata: None,
        fields: self::fields(fields),
    }
}

fn mount_at(fields: Value, w: u16, h: u16) -> T {
    let mut app = App::new();
    let (tx, rx) = oneshot::channel();
    app.on_event(Ev::Form(info(fields), tx));
    let t = T { app, rx, w, h };
    assert!(t.frame().contains("Authorization required"));
    t
}

fn mount(fields: Value) -> T {
    mount_at(fields, 80, 20)
}

fn authorize() -> Value {
    json!([{
        "key": "authorization", "type": "external",
        "url": "https://example.com/authorize", "title": "Authorize access",
    }])
}

impl T {
    fn frame(&self) -> String {
        let mut term = Terminal::new(TestBackend::new(self.w, self.h)).unwrap();
        term.draw(|f| super::view::draw(f, &self.app)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..self.h)
            .map(|y| {
                (0..self.w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    fn press(&mut self, code: KeyCode) -> Option<super::app::Action> {
        self.app.on_key(KeyEvent::new(code, KeyModifiers::NONE))
    }
    fn ctrl(&mut self, c: char) {
        self.app
            .on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
    }
    fn typ(&mut self, s: &str) {
        for c in s.chars() {
            self.press(KeyCode::Char(c));
        }
    }
    fn form(&self) -> &form::Form {
        match &self.app.modal {
            Some(Modal::Question { form, .. }) => form,
            _ => panic!("no form open"),
        }
    }
    fn editing(&self) -> bool {
        self.form().editing
    }
    fn editor(&self) -> String {
        self.form().editor.text().to_string()
    }
    fn reply(&mut self) -> Option<Option<Answer>> {
        self.rx.try_recv().ok()
    }
    fn replied(&mut self) -> Option<Value> {
        self.reply()
            .flatten()
            .map(|a| serde_json::to_value(a).unwrap())
    }
    fn closed(&self) -> bool {
        self.app.modal.is_none()
    }
}

fn staging(kind: &str, extra: Value) -> Value {
    json!([with(
        json!({ "key": if kind == "string" { "target" } else { "targets" }, "type": kind, "options": [{ "value": "staging", "label": "Staging" }] }),
        extra
    )])
}

// (adapted) OpenCode's form calls fail with 404 or 409 when the form was
// settled elsewhere, and a refresh may fail too; here the turn that asked
// went away (its reply channel closed). Either way the dialog closes
// without showing an error.
fn recovering() -> T {
    let mut t = mount(staging("string", json!({})));
    let (_, rx) = oneshot::channel();
    t.rx = rx;
    t
}

// form.test.tsx: "restores the composer when terminal-form revalidation fails"
#[test]
fn oc_restores_the_composer_when_terminal_form_revalidation_fails() {
    let mut t = recovering();
    t.press(KeyCode::Enter);
    assert!(t.closed());
    assert!(!t.frame().contains("Form not found"));
}

// form.test.tsx: "restores the composer after a terminal ${status} reply" (2 cases)
#[test]
fn oc_restores_the_composer_after_a_terminal_reply() {
    for _status in [404, 409] {
        let mut t = recovering();
        t.press(KeyCode::Enter);
        assert!(t.closed());
        let f = t.frame();
        assert!(!f.contains("Form not found") && !f.contains("Form already settled"));
    }
}

// form.test.tsx: "restores the composer after a terminal ${status} cancellation" (2 cases)
#[test]
fn oc_restores_the_composer_after_a_terminal_cancellation() {
    for _status in [404, 409] {
        let mut t = recovering();
        t.press(KeyCode::Esc);
        assert!(t.closed());
        let f = t.frame();
        assert!(!f.contains("Form not found") && !f.contains("Form already settled"));
    }
}

// form.test.tsx: "requires explicit acknowledgement before submitting an external field"
#[test]
fn oc_requires_explicit_acknowledgement_before_submitting_an_external_field() {
    let mut t = mount(authorize());
    t.press(KeyCode::Right);
    assert!(t.frame().contains("(acknowledgement required)"));
    t.press(KeyCode::Enter);
    assert!(t.frame().contains("External action must be acknowledged"));
    assert_eq!(t.reply(), None);

    t.press(KeyCode::Left);
    t.press(KeyCode::Char('c'));
    assert!(t.frame().contains("press enter to confirm"));
    assert_eq!(
        t.app.clipboard.as_deref(),
        Some("https://example.com/authorize")
    );
    assert_eq!(t.reply(), None);

    t.press(KeyCode::Enter);
    assert!(t.frame().contains("Acknowledged"));
    assert_eq!(t.reply(), None);

    t.press(KeyCode::Enter);
    assert_eq!(t.replied(), Some(json!({ "authorization": true })));
}

// (not in OpenCode's test) Enter on an unopened link opens it first.
#[test]
fn enter_opens_an_external_link_before_confirming_it() {
    let mut t = mount(authorize());
    assert_eq!(
        t.press(KeyCode::Enter),
        Some(super::app::Action::OpenUrl(
            "https://example.com/authorize".into()
        ))
    );
    assert!(t.frame().contains("press enter to confirm"));
}

// form.test.tsx: "includes external acknowledgements in progress"
#[test]
fn oc_includes_external_acknowledgements_in_progress() {
    let mut t = mount_at(authorize(), 32, 20);
    assert!(t.frame().contains("0/1"), "{}", t.frame());
    assert_eq!(t.reply(), None);
}

// form.test.tsx: "shows a compact confirmation summary"
#[test]
fn oc_shows_a_compact_confirmation_summary() {
    let mut t = mount(staging("multiselect", json!({ "default": ["staging"] })));
    t.press(KeyCode::Right);
    let f = t.frame();
    assert!(f.contains("enter submit"), "{f}");
    assert!(!f.contains("Review") && !f.contains("Submit answers"));
    t.press(KeyCode::Enter);
    assert_eq!(t.replied(), Some(json!({ "targets": ["staging"] })));
}

// form.test.tsx: "confirmation fits wrapped answers before offering scroll"
#[test]
fn oc_confirmation_fits_wrapped_answers_before_offering_scroll() {
    let long = "a very long selected deliverable that wraps";
    let mut t = mount_at(
        json!([
            { "key": "targets", "title": "Include", "type": "multiselect",
              "options": [{ "value": long, "label": "A very long selected deliverable that wraps" }],
              "default": [long] },
            { "key": "priority", "title": "Priority", "type": "multiselect",
              "options": [{ "value": "now", "label": "Now" }], "default": ["now"] },
        ]),
        48,
        40,
    );
    assert!(t.frame().contains("Include  Priority  Submit"));
    t.press(KeyCode::Right);
    t.press(KeyCode::Right);
    let f = t.frame();
    assert!(
        f.contains("Priority: Now") && f.contains("enter submit"),
        "{f}"
    );
    assert!(!f.contains("↑↓ scroll"));
    // (adapted) the dialog is centred: the review and its keys fit in it
    // without scrolling, right below the answers.
    let lines: Vec<&str> = f.lines().collect();
    let at = lines
        .iter()
        .position(|l| l.contains("enter submit"))
        .unwrap();
    let first = lines.iter().position(|l| l.contains("Include: ")).unwrap();
    assert!(at - first <= 5, "{f}");
}

fn custom_choice() -> T {
    mount(staging("string", json!({ "custom": true })))
}

// form.test.tsx: "pasting on a custom choice opens its editor without submitting"
#[test]
fn oc_pasting_on_a_custom_choice_opens_its_editor_without_submitting() {
    let mut t = custom_choice();
    t.app.on_paste("production\nwest");
    assert!(t.editing());
    assert_eq!(t.editor(), "production\nwest");
    let f = t.frame();
    assert!(f.contains("production") && !f.contains("Type your own answer"));
    assert_eq!(t.reply(), None);
}

// form.test.tsx: "pasting in an active custom editor inserts at the cursor"
#[test]
fn oc_pasting_in_an_active_custom_editor_inserts_at_the_cursor() {
    let mut t = custom_choice();
    t.press(KeyCode::Down);
    t.press(KeyCode::Enter);
    assert!(t.editing());
    t.typ("prodwest");
    for _ in 0..4 {
        t.press(KeyCode::Left);
    }
    t.app.on_paste("uction ");
    assert_eq!(t.editor(), "production west");
    t.typ("!");
    assert_eq!(t.editor(), "production !west");
    assert_eq!(t.reply(), None);
}

// form.test.tsx: "clipboard shortcut opens a custom choice editor without submitting"
// (adapted) The terminal pastes on ctrl+v (`prompt.paste` lets the key
// through): its bracketed paste opens the editor.
#[test]
fn oc_clipboard_shortcut_opens_a_custom_choice_editor_without_submitting() {
    let mut t = custom_choice();
    t.ctrl('v');
    assert!(!t.editing());
    t.app.on_paste("production\nwest");
    assert_eq!(t.editor(), "production\nwest");
    assert!(!t.frame().contains("Type your own answer"));
    assert_eq!(t.reply(), None);
}

fn custom_multi(extra: Value) -> T {
    mount(staging(
        "multiselect",
        with(json!({ "custom": true }), extra),
    ))
}

// form.test.tsx: "typing a custom multiselect answer selects it before commit"
#[test]
fn oc_typing_a_custom_multiselect_answer_selects_it_before_commit() {
    let mut t = custom_multi(json!({ "minItems": 1 }));
    t.press(KeyCode::Down);
    t.press(KeyCode::Enter);
    assert!(t.editing());
    t.typ("production");
    assert!(t.frame().contains("[✓] production"));
    assert_eq!(t.reply(), None);
}

// form.test.tsx: "up leaves a custom editor only from its first visual line"
#[test]
fn oc_up_leaves_a_custom_editor_only_from_its_first_visual_line() {
    let mut t = custom_multi(json!({}));
    t.press(KeyCode::Down);
    t.press(KeyCode::Enter);
    t.app.on_paste("production\nwest");
    t.press(KeyCode::Up);
    assert!(t.editing());
    t.press(KeyCode::Up);
    assert!(!t.editing());
    t.press(KeyCode::Enter);
    assert!(t.frame().contains("[✓] Staging"));
}

// form.test.tsx: "typing on the highlighted custom option opens it without losing burst input"
#[test]
fn oc_typing_on_the_highlighted_custom_option_opens_it_without_losing_burst_input() {
    let mut t = custom_multi(json!({}));
    t.press(KeyCode::Down);
    t.typ("production target");
    assert_eq!(t.editor(), "production target");
    assert!(t.frame().contains("[✓] production target"));
    assert_eq!(t.reply(), None);
}

// form.test.tsx: "enter on an empty custom multiselect option clearly enters editing"
#[test]
fn oc_enter_on_an_empty_custom_multiselect_option_clearly_enters_editing() {
    let mut t = custom_multi(json!({ "minItems": 1 }));
    t.press(KeyCode::Down);
    assert!(t.frame().contains("enter edit"));
    t.press(KeyCode::Enter);
    assert!(t.editing());
    let f = t.frame();
    assert!(
        f.contains("[✓] Type your own answer") && f.contains("enter done"),
        "{f}"
    );
    assert!(f.contains("esc close") && !f.contains("↑↓ select"));

    t.press(KeyCode::Esc);
    assert!(!t.editing());
    assert!(t.frame().contains("[ ] Type your own answer"));

    t.press(KeyCode::Enter);
    assert!(t.editing());
    t.press(KeyCode::Enter);
    assert!(!t.editing());
    let f = t.frame();
    assert!(f.contains("[ ] Type your own answer") && !f.contains("Select at least"));
    assert_eq!(t.reply(), None);
}

// form.test.tsx: "defers multiselect validation until submission"
#[test]
fn oc_defers_multiselect_validation_until_submission() {
    let mut t = mount(json!([
        { "key": "targets", "type": "multiselect", "options": [{ "value": "staging", "label": "Staging" }],
          "required": true, "minItems": 1 },
        { "key": "priority", "type": "multiselect", "options": [{ "value": "now", "label": "Now" }] },
    ]));
    t.press(KeyCode::Enter);
    t.press(KeyCode::Enter);
    t.press(KeyCode::Right);
    let f = t.frame();
    assert!(
        f.contains("[ ] Now") && !f.contains("Select at least"),
        "{f}"
    );
    t.press(KeyCode::Right);
    t.press(KeyCode::Enter);
    assert!(t.frame().contains("Select at least"));
    assert_eq!(t.reply(), None);
}

// form.test.tsx: "submits an optional empty multiselect as an omitted answer"
#[test]
fn oc_submits_an_optional_empty_multiselect_as_an_omitted_answer() {
    let mut t = mount(json!([
        { "key": "targets", "type": "multiselect", "options": [{ "value": "staging", "label": "Staging" }],
          "minItems": 1 },
        { "key": "priority", "type": "multiselect", "options": [{ "value": "now", "label": "Now" }],
          "default": ["now"] },
    ]));
    t.press(KeyCode::Enter);
    t.press(KeyCode::Enter);
    t.press(KeyCode::Right);
    t.press(KeyCode::Right);
    t.press(KeyCode::Enter);
    assert_eq!(t.replied(), Some(json!({ "priority": ["now"] })));
}

// form.test.tsx: "typing a custom single-select answer selects it without submitting"
#[test]
fn oc_typing_a_custom_single_select_answer_selects_it_without_submitting() {
    let mut t = custom_choice();
    t.press(KeyCode::Down);
    t.press(KeyCode::Enter);
    t.typ("production");
    assert!(t.frame().contains("production"));
    assert_eq!(t.reply(), None);
}

// form.test.tsx: "committing a custom multiselect answer keeps one editable custom row"
#[test]
fn oc_committing_a_custom_multiselect_answer_keeps_one_editable_custom_row() {
    let mut t = custom_multi(json!({}));
    t.app.on_paste("production");
    assert_eq!(t.editor(), "production");
    t.press(KeyCode::Enter);
    let f = t.frame();
    assert!(f.contains("2. [✓] production"), "{f}");
    assert!(!f.contains("Type your own answer") && !f.contains("3."));
    assert_eq!(t.reply(), None);
}

// form.test.tsx: "text fields retain default paste behavior"
#[test]
fn oc_text_fields_retain_default_paste_behavior() {
    let mut t = mount(json!([{ "key": "notes", "type": "string" }]));
    t.app.on_paste("normal paste");
    assert_eq!(t.editor(), "normal paste");
    assert_eq!(t.reply(), None);
}

// form.test.tsx: "ctrl+c clears a text field before cancelling its form"
#[test]
fn oc_ctrl_c_clears_a_text_field_before_cancelling_its_form() {
    let mut t = mount(json!([{ "key": "notes", "type": "string" }]));
    t.typ("draft answer");
    assert_eq!(t.editor(), "draft answer");
    t.ctrl('c');
    assert_eq!(t.editor(), "");
    assert_eq!(t.reply(), None);
    t.ctrl('c');
    assert_eq!(t.reply(), Some(None));
}

// form.test.tsx: "pasting on a choice without custom answers does not open an editor"
#[test]
fn oc_pasting_on_a_choice_without_custom_answers_does_not_open_an_editor() {
    let mut t = mount(staging("string", json!({})));
    t.app.on_paste("production");
    assert!(!t.editing());
    assert!(!t.frame().contains("production"));
    assert_eq!(t.reply(), None);
}

// form.test.tsx: "space toggles the selected multiselect option"
#[test]
fn oc_space_toggles_the_selected_multiselect_option() {
    let mut t = mount(
        json!([{ "key": "targets", "type": "multiselect", "options": [
        { "value": "staging", "label": "Staging" }, { "value": "production", "label": "Production" },
    ] }]),
    );
    t.press(KeyCode::Char(' '));
    assert!(t.frame().contains("[✓] Staging"));
    assert_eq!(t.reply(), None);
    t.press(KeyCode::Char(' '));
    assert!(t.frame().contains("[ ] Staging"));
    assert_eq!(t.reply(), None);
}

// form.test.tsx: "keeps a visible space between a multiselect marker and wrapped label"
#[test]
fn oc_keeps_a_visible_space_between_a_multiselect_marker_and_wrapped_label() {
    let v = "responsive verification across narrow and wide terminal layouts";
    let t = mount_at(
        json!([{ "key": "targets", "type": "multiselect",
            "options": [{ "value": v, "label": "Responsive verification across narrow and wide terminal layouts" }],
            "default": [v] }]),
        58,
        20,
    );
    assert!(t.frame().contains("[✓] Responsive verification"));
}

// form.test.tsx: "space activates the custom multiselect option"
#[test]
fn oc_space_activates_the_custom_multiselect_option() {
    let mut t = custom_multi(json!({}));
    t.press(KeyCode::Down);
    t.press(KeyCode::Char(' '));
    assert!(t.editing());
    assert!(t.frame().contains("[✓] Type your own answer"));
    assert_eq!(t.reply(), None);
}

// form.test.tsx: "space does not select a single-choice option"
#[test]
fn oc_space_does_not_select_a_single_choice_option() {
    let mut t = mount(staging("string", json!({})));
    t.press(KeyCode::Char(' '));
    assert!(!t.frame().contains("Staging ✓"));
    assert_eq!(t.reply(), None);
}

// The dialog tests: a question that arrives while another dialog is open
// waits behind it. (adapted) CodeSmith renames with `/rename <title>`, not
// a dialog: a filtered picker stands in for the rename dialog, its filter
// for the rename input.

fn picker(a: &mut App) {
    let items = ["Initial", "Initial 2"]
        .iter()
        .map(|l| PickItem {
            id: l.to_string(),
            label: l.to_string(),
            detail: String::new(),
            current: false,
        })
        .collect();
    a.modal = Some(Modal::Picker(Picker::new(Pick::Session, "Sessions", items)));
}

fn ask(a: &mut App, q: Value) -> Receiver<Option<Answer>> {
    let (tx, rx) = oneshot::channel();
    let mut i = info(json!([q]));
    i.title = "Question".into();
    a.on_event(Ev::Form(i, tx));
    rx
}

fn shows(a: &App, s: &str) -> bool {
    let mut term = Terminal::new(TestBackend::new(80, 30)).unwrap();
    term.draw(|f| super::view::draw(f, a)).unwrap();
    let buf = term.backend().buffer().clone();
    let text: String = buf.content().iter().map(|c| c.symbol()).collect();
    text.contains(s)
}

// form.test.tsx: "arriving choice question keeps focus with an open rename dialog until it closes"
#[test]
fn oc_arriving_choice_question_keeps_focus_with_an_open_dialog_until_it_closes() {
    let mut a = App::new();
    picker(&mut a);
    let mut rx = ask(
        &mut a,
        json!({ "key": "approach", "type": "string", "description": "Which approach should I take?",
            "options": [{ "value": "a", "label": "Option A" }, { "value": "b", "label": "Option B" }],
            "custom": true }),
    );
    for c in " 2".chars() {
        a.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    let Some(Modal::Picker(p)) = &a.modal else {
        panic!("the picker keeps the keys")
    };
    assert_eq!(p.filter, " 2");
    let picked = a.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(
        picked,
        Some(super::app::Action::Picked(Pick::Session, _))
    ));
    assert!(rx.try_recv().is_err());
    assert!(shows(&a, "Option A"));
    a.on_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
    assert_eq!(
        serde_json::to_value(rx.try_recv().unwrap().unwrap()).unwrap(),
        json!({ "approach": "b" })
    );
}

// form.test.tsx: "arriving text question does not steal editor focus from an open rename dialog"
#[test]
fn oc_arriving_text_question_does_not_steal_editor_focus_from_an_open_dialog() {
    let mut a = App::new();
    picker(&mut a);
    let mut rx = ask(
        &mut a,
        json!({ "key": "notes", "type": "string", "description": "Explain why" }),
    );
    for c in "Initial".chars() {
        a.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    a.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(shows(&a, "Explain why"));
    assert!(rx.try_recv().is_err());
    for c in "because".chars() {
        a.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    a.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    a.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        serde_json::to_value(rx.try_recv().unwrap().unwrap()).unwrap(),
        json!({ "notes": "because" })
    );
}

// form.test.tsx: "escape closes an open command palette instead of cancelling an arriving question"
#[test]
fn oc_escape_closes_an_open_command_palette_instead_of_cancelling_an_arriving_question() {
    let mut a = App::new();
    a.open_palette();
    assert!(matches!(&a.modal, Some(Modal::Picker(p)) if p.kind == Pick::Command));
    let mut rx = ask(
        &mut a,
        json!({ "key": "approach", "type": "string",
            "options": [{ "value": "a", "label": "Option A" }, { "value": "b", "label": "Option B" }] }),
    );
    a.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!matches!(&a.modal, Some(Modal::Picker(_))));
    assert!(rx.try_recv().is_err());
    assert!(shows(&a, "Option A"));
    a.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        serde_json::to_value(rx.try_recv().unwrap().unwrap()).unwrap(),
        json!({ "approach": "a" })
    );
}

// The question tool's prompts become OpenCode's fields and come back as
// labels (and custom answers) by prompt.
#[test]
fn question_prompts_become_custom_choice_fields() {
    use codesmith_core::question::{Choice, Prompt};
    let p = Prompt {
        question: "Which database?".into(),
        header: "DB".into(),
        options: vec![Choice {
            label: "Postgres".into(),
            description: "the usual".into(),
        }],
        multiple: true,
    };
    let f = form::question_field(&p, 0);
    assert_eq!(
        serde_json::to_value(&f).unwrap(),
        json!({ "type": "multiselect", "key": "q0", "title": "DB", "description": "Which database?",
            "options": [{ "value": "Postgres", "label": "Postgres", "description": "the usual" }],
            "custom": true })
    );
    let a: Answer = [("q0".to_string(), json!(["Postgres", "SQLite"]))].into();
    assert_eq!(
        form::question_answers(2, &a),
        vec![vec!["Postgres".to_string(), "SQLite".into()], vec![]]
    );
    let mut fm = form::Form::questions(&[p]);
    assert_eq!(fm.paste("x"), Outcome::Nothing);
}
