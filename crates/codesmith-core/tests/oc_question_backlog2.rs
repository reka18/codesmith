//! Ports of the question and todo tool cases in PARITY.md's porting
//! backlog (`packages/core/test/tool-question.test.ts`,
//! `tool-todo.test.ts`, MIT), through a session on the scripted model.

mod scripted;

use codesmith_core::permission::Reply;
use codesmith_core::question::{self, Prompt, Questioner};
use codesmith_core::tool::todo;
use scripted::*;
use serde_json::json;
use std::sync::{Arc, Mutex};

/// Answers with `answers` (`None`: dismisses) and records what it was asked.
struct Asked {
    answers: Option<Vec<Vec<String>>>,
    seen: Mutex<Vec<Vec<Prompt>>>,
}

impl Questioner for Asked {
    fn ask(&self, prompts: &[Prompt]) -> Option<Vec<Vec<String>>> {
        self.seen.lock().unwrap().push(prompts.to_vec());
        self.answers.clone()
    }
}

fn asked(answers: Option<Vec<Vec<String>>>) -> Arc<Asked> {
    Arc::new(Asked {
        answers,
        seen: Mutex::default(),
    })
}

fn questions() -> serde_json::Value {
    json!([
        {"question": "What should happen?", "header": "Action", "options": [{"label": "Build", "description": "Build it"}]},
        {"question": "Which environment?", "header": "Environment", "options": [{"label": "Dev", "description": "Development"}], "multiple": true},
        {"question": "Anything else?", "header": "Optional", "options": []},
    ])
}

/// "emits one item schema for the nonempty questions array": one `items`
/// schema, at least one question; an empty list is refused before anyone
/// is asked.
#[tokio::test(flavor = "multi_thread")]
async fn oc_question_emits_one_item_schema_for_the_nonempty_questions_array() {
    let (_, schema) = question::spec();
    let q = &schema["properties"]["questions"];
    assert_eq!(q["type"], "array");
    assert_eq!(q["minItems"], 1);
    assert!(q["items"].is_object());
    assert!(q.get("prefixItems").is_none());
    let user = asked(Some(vec![]));
    let mut t = start(Setup {
        questioner: Some(user.clone()),
        ..Default::default()
    })
    .await;
    t.push([
        call("call-question-empty", "question", json!({"questions": []})),
        text("ok"),
    ]);
    t.prompt("Ask").await.unwrap();
    let r = results(&t.s);
    assert!(r[0].2, "{r:?}");
    assert!(user.seen.lock().unwrap().is_empty());
}

/// "omits a catalog-denied question and enforces its leaf permission"
#[tokio::test(flavor = "multi_thread")]
async fn oc_question_omits_a_denied_question_and_refuses_it() {
    let user = asked(Some(vec![]));
    let mut t = start(Setup {
        questioner: Some(user.clone()),
        extra: json!({"permissions": [{"action": "question", "resource": "*", "effect": "deny"}]}),
        ..Default::default()
    })
    .await;
    t.push([
        call(
            "call-question-denied",
            "question",
            json!({"questions": questions()}),
        ),
        text("ok"),
    ]);
    t.prompt("Ask").await.unwrap();
    assert!(!tool_names(&t.requests()[0]).contains(&"question".to_string()));
    let r = results(&t.s);
    assert!(r[0].2, "{r:?}");
    assert!(user.seen.lock().unwrap().is_empty());
}

/// "registers question and projects user answers without a permission
/// assertion": with every permission prompt refused, the user is still
/// asked, and the answers reach the model.
#[tokio::test(flavor = "multi_thread")]
async fn oc_question_returns_answers_without_a_permission_prompt() {
    let user = asked(Some(vec![vec!["Build".into()], vec!["Dev".into()], vec![]]));
    let mut t = start(Setup {
        questioner: Some(user.clone()),
        reply: Reply::Reject(None),
        ..Default::default()
    })
    .await;
    t.push([
        call(
            "call-question",
            "question",
            json!({"questions": questions()}),
        ),
        text("ok"),
    ]);
    t.prompt("Ask").await.unwrap();
    assert!(tool_names(&t.requests()[0]).contains(&"question".to_string()));
    let r = results(&t.s);
    assert!(!r[0].2, "{r:?}");
    assert_eq!(
        r[0].1,
        "User has answered your questions: \"What should happen?\"=\"Build\", \"Which environment?\"=\"Dev\", \"Anything else?\"=\"Unanswered\". You can now continue with the user's answers in mind."
    );
    assert_eq!(user.seen.lock().unwrap().len(), 1);
}

/// "keeps dismissed questions out of model-facing output": a dismissal
/// says only that, and stops the turn.
#[tokio::test(flavor = "multi_thread")]
async fn oc_question_keeps_dismissed_questions_out_of_model_facing_output() {
    let user = asked(None);
    let mut t = start(Setup {
        questioner: Some(user),
        ..Default::default()
    })
    .await;
    t.push([
        call(
            "call-question",
            "question",
            json!({"questions": questions()}),
        ),
        text("must not run"),
    ]);
    let _ = t.prompt("Ask").await;
    let r = results(&t.s);
    assert_eq!(r[0].1, "The user dismissed this question");
    assert!(!r[0].1.contains("What should happen?"));
    assert_eq!(t.requests().len(), 1, "the turn stops");
}

/// "registers todowrite on the native tool list and summarizes the update"
#[tokio::test(flavor = "multi_thread")]
async fn oc_todo_registers_todowrite_and_summarizes_the_update() {
    let (_, schema) = todo::spec();
    assert_eq!(schema["properties"]["todos"]["type"], "array");
    assert_eq!(
        schema["properties"]["todos"]["items"]["required"],
        json!(["content", "status"])
    );
    let todos = json!([
        {"content": "Design", "status": "completed"},
        {"content": "Write tests", "status": "in_progress"},
        {"content": "Ship", "status": "pending"},
        {"content": "Old plan", "status": "cancelled"},
    ]);
    let mut t = plain().await;
    t.push([
        call("call-todo", "todowrite", json!({"todos": todos})),
        text("ok"),
    ]);
    t.prompt("Plan").await.unwrap();
    assert!(tool_names(&t.requests()[0]).contains(&"todowrite".to_string()));
    let r = results(&t.s);
    assert_eq!(
        r[0].1,
        "Todos updated: 1/4 completed, 1 in progress, 1 pending, 1 cancelled. In progress: \"Write tests\""
    );
}
