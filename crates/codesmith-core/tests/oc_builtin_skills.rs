//! The skills shipped with CodeSmith, ported from OpenCode's
//! `plugin/skill.test.ts` (MIT).
//!
//! Adapted: OpenCode's `opencode` skill is CodeSmith's `codesmith`, and the
//! built-in skills are offered only with `"builtin_skills": true`, so a
//! session's prompt is unchanged by default.

mod support;

use codesmith_core::skill;
use serde_json::json;
use support::H;

// plugin/skill.test.ts: "registers built-in skills"
#[test]
fn oc_skill_registers_built_in_skills() {
    let skills = skill::builtin("1.2.3");
    let codesmith = skills.iter().find(|s| s.id == "codesmith").unwrap();
    assert_eq!(codesmith.name, "CodeSmith");
    assert_eq!(
        codesmith.path,
        std::path::PathBuf::from("skill://codesmith/SKILL.md")
    );
    assert!(codesmith
        .description
        .contains("any question about CodeSmith itself"));
    let report = skills.iter().find(|s| s.id == "report").unwrap();
    assert_eq!(report.name, "Report");
    assert_eq!(
        report.path,
        std::path::PathBuf::from("skill://report/SKILL.md")
    );
    assert!(report.description.contains("CodeSmith issue"));
    assert!(report.content.contains("- CodeSmith version: 1.2.3"));
    assert!(report
        .content
        .contains("gh issue create --repo reka18/codesmith"));
}

#[test]
fn a_users_skill_of_the_same_id_wins() {
    let mine = skill::Skill {
        id: "report".into(),
        name: "Mine".into(),
        description: "my report".into(),
        path: "/x/report/SKILL.md".into(),
        content: "mine".into(),
    };
    let all = skill::with_builtin(vec![mine], "1");
    let ids: Vec<&str> = all.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, ["codesmith", "report"]);
    assert_eq!(all[1].name, "Mine");
}

fn tool_names(body: &serde_json::Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["function"]["name"].as_str().map(str::to_string))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn built_in_skills_are_offered_only_when_turned_on() {
    let mut off = H::new().await;
    off.prompt("hi").await;
    let body = off.requests().last().cloned().unwrap();
    assert!(!tool_names(&body).contains(&"skill".to_string()));

    let mut on = H::with(json!({"builtin_skills": true}), None).await;
    on.prompt("hi").await;
    let body = on.requests().last().cloned().unwrap();
    assert!(tool_names(&body).contains(&"skill".to_string()), "{body}");
    assert!(body.to_string().contains("codesmith"));
}
