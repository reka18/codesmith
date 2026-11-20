//! Ports of OpenCode's form and MCP elicitation tests (MIT): `form.test.ts`
//! and the elicitation cases of `mcp.test.ts`. Each test names the OpenCode
//! test it ports; see `crates/codesmith-core/PARITY.md`.

use codesmith_core::form::{self, Create, Error, Field, Forms, State};
use serde_json::{json, Value};

fn fields(v: Value) -> Vec<Field> {
    serde_json::from_value(v).unwrap()
}

fn create(title: &str, f: Value) -> Create {
    Create {
        session_id: form::GLOBAL.into(),
        title: title.into(),
        fields: fields(f),
        ..Create::default()
    }
}

fn answer(v: Value) -> form::Answer {
    serde_json::from_value(v).unwrap()
}

fn invalid(id: &str, message: &str) -> Error {
    Error::InvalidAnswer {
        id: id.into(),
        message: message.into(),
    }
}

// form.test.ts: "validates absolute URI formats without restricting schemes"
#[test]
fn oc_form_validates_absolute_uris_without_restricting_schemes() {
    let f = fields(json!([{"key": "uri", "type": "string", "format": "uri"}]));
    let check = |v: &str| form::validate_answer(&f, &answer(json!({"uri": v})));
    assert_eq!(check("https://example.com/path"), None);
    assert_eq!(check("mailto:user@example.com"), None);
    assert_eq!(
        check("relative/path").as_deref(),
        Some("Expected URI for form field: uri")
    );
    assert_eq!(
        check("://invalid").as_deref(),
        Some("Expected URI for form field: uri")
    );
}

// form.test.ts: "returns a terminal cancelled state from ask"
#[tokio::test]
async fn oc_form_returns_a_terminal_cancelled_state_from_ask() {
    let forms = Forms::new();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    forms.listen(move |e| {
        if let form::Event::Created(f) = e {
            let _ = tx.send(f.id.clone());
        }
        Ok(())
    });
    let asking = {
        let forms = forms.clone();
        tokio::spawn(async move {
            forms
                .ask(Create {
                    id: Some("frm_test".into()),
                    session_id: "ses_test".into(),
                    title: "Test form".into(),
                    metadata: None,
                    fields: fields(json!([{"key": "name", "type": "string", "required": true}])),
                })
                .await
        })
    };
    let id = rx.recv().await.unwrap();
    forms.cancel(&id, None).unwrap();
    assert_eq!(
        asking.await.unwrap().unwrap(),
        State::Cancelled { message: None }
    );
    assert_eq!(
        forms.state(&id).unwrap(),
        State::Cancelled { message: None }
    );
}

// form.test.ts: "supports the temporary global mcp elicitation owner"
#[test]
fn oc_form_supports_the_global_mcp_elicitation_owner() {
    let forms = Forms::new();
    let created = forms
        .create(create(
            "MCP input",
            json!([{"key": "name", "type": "string", "required": true}]),
        ))
        .unwrap();
    assert_eq!(created.session_id, "global");
    assert_eq!(created.title, "MCP input");
    let owned: Vec<String> = forms
        .list(Some("global"))
        .into_iter()
        .map(|f| f.id)
        .collect();
    assert_eq!(owned, std::slice::from_ref(&created.id));
    assert!(forms.list(Some("other")).is_empty());
    forms
        .reply(&created.id, answer(json!({"name": "Ava"})))
        .unwrap();
    assert_eq!(
        forms.state(&created.id).unwrap(),
        State::Answered {
            answer: answer(json!({"name": "Ava"}))
        }
    );
    let external = forms
        .create(create(
            "External setup",
            json!([{"key": "setup", "type": "external", "url": "https://example.com/setup"}]),
        ))
        .unwrap();
    forms
        .reply(&external.id, answer(json!({"setup": true})))
        .unwrap();
    assert_eq!(
        forms.state(&external.id).unwrap(),
        State::Answered {
            answer: answer(json!({"setup": true}))
        }
    );
}

