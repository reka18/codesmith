//! Delegation and reuse: a subagent (explore) and its report, skills,
//! custom commands, and `/review` against a calibration file.

use crate::harness::{user_texts, Env};
use crate::model::{self, Model, Reply, Script, M};
use crate::need_git;
use codesmith::core::tool::subagent::PREAMBLE;
use serde_json::{json, Value};

#[tokio::test(flavor = "multi_thread")]
async fn the_explore_subagent_searches_and_reports_back() {
    let e = Env::new();
    e.write("src/add.sh", "echo $(($1 + $2))\n");
    let parent = M::tools()
        .and(M::prompt("where is the adder"))
        .and(M::prompt(PREAMBLE).not());
    let child = M::tools()
        .and(M::prompt(PREAMBLE))
        .and(M::system("file search specialist"));
    let m = Model::start(
        Script::new()
            .titled("Find the adder")
            .then(
                "delegate",
                parent.clone().and(M::step(0)),
                Reply::call(
                    "subagent",
                    json!({"agent": "explore", "description": "Find the adder", "prompt": "Find the shell script that adds numbers. quick"}),
                ),
            )
            .then(
                "child-glob",
                child.clone().and(M::step(0)),
                Reply::call("glob", json!({"pattern": "**/*.sh"})),
            )
            .then(
                "child-write",
                child.clone().and(M::step(1)),
                Reply::call("write", json!({"path": "notes.md", "content": "x"})),
            )
            .then(
                "child-answer",
                child.and(M::step(2)),
                Reply::text("It is src/add.sh."),
            )
            .then(
                "answer",
                parent.and(M::step(1)),
                Reply::text("The adder lives in src/add.sh."),
            ),
    )
    .await;
    e.config(&m.url, json!({}));

    let out = e
        .run(&["run", "--format", "json", "where is the adder?"])
        .await;
    out.ok();
    assert_eq!(out.text(), "The adder lives in src/add.sh.");
    let ev = out.events();
    let names: Vec<&str> = ev
        .iter()
        .filter(|e| e["type"] == "tool_call")
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["subagent", "explore › glob", "explore › write"]);
    // The child's own text is not streamed as the parent's answer.
    assert!(!out.text().contains("It is src/add.sh."));

    // The child is the explore agent: its prompt, its tools, its rules.
    let c = &m.by("child-glob")[0];
    assert!(model::system(c).contains("file search specialist"));
    assert!(model::prompt(c).starts_with(PREAMBLE));
    assert!(model::prompt(c).ends_with("Find the shell script that adds numbers. quick"));
    let tools = model::tools(c);
    assert!(tools.contains(&"glob".to_string()) && !tools.contains(&"subagent".to_string()));
    let refused = &model::results(&m.by("child-answer")[0])[1];
    assert!(refused.starts_with("No tool named \"write\""), "{refused}");
    assert!(!e.path("notes.md").exists());
    let found = &model::results(&m.by("child-write")[0])[0];
    assert!(found.contains("src/add.sh"), "{found}");

    // The parent gets the report in its documented shape.
    let sid = out.done()["session"].as_str().unwrap().to_string();
    let db = e.store();
    let kids: Vec<_> = db
        .list(None, true)
        .unwrap()
        .into_iter()
        .filter(|s| s.parent_id.as_deref() == Some(sid.as_str()))
        .collect();
    assert_eq!(kids.len(), 1);
    let kid = &kids[0];
    assert_eq!(
        (kid.agent.as_str(), kid.title.as_deref()),
        ("explore", Some("Find the adder"))
    );
    let report = &model::results(&m.by("answer")[0])[0];
    assert_eq!(
        report,
        &format!(
            "<subagent sessionID=\"{}\" state=\"completed\" description=\"Find the adder\">\nIt is src/add.sh.\n</subagent>",
            kid.id
        )
    );
    // The parent's usage includes the child's three steps (and its own
    // title call).
    assert_eq!(out.done()["usage"]["input"], 600);
    // Children stay out of the default listing.
    assert_eq!(db.list(None, false).unwrap().len(), 1);
    let stats = e.run(&["stats", "--all", "--json"]).await;
    let s: Value = serde_json::from_str(&stats.stdout).unwrap();
    assert_eq!(
        (&s["sessions"], &s["subagents"]),
        (&json!(1), &json!(1)),
        "{s:#}"
    );
    m.assert_done();
}

