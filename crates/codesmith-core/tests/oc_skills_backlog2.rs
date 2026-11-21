//! Ports of the skill and command cases in PARITY.md's porting backlog
//! (`packages/core/test/config/markdown.test.ts`, `config/skill.test.ts`,
//! `plugin/command.test.ts`, `skill.test.ts`, `skill/instructions.test.ts`,
//! `tool-skill.test.ts`, MIT). OpenCode's skill service filters by the
//! agent's permissions; here the session does, from the agent's rules.

mod scripted;

use codesmith_core::session::{Options, Session};
use codesmith_core::{command, skill};
use scripted::*;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn write_skill(dir: &Path, id: &str, description: &str, body: &str) {
    let d = dir.join(id);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("SKILL.md"),
        format!("---\nname: {id}\ndescription: {description}\n---\n{body}\n"),
    )
    .unwrap();
}

/// "parses plain content without frontmatter"
#[test]
fn oc_markdown_parses_plain_content_without_frontmatter() {
    let (meta, body) = skill::frontmatter("just body");
    assert!(meta.is_empty());
    assert_eq!(body, "just body");
}

/// "maps config entry types to skill directories". Fixed: a configured
/// `~/shared` was looked for under the project (`<project>/~/shared`); it
/// is in the home folder now, and `./relative` stays the project's.
#[test]
fn oc_skill_roots_map_config_entries_to_skill_directories() {
    let tmp = tempfile::tempdir().unwrap();
    let (config, home, project) = (
        tmp.path().join("config"),
        tmp.path().join("home"),
        tmp.path().join("project"),
    );
    let roots = skill::roots(
        &project,
        &project,
        &config,
        &home,
        &["~/shared".into(), "./relative".into()],
    );
    for want in [
        config.join("skills"),
        home.join(".agents/skills"),
        project.join(".agents/skills"),
        project.join(".codesmith/skills"),
        home.join("shared"),
        project.join("./relative"),
    ] {
        assert!(roots.contains(&want), "{want:?} not in {roots:?}");
    }
    assert!(!roots.iter().any(|r| r.to_string_lossy().contains('~')));
    // A configured folder's skills are found.
    write_skill(&home.join("shared"), "shared", "Shared skill", "Body");
    let found = skill::discover(&roots);
    assert!(found.iter().any(|s| s.id == "shared"));
}

/// "prefers a worktree skill over the parent checkout copy": the walk for
/// a worktree stops at its own root.
#[test]
fn oc_skill_prefers_a_worktree_skill_over_the_parent_checkout_copy() {
    let tmp = tempfile::tempdir().unwrap();
    let checkout = tmp.path().join("repo");
    let worktree = checkout.join(".worktrees/feature");
    std::fs::create_dir_all(checkout.join(".git")).unwrap();
    std::fs::create_dir_all(&worktree).unwrap();
    std::fs::write(
        worktree.join(".git"),
        "gitdir: ../../../.git/worktrees/feature\n",
    )
    .unwrap();
    write_skill(
        &checkout.join(".agents/skills"),
        "review",
        "Parent checkout",
        "x",
    );
    write_skill(&worktree.join(".agents/skills"), "review", "Worktree", "x");
    let roots = skill::roots(
        &worktree,
        &worktree,
        &tmp.path().join("config"),
        &tmp.path().join("home"),
        &[],
    );
    let found = skill::discover(&roots);
    let review = found.iter().find(|s| s.id == "review").unwrap();
    assert_eq!(review.description, "Worktree");
    assert_eq!(review.path, worktree.join(".agents/skills/review/SKILL.md"));
}

/// "registers built-in init and review commands"
#[test]
fn oc_command_registers_builtin_init_and_review_commands() {
    let b = command::builtins(Path::new("/project"));
    assert!(b["init"].template.contains("/project"));
    assert!(b["review"].native);
}

// --- Skills and the agent's rules ---