// form.test.ts: "gates required fields and rejects inactive answers via when"
#[test]
fn oc_form_gates_required_fields_and_rejects_inactive_answers() {
    let forms = Forms::new();
    let f = forms
        .create(create(
            "Conditional form",
            json!([
                {"key": "confirm", "type": "boolean", "required": true},
                {"key": "reason", "type": "string", "required": true, "when": [{"key": "confirm", "op": "eq", "value": false}]},
            ]),
        ))
        .unwrap();
    assert_eq!(
        forms.reply(&f.id, answer(json!({"confirm": true, "reason": "x"}))),
        Err(invalid(&f.id, "Form field is not active: reason"))
    );
    assert_eq!(
        forms.reply(&f.id, answer(json!({"confirm": false}))),
        Err(invalid(&f.id, "Missing required form field: reason"))
    );
    forms
        .reply(
            &f.id,
            answer(json!({"confirm": false, "reason": "not ready"})),
        )
        .unwrap();
    assert_eq!(
        forms.state(&f.id).unwrap(),
        State::Answered {
            answer: answer(json!({"confirm": false, "reason": "not ready"}))
        }
    );
}

const LANGS: &str = r#"[{"value": "go", "label": "Go"}, {"value": "ts", "label": "TypeScript"}]"#;

// form.test.ts: "evaluates when against multiselect answers as inclusion"
#[test]
fn oc_form_evaluates_when_against_multiselect_answers_as_inclusion() {
    let forms = Forms::new();
    let options: Value = serde_json::from_str(LANGS).unwrap();
    let f = forms
        .create(create(
            "Multiselect form",
            json!([
                {"key": "langs", "type": "multiselect", "options": options},
                {"key": "goVersion", "type": "string", "required": true, "when": [{"key": "langs", "op": "eq", "value": "go"}]},
            ]),
        ))
        .unwrap();
    assert_eq!(
        forms.reply(&f.id, answer(json!({"langs": ["go", "ts"]}))),
        Err(invalid(&f.id, "Missing required form field: goVersion"))
    );
    forms
        .reply(&f.id, answer(json!({"langs": ["ts"]})))
        .unwrap();
    assert_eq!(
        forms.state(&f.id).unwrap(),
        State::Answered {
            answer: answer(json!({"langs": ["ts"]}))
        }
    );
}

// form.test.ts: "requires every when condition to match and treats empty when as active"
#[test]
fn oc_form_requires_every_condition_and_treats_empty_when_as_active() {
    let forms = Forms::new();
    let f = forms
        .create(create(
            "Dependent form",
            json!([
                {"key": "a", "type": "boolean"},
                {"key": "b", "type": "boolean"},
                {"key": "x", "type": "string", "required": true, "when": [
                    {"key": "a", "op": "eq", "value": true},
                    {"key": "b", "op": "eq", "value": true},
                ]},
                {"key": "z", "type": "string", "required": true, "when": []},
            ]),
        ))
        .unwrap();
    assert_eq!(
        forms.reply(&f.id, answer(json!({"a": true, "b": true, "z": "ok"}))),
        Err(invalid(&f.id, "Missing required form field: x"))
    );
    assert_eq!(
        forms.reply(
            &f.id,
            answer(json!({"a": true, "b": false, "x": "nope", "z": "ok"}))
        ),
        Err(invalid(&f.id, "Form field is not active: x"))
    );
    assert_eq!(
        forms.reply(&f.id, answer(json!({"a": true, "b": false}))),
        Err(invalid(&f.id, "Missing required form field: z"))
    );
    forms
        .reply(&f.id, answer(json!({"a": true, "b": false, "z": "ok"})))
        .unwrap();
    assert_eq!(
        forms.state(&f.id).unwrap(),
        State::Answered {
            answer: answer(json!({"a": true, "b": false, "z": "ok"}))
        }
    );
}