#[tokio::test(flavor = "multi_thread")]
async fn skills_load_on_demand() {
    let e = Env::new();
    e.write(
        ".codesmith/skills/release/SKILL.md",
        "---\nname: release\ndescription: How to cut a release of this project\n---\nRun ./release.sh --dry-run before tagging.\n",
    );
    // A skill under the real home would leak in; this one is the temp home's.
    std::fs::create_dir_all(e.home.join(".claude/skills/home-skill")).unwrap();
    std::fs::write(
        e.home.join(".claude/skills/home-skill/SKILL.md"),
        "---\nname: home-skill\ndescription: A skill from the home folder\n---\nHome.\n",
    )
    .unwrap();
    let ask = M::tools().and(M::prompt("cut a release"));
    let m = Model::start(
        Script::new()
            .titled("Release")
            .then(
                "load",
                ask.clone().and(M::step(0)),
                Reply::call("skill", json!({"id": "release"})),
            )
            .then(
                "answer",
                ask.and(M::step(1)),
                Reply::text("First a dry run."),
            ),
    )
    .await;
    e.config(&m.url, json!({}));
    let out = e.run(&["run", "--format", "json", "cut a release"]).await;
    out.ok();
    assert_eq!(out.text(), "First a dry run.");
    let first = &m.by("load")[0];
    let skill_tool = first["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["function"]["name"] == "skill")
        .unwrap()
        .to_string();
    let listing = format!("{}{skill_tool}", model::system(first));
    assert!(
        listing.contains("How to cut a release of this project"),
        "{listing}"
    );
    assert!(listing.contains("home-skill"));
    let loaded = &model::results(&m.by("answer")[0])[0];
    assert!(
        loaded.contains("Run ./release.sh --dry-run before tagging."),
        "{loaded}"
    );
    m.assert_done();
}

#[tokio::test(flavor = "multi_thread")]
async fn custom_commands_expand_and_pick_their_agent() {
    let e = Env::new();
    e.write(
        ".codesmith/commands/explain.md",
        "---\ndescription: Explain a file\nagent: plan\n---\nExplain $1 to a $2 reader.\n",
    );
    let m = Model::start(
        Script::new()
            .titled("Explain")
            .then(
                "explain",
                M::tools().and(M::prompt("Explain add.sh to a new reader.")),
                Reply::text("It adds."),
            )
            .then(
                "inline",
                M::tools().and(M::prompt("Say hello to Ada")),
                Reply::text("Hello, Ada."),
            ),
    )
    .await;
    e.config(
        &m.url,
        json!({"commands": {"greet": {"template": "Say hello to $ARGUMENTS", "description": "Greet someone"}}}),
    );
    let list = e.run(&["commands"]).await;
    list.ok();
    for want in [
        "/explain",
        "Explain a file",
        "/greet",
        "Greet someone",
        "/review",
        "/init",
    ] {
        assert!(list.stdout.contains(want), "{want}: {}", list.stdout);
    }
    let out = e
        .run(&["run", "--format", "json", "/explain add.sh new"])
        .await;
    out.ok();
    assert_eq!(out.text(), "It adds.");
    // The command's agent: plan mode tools and rules.
    let sid = out.done()["session"].as_str().unwrap().to_string();
    assert_eq!(e.store().get(&sid).unwrap().unwrap().agent, "plan");
    // Plan mode is announced before the prompt.
    let texts = user_texts(&e.history(&sid));
    assert_eq!(texts.len(), 2, "{texts:?}");
    assert!(texts[0].contains("You are in Plan mode"), "{texts:?}");
    assert_eq!(texts[1], "Explain add.sh to a new reader.");
    let out = e.run(&["run", "/greet", "Ada"]).await;
    out.ok();
    assert_eq!(out.stdout.trim(), "Hello, Ada.");
    m.assert_done();
}

const CALIBRATION: &str = r#"{
  "version": 1,
  "generated": "2026-11-01T00:00:00Z",
  "summary": "A tiny shell project.",
  "dimensions": [
    {"id": "shell-safety", "name": "Shell safety", "focus": "Quoting and word splitting in shell scripts.", "paths": ["*.sh"], "checks": ["Arguments are quoted"]},
    {"id": "docs", "name": "Docs", "focus": "Markdown accuracy.", "paths": ["docs/**"]}
  ]
}
"#;

fn json_answer(v: Value) -> Reply {
    Reply::text(&format!("Here is my answer.\n\n```json\n{v}\n```"))
}