/// A session of `agent` (configured with `permissions`) in a project with
/// the skills `effect`, `deploy` and `hidden`; the first request it sends.
async fn first_request(permissions: Value, replies: Vec<String>) -> (T, Session, Value) {
    let t = start(Setup {
        extra: json!({"agents": {"x": {"mode": "primary", "permissions": permissions}}}),
        ..Default::default()
    })
    .await;
    let skills: PathBuf = t.path(".codesmith/skills");
    write_skill(
        &skills,
        "effect",
        "Build applications with Effect",
        "Use Effect",
    );
    write_skill(&skills, "deploy", "Deploy things", "Deploy carefully");
    write_skill(&skills, "hidden", "Hidden skill", "Hidden");
    t.push(replies);
    let loaded = t.loaded();
    let root = loaded.root.clone();
    let mut s = Session::new(
        &loaded,
        root,
        Options {
            agent: Some("x".into()),
            model: None,
            resume: None,
            title: Some("Test".into()),
            parent: None,
            questioner: None,
            mcp: None,
        },
        Arc::new(codesmith_core::permission::Fixed(
            codesmith_core::permission::Reply::Once,
        )),
        None,
    )
    .unwrap();
    s.prompt("Hello", &mut Rec(t.seen.clone())).await.unwrap();
    let body = t.requests()[0].clone();
    (t, s, body)
}

fn listed(body: &Value) -> Vec<String> {
    let text = body["messages"].to_string();
    ["effect", "deploy", "hidden"]
        .into_iter()
        .filter(|id| text.contains(&format!("<id>{id}</id>")))
        .map(String::from)
        .collect()
}

/// "renders described agent skills and updates the complete available
/// list", "filters values by agent permissions", "hides and refuses denied
/// skills while organization policy overrides local allows" (adapted: the
/// agent's rules, not an organization policy), "omits instructions when
/// the selected agent denies all skills", "... when a resource-specific
/// denial follows the global denial", "retains specifically allowed
/// skills after a global denial" and "omits instructions when a
/// specifically allowed skill is denied again". Fixed: the list offered
/// every discovered skill whatever the agent's rules.
#[tokio::test(flavor = "multi_thread")]
async fn oc_skill_list_follows_the_agents_skill_rules() {
    let rule = |r: &str, e: &str| json!({"action": "skill", "resource": r, "effect": e});
    let cases: Vec<(Value, &[&str])> = vec![
        (json!([]), &["effect", "deploy", "hidden"]),
        (json!([rule("deploy", "deny")]), &["effect", "hidden"]),
        (json!([rule("*", "deny")]), &[]),
        (json!([rule("*", "deny"), rule("hidden", "deny")]), &[]),
        (
            json!([rule("*", "deny"), rule("effect", "allow")]),
            &["effect"],
        ),
        (
            json!([
                rule("*", "deny"),
                rule("effect", "allow"),
                rule("effect", "deny")
            ]),
            &[],
        ),
    ];
    for (permissions, want) in cases {
        let (_t, _s, body) = first_request(permissions.clone(), vec![text("ok")]).await;
        assert_eq!(listed(&body), want, "{permissions}");
        if want.is_empty() {
            assert!(
                !body.to_string().contains("<available_skills>"),
                "{permissions}"
            );
        }
    }
}

/// "refuses a policy-blocked skill before authorization or content
/// preparation" and "hides and refuses denied skills..." (the refusal):
/// a denied skill's content is never loaded.
#[tokio::test(flavor = "multi_thread")]
async fn oc_skill_tool_refuses_a_denied_skill_before_loading_it() {
    let (t, s, _) = first_request(
        json!([{"action": "skill", "resource": "deploy", "effect": "deny"}]),
        vec![call("c1", "skill", json!({"id": "deploy"})), text("done")],
    )
    .await;
    let r = results(&s);
    assert_eq!(r.len(), 1);
    assert!(r[0].2, "{r:?}");
    assert!(!r[0].1.contains("Deploy carefully"), "{r:?}");
    drop(t);
}

/// "lists available skills, authorizes the selected ID, and loads
/// model-facing content"
#[tokio::test(flavor = "multi_thread")]
async fn oc_skill_tool_lists_skills_and_loads_the_chosen_ones_content() {
    let (t, s, body) = first_request(
        json!([]),
        vec![call("c1", "skill", json!({"id": "effect"})), text("done")],
    )
    .await;
    let tool = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["function"]["name"] == "skill")
        .unwrap()
        .to_string();
    assert!(tool.contains("effect") && tool.contains("deploy"), "{tool}");
    let r = results(&s);
    assert_eq!(r.len(), 1);
    assert!(!r[0].2 && r[0].1.contains("Use Effect"), "{r:?}");
    drop(t);
}