// form.test.ts: "evaluates neq against multiselect answers as non-inclusion"
#[test]
fn oc_form_evaluates_neq_against_multiselect_answers_as_non_inclusion() {
    let forms = Forms::new();
    let options: Value = serde_json::from_str(LANGS).unwrap();
    let f = forms
        .create(create(
            "Selection form",
            json!([
                {"key": "langs", "type": "multiselect", "options": options},
                {"key": "note", "type": "string", "required": true, "when": [{"key": "langs", "op": "neq", "value": "go"}]},
            ]),
        ))
        .unwrap();
    assert_eq!(
        forms.reply(&f.id, answer(json!({"langs": ["ts"]}))),
        Err(invalid(&f.id, "Missing required form field: note"))
    );
    assert_eq!(
        forms.reply(&f.id, answer(json!({"langs": []}))),
        Err(invalid(&f.id, "Missing required form field: note"))
    );
    assert_eq!(
        forms.reply(&f.id, answer(json!({"langs": ["go"], "note": "x"}))),
        Err(invalid(&f.id, "Form field is not active: note"))
    );
    forms
        .reply(&f.id, answer(json!({"langs": ["go"]})))
        .unwrap();
    assert_eq!(
        forms.state(&f.id).unwrap(),
        State::Answered {
            answer: answer(json!({"langs": ["go"]}))
        }
    );
}

// form.test.ts: "treats unanswered when references as false and cascades inactivity"
#[test]
fn oc_form_treats_unanswered_references_as_false_and_cascades() {
    let forms = Forms::new();
    let f = forms
        .create(create(
            "Cascading form",
            json!([
                {"key": "a", "type": "boolean"},
                {"key": "b", "type": "string", "when": [{"key": "a", "op": "eq", "value": true}]},
                {"key": "c", "type": "string", "required": true, "when": [{"key": "b", "op": "neq", "value": "x"}]},
            ]),
        ))
        .unwrap();
    assert_eq!(
        forms.reply(&f.id, answer(json!({"a": false, "b": "yes"}))),
        Err(invalid(&f.id, "Form field is not active: b"))
    );
    forms.reply(&f.id, answer(json!({"a": false}))).unwrap();
    assert_eq!(
        forms.state(&f.id).unwrap(),
        State::Answered {
            answer: answer(json!({"a": false}))
        }
    );
}

// form.test.ts: "rejects invalid when definitions at creation"
#[test]
fn oc_form_rejects_invalid_when_definitions_at_creation() {
    let forms = Forms::new();
    let flip = |f: Value| forms.create(create("Invalid form", f)).unwrap_err();
    assert_eq!(
        flip(
            json!([{"key": "b", "type": "string", "when": [{"key": "missing", "op": "eq", "value": "x"}]}])
        ),
        Error::InvalidForm(
            "Form field condition must reference an earlier field: b -> missing".into()
        )
    );
    assert_eq!(
        flip(json!([{"key": "a", "type": "string"}, {"key": "a", "type": "string"}])),
        Error::InvalidForm("Duplicate form field key: a".into())
    );
    assert_eq!(
        flip(
            json!([{"key": "a", "type": "external", "url": "https://example.com"}, {"key": "a", "type": "string"}])
        ),
        Error::InvalidForm("Duplicate form field key: a".into())
    );
    assert_eq!(
        flip(json!([
            {"key": "a", "type": "boolean"},
            {"key": "b", "type": "string", "when": [{"key": "a", "op": "eq", "value": "yes"}]},
        ])),
        Error::InvalidForm("Form field condition value must be a boolean: b -> a".into())
    );
    assert_eq!(
        flip(json!([
            {"key": "a", "type": "string", "options": [{"value": "x", "label": "X"}]},
            {"key": "b", "type": "string", "when": [{"key": "a", "op": "eq", "value": "y"}]},
        ])),
        Error::InvalidForm(
            "Form field condition value must be one of the field's options: b -> a".into()
        )
    );
}

// form.test.ts: "requires external field acknowledgements"
#[test]
fn oc_form_requires_external_field_acknowledgements() {
    let forms = Forms::new();
    let f = forms
        .create(create(
            "External setup",
            json!([
                {"key": "authorization", "type": "external", "url": "https://example.com/setup", "title": "Open setup"},
                {"key": "name", "type": "string", "required": true},
            ]),
        ))
        .unwrap();
    for a in [
        json!({"name": "Ava"}),
        json!({"authorization": false, "name": "Ava"}),
        json!({"authorization": "yes", "name": "Ava"}),
    ] {
        assert_eq!(
            forms.reply(&f.id, answer(a)),
            Err(invalid(
                &f.id,
                "External form field must be acknowledged: authorization"
            ))
        );
    }
    forms
        .reply(&f.id, answer(json!({"authorization": true, "name": "Ava"})))
        .unwrap();
    assert_eq!(
        forms.state(&f.id).unwrap(),
        State::Answered {
            answer: answer(json!({"authorization": true, "name": "Ava"}))
        }
    );
}