#[tokio::test(flavor = "multi_thread")]
async fn review_runs_the_calibrated_dimensions_and_verifies_findings() {
    need_git!();
    let e = Env::new();
    e.write("add.sh", "echo $(($1 + $2))\n");
    e.write(".codesmith/review.json", CALIBRATION);
    e.commit("add calculator");
    e.write("add.sh", "x=$1\necho $(($x + $2))\nrm -rf $3\n");
    let m = Model::start(
        Script::new()
            .titled("Review")
            .then(
                "correctness",
                M::tools().and(M::prompt("## Dimension: Correctness (`correctness`)")),
                json_answer(json!({"findings": []})),
            )
            .then(
                "shell-reviewer-reads",
                M::tools()
                    .and(M::prompt("## Dimension: Shell safety (`shell-safety`)"))
                    .and(M::step(0)),
                Reply::call("shell", json!({"command": "git diff -- add.sh", "description": "diff"})),
            )
            .then(
                "shell-reviewer-tries-to-fix",
                M::tools()
                    .and(M::prompt("## Dimension: Shell safety (`shell-safety`)"))
                    .and(M::step(1)),
                Reply::call("edit", json!({"path": "add.sh", "oldString": "rm -rf $3", "newString": "rm -rf \"$3\""})),
            )
            .then(
                "shell-safety",
                M::tools()
                    .and(M::prompt("## Dimension: Shell safety (`shell-safety`)"))
                    .and(M::step(2)),
                json_answer(json!({"findings": [
                    {"file": "add.sh", "line": 3, "severity": "high", "title": "Unquoted rm target deletes the wrong paths", "detail": "rm -rf $3 splits words.", "failure_scenario": "An argument with a space removes two paths."},
                    {"file": "add.sh", "line": 1, "severity": "low", "title": "Needless temporary variable x", "detail": "", "failure_scenario": ""}
                ]})),
            )
            .then(
                "verify-rm",
                M::tools().and(M::prompt("Unquoted rm target deletes the wrong paths")).and(M::prompt("## Dimension:").not()),
                json_answer(json!({"verdict": "confirmed", "confidence": 92, "reason": "Line 3 passes $3 unquoted."})),
            )
            .then(
                "verify-x",
                M::tools().and(M::prompt("Needless temporary variable x")).and(M::prompt("## Dimension:").not()),
                json_answer(json!({"verdict": "rejected", "confidence": 70, "reason": "Style only."})),
            ),
    )
    .await;
    e.config(&m.url, json!({}));
    let out = e.run(&["review"]).await;
    out.ok();
    let report = &out.stdout;
    assert!(report.contains("## Code review:"), "{}", out.both());
    assert!(
        report.contains(
            "1. **[high] Unquoted rm target deletes the wrong paths** — `add.sh:3` (shell-safety)"
        ),
        "{report}"
    );
    assert!(report.contains("Verified (92% confidence): Line 3 passes $3 unquoted."));
    assert!(!report.contains("Needless temporary variable"));
    assert!(report.contains("2 finding(s) raised, 1 confirmed by independent verification."));
    assert!(!report.contains("/calibrate"), "calibrated: {report}");
    // Only the dimensions that cover the change ran (not docs).
    assert!(
        out.stderr.contains("correctness, shell-safety"),
        "{}",
        out.stderr
    );
    // Reviewers are read-only: the edit was refused.
    assert_eq!(e.read("add.sh"), "x=$1\necho $(($x + $2))\nrm -rf $3\n");
    let after_edit = &m.by("shell-safety")[0];
    let r = model::results(after_edit);
    assert!(
        r[0].contains("rm -rf $3"),
        "the reviewer saw the diff: {r:?}"
    );
    assert!(r[1].starts_with("No tool named \"edit\""), "{r:?}");
    // The reviewer was given the diff and the calibrated checks.
    let sent = model::prompt(&m.by("correctness")[0]);
    assert!(sent.contains("+rm -rf $3"), "{sent}");
    let sent = model::prompt(&m.by("shell-reviewer-reads")[0]);
    assert!(sent.contains("- Arguments are quoted"));
    // The review is stored as an exchange in its own session, with the
    // reviewers and verifiers as its children.
    let db = e.store();
    let top = db.list(None, false).unwrap();
    assert_eq!(top.len(), 1);
    let h = e.history(&top[0].id);
    assert_eq!(user_texts(&h), ["/review"]);
    let kids = db
        .list(None, true)
        .unwrap()
        .into_iter()
        .filter(|s| s.parent_id.as_deref() == Some(top[0].id.as_str()))
        .count();
    assert_eq!(kids, 4);
    m.assert_done();
}
