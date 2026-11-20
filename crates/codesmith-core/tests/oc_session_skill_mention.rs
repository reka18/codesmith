//! Ports of OpenCode's skill mention tests (MIT): `session-skill.test.ts`
//! and the skill cases of `session-runner-message.test.ts`. Each test names
//! the OpenCode test it ports; see `crates/codesmith-core/PARITY.md`.
//!
//! Adapted: OpenCode's prompts carry skill attachments with mention
//! offsets from the client; CodeSmith finds `@id` mentions in the prompt's
//! text (see `codesmith_core::skill_mention`).

mod support;

use codesmith_ai::Part;
use codesmith_core::skill_mention::{self, NotFound};
use serde_json::json;
use support::{text, H};

/// A session whose project has the `effect` and `api-design` skills.
async fn with_skills(extra: serde_json::Value) -> H {
    let mut h = H::with(extra, None).await;
    for (id, body) in [("effect", "Use Effect"), ("api-design", "Design APIs")] {
        let dir = h.root().join(".codesmith/skills").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {id}\ndescription: The {id} skill\n---\n{body}\n"),
        )
        .unwrap();
    }
    h.renew();
    h
}

/// The texts of the latest user prompt's message, as the model got them.
fn prompt_parts(h: &H) -> Vec<String> {
    let stored = h.store().history(&h.s.id).unwrap();
    let m = stored
        .iter()
        .rev()
        .find(|m| m.role == codesmith_ai::Role::User)
        .unwrap()
        .clone();
    m.parts
        .iter()
        .filter_map(|p| match p {
            Part::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect()
}

// session-runner-message.test.ts: "lowers each prepared skill once before the prompt"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_lowers_each_mentioned_skill_once_before_the_prompt() {
    let mut h = with_skills(json!({})).await;
    h.prompt("Use @effect and @api-design, then @effect again")
        .await;
    let parts = prompt_parts(&h);
    assert_eq!(parts.len(), 3, "{parts:?}");
    assert!(
        parts[0].starts_with("<skill_content name=\"effect\">"),
        "{parts:?}"
    );
    assert!(parts[0].contains("Use Effect"));
    assert!(parts[1].starts_with("<skill_content name=\"api-design\">"));
    assert!(parts[1].contains("Design APIs"));
    assert_eq!(parts[2], "Use @effect and @api-design, then @effect again");
    // The model gets them in that order, in the prompt's message.
    let req = h.requests().last().unwrap().clone();
    let user = req["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rfind(|m| m["role"] == "user")
        .unwrap()
        .to_string();
    let (e, a, p) = (
        user.find("Use Effect").unwrap(),
        user.find("Design APIs").unwrap(),
        user.find("then @effect again").unwrap(),
    );
    assert!(e < a && a < p, "{user}");
    // Views show only what was typed.
    assert!(parts[..2]
        .iter()
        .all(|t| codesmith_core::runner::is_engine_message(t)));
}

// session-runner-message.test.ts: "does not inject skill content for reference-only attachments"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_does_not_inject_skill_content_for_mere_references() {
    let mut h = with_skills(json!({})).await;
    // Not a mention: inside a word, an unknown skill, or a file in the project.
    std::fs::write(h.root().join("api-design"), "a file").unwrap();
    h.prompt("mail me@effect about @unknown and @api-design")
        .await;
    assert_eq!(
        prompt_parts(&h),
        ["mail me@effect about @unknown and @api-design"]
    );
}

// session-skill.test.ts: "refuses blocked skill mentions and standalone activation without writing their content"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_refuses_blocked_skill_mentions_and_activation() {
    let mut h = with_skills(json!({
        "permissions": [{"action": "skill", "resource": "effect", "effect": "deny"}]
    }))
    .await;
    let e = h.s.activate_skill("effect").unwrap_err();
    assert_eq!(
        e.downcast_ref::<NotFound>(),
        Some(&NotFound {
            skill: "effect".into()
        })
    );
    let e =
        h.s.prompt("@effect", &mut support::Quiet)
            .await
            .unwrap_err();
    assert_eq!(
        e.downcast_ref::<NotFound>(),
        Some(&NotFound {
            skill: "effect".into()
        })
    );
    assert!(h.s.messages.is_empty());
    assert!(h.store().get(&h.s.id).unwrap().is_none(), "nothing written");
    assert!(h.requests().is_empty());
    // Another skill is fine.
    h.prompt("@api-design").await;
    assert_eq!(prompt_parts(&h).len(), 2);
}

// session-skill.test.ts: "materializes mentioned skills on their owning prompt"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_materializes_mentioned_skills_on_their_prompt() {
    let mut h = with_skills(json!({})).await;
    h.prompt("Apply @effect and @effect").await;
    let parts = prompt_parts(&h);
    assert_eq!(parts.len(), 2, "{parts:?}");
    assert!(parts[0].contains("Use Effect"));
    assert_eq!(parts[1], "Apply @effect and @effect");
    // Stored with the prompt, so a reopened session has it.
    let id = h.s.id.clone();
    let reopened = h.open(&id);
    let user = reopened
        .messages
        .iter()
        .find(|m| m.role == codesmith_ai::Role::User)
        .unwrap();
    assert_eq!(user.parts.len(), 2);
}

// session-skill.test.ts: "excludes mentioned skills when forking before their prompt"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_excludes_mentioned_skills_when_forking_before_their_prompt() {
    let mut h = with_skills(json!({})).await;
    h.prompt("Before the skill").await;
    h.prompt("Apply @effect").await;
    let id = h.s.id.clone();
    let mut st = h.store();
    let entries = st.entries(&id).unwrap();
    let selected = codesmith_core::fork::messages(&entries)
        .into_iter()
        .find(|(_, m)| text(m).contains("Apply @effect"))
        .unwrap()
        .0;
    st.fork_at(&id, "fork", Some(selected)).unwrap();
    let texts: Vec<String> = st.history("fork").unwrap().iter().map(text).collect();
    assert_eq!(texts, ["Before the skill", "ok"]);
    assert!(!texts.iter().any(|t| t.contains("Use Effect")));
}

// session-skill.test.ts: "publishes raw standalone content under the caller-supplied ID without inbox admission"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_activates_a_skill_with_its_raw_content_without_the_model() {
    let mut h = with_skills(json!({})).await;
    h.s.activate_skill("effect").unwrap();
    let history = h.store().history(&h.s.id).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(text(&history[0]).trim(), "Use Effect");
    assert!(h.requests().is_empty(), "no model call");
    assert!(!skill_mention::is_content(&text(&history[0])));
}