fn test_input() -> Create {
    Create {
        id: Some("frm_test".into()),
        session_id: "ses_test".into(),
        title: "Test form".into(),
        metadata: None,
        fields: fields(json!([{"key": "name", "type": "string", "required": true}])),
    }
}

// form.test.ts: "cleans up created forms when event publication fails"
#[test]
fn oc_form_cleans_up_created_forms_when_a_listener_fails() {
    let forms = Forms::new();
    let l = forms.listen(|e| match e {
        form::Event::Created(_) => Err("create listener failed".into()),
        _ => Ok(()),
    });
    assert!(forms.create(test_input()).is_err());
    assert_eq!(
        forms.get("frm_test"),
        Err(Error::NotFound("frm_test".into()))
    );
    forms.unlisten(l);
    assert_eq!(forms.create(test_input()).unwrap().id, "frm_test");
}

// form.test.ts: "keeps forms pending when reply event publication fails"
#[test]
fn oc_form_keeps_forms_pending_when_a_reply_listener_fails() {
    let forms = Forms::new();
    forms.create(test_input()).unwrap();
    let l = forms.listen(|e| match e {
        form::Event::Replied { .. } => Err("reply listener failed".into()),
        _ => Ok(()),
    });
    assert!(forms
        .reply("frm_test", answer(json!({"name": "Ava"})))
        .is_err());
    assert_eq!(forms.state("frm_test").unwrap(), State::Pending);
    forms.unlisten(l);
    forms
        .reply("frm_test", answer(json!({"name": "Ava"})))
        .unwrap();
    assert_eq!(
        forms.state("frm_test").unwrap(),
        State::Answered {
            answer: answer(json!({"name": "Ava"}))
        }
    );
}

// --- MCP elicitations (mcp.test.ts) ---

use codesmith_core::mcp::{Mcp, McpConfig};
use codesmith_core::question::{Prompt, Questioner};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn python() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|p| {
        std::process::Command::new(p)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

/// The elicitation fixture connected as server `name`.
async fn connect(name: &str) -> Option<Arc<Mcp>> {
    let py = python()?;
    let fixture = format!(
        "{}/tests/fixtures/mcp_elicit.py",
        env!("CARGO_MANIFEST_DIR")
    );
    let cfg: McpConfig = serde_json::from_value(json!({
        "servers": {name: {"type": "local", "command": [py, fixture]}}
    }))
    .unwrap();
    let dir = std::env::temp_dir();
    Some(Mcp::connect(&cfg, &dir).await)
}

/// Answers with `reply` (`None`: waits a while, then dismisses), recording
/// what it was asked.
struct Asker {
    reply: Option<Vec<Vec<String>>>,
    asked: Mutex<Vec<Vec<Prompt>>>,
}

impl Questioner for Asker {
    fn ask(&self, prompts: &[Prompt]) -> Option<Vec<Vec<String>>> {
        self.asked.lock().unwrap().push(prompts.to_vec());
        if self.reply.is_none() {
            std::thread::sleep(Duration::from_millis(1500));
        }
        self.reply.clone()
    }
}

fn asker(reply: Option<Vec<Vec<&str>>>) -> Arc<Asker> {
    Arc::new(Asker {
        reply: reply.map(|r| {
            r.into_iter()
                .map(|a| a.into_iter().map(String::from).collect())
                .collect()
        }),
        asked: Mutex::new(Vec::new()),
    })
}

/// Records the global forms server `name`'s elicitations create.
fn forms_of(name: &str) -> Arc<Mutex<Vec<form::Info>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (s, name) = (seen.clone(), name.to_string());
    form::global().listen(move |e| {
        if let form::Event::Created(f) = e {
            if f.metadata
                .as_ref()
                .is_some_and(|m| m["server"] == name.as_str())
            {
                s.lock().unwrap().push(f.clone());
            }
        }
        Ok(())
    });
    seen
}

fn structured(r: &Value) -> Value {
    r["structuredContent"].clone()
}

// mcp.test.ts: "accepts empty MCP elicitations without creating forms"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_accepts_empty_elicitations_without_creating_forms() {
    let Some(mcp) = connect("elicit-empty").await else {
        return;
    };
    let seen = forms_of("elicit-empty");
    let a = asker(None);
    let r = mcp
        .call_raw_asking("elicit-empty_empty", json!({}), Some(a.clone()))
        .await
        .unwrap();
    assert_eq!(structured(&r), json!({"action": "accept", "content": {}}));
    assert!(seen.lock().unwrap().is_empty());
    assert!(a.asked.lock().unwrap().is_empty());
}

// mcp.test.ts: "acknowledges completed MCP URL elicitations without returning internal content"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_acknowledges_completed_url_elicitations() {
    let Some(mcp) = connect("elicit-url").await else {
        return;
    };
    let seen = forms_of("elicit-url");
    let r = mcp
        .call_raw_asking(
            "elicit-url_url",
            json!({"complete": true}),
            Some(asker(None)),
        )
        .await
        .unwrap();
    assert_eq!(structured(&r), json!({"action": "accept"}));
    let f = seen.lock().unwrap()[0].clone();
    assert_eq!(
        serde_json::to_value(&f.fields).unwrap(),
        json!([{"key": "elicitation", "type": "external", "url": "https://example.com/authorize"}])
    );
    assert_eq!(f.session_id, form::GLOBAL);
    assert_eq!(
        form::global().state(&f.id).unwrap(),
        State::Answered {
            answer: answer(json!({"elicitation": true}))
        }
    );
}

// mcp.test.ts: "cancels MCP elicitation forms when the server gives up on them"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_cancels_elicitation_forms_when_the_server_gives_up() {
    let Some(mcp) = connect("elicit-giveup").await else {
        return;
    };
    let seen = forms_of("elicit-giveup");
    let r = mcp
        .call_raw_asking("elicit-giveup_giveup", json!({}), Some(asker(None)))
        .await
        .unwrap();
    assert_eq!(r["isError"], json!(true));
    let id = seen.lock().unwrap()[0].id.clone();
    let mut state = form::global().state(&id).unwrap();
    for _ in 0..200 {
        if state != State::Pending {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        state = form::global().state(&id).unwrap();
    }
    assert!(matches!(state, State::Cancelled { .. }), "{state:?}");
}

// mcp.test.ts: "settles modern MCP URL elicitations when the user confirms"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_settles_url_elicitations_when_the_user_confirms() {
    let Some(mcp) = connect("elicit-modern").await else {
        return;
    };
    let seen = forms_of("elicit-modern");
    let a = asker(Some(vec![vec!["Done"]]));
    let r = mcp
        .call_raw_asking("elicit-modern_modern", json!({}), Some(a.clone()))
        .await
        .unwrap();
    assert_eq!(structured(&r), json!({"action": "accept"}));
    let f = seen.lock().unwrap()[0].clone();
    assert!(f.metadata.as_ref().unwrap().get("elicitationID").is_none());
    let asked = a.asked.lock().unwrap();
    assert!(asked[0][0]
        .question
        .contains("https://example.com/authorize"));
}

/// Not in OpenCode's tests: a form elicitation through the question UI,
/// and one nobody can answer.
#[tokio::test(flavor = "multi_thread")]
async fn form_elicitations_are_answered_through_the_question_ui() {
    let Some(mcp) = connect("elicit-form").await else {
        return;
    };
    // Fields come in key order (JSON objects are read sorted).
    let a = asker(Some(vec![vec!["Yes"], vec!["Ava"]]));
    let r = mcp
        .call_raw_asking("elicit-form_form", json!({}), Some(a.clone()))
        .await
        .unwrap();
    assert_eq!(
        structured(&r),
        json!({"action": "accept", "content": {"name": "Ava", "confirm": true}})
    );
    let asked = a.asked.lock().unwrap()[0].clone();
    // The message comes first; a machine title gives way to the description.
    assert_eq!(asked[0].question, "Who are you?\n\nGo ahead");
    assert_eq!(asked[0].options.len(), 2);
    assert_eq!(asked[1].question, "Name");
    drop(asked);

    let r = mcp
        .call_raw_asking("elicit-form_form", json!({}), None)
        .await
        .unwrap();
    assert_eq!(structured(&r), json!({"action": "cancel"}));
    let _ = McpConfig::default();
}
