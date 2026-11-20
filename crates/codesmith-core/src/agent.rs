//! Agents: a prompt, a model and permission rules. `build` and `plan` are
//! built in (ported from OpenCode); `agents` in config adds or overrides.

use crate::config::Config;
use crate::permission::{base_rules, evaluate, Effect, Rule};
use crate::tool::{Tool, ALL};

#[derive(Debug, Clone, PartialEq)]
pub struct Agent {
    pub id: String,
    pub name: String,
    pub description: String,
    /// `primary` agents are chosen by the user; `subagent`s are delegated to.
    pub mode: String,
    pub system: Option<String>,
    pub model: Option<String>,
    pub steps: Option<u32>,
    pub permissions: Vec<Rule>,
}

pub const PROMPT_EXPLORE: &str =
    "You are a file search specialist. You excel at thoroughly navigating and exploring codebases.

Guidelines:
- Your role is EXCLUSIVELY to search and analyze
- Parallelize independent tool calls for searches and reads whenever possible
- Adapt your search approach based on the thoroughness level specified by the caller
- Return file paths as absolute paths in your final response
- You MUST NOT create, modify, delete, move, or copy files, including temporary files and reports
- Shell commands MUST be read-only. NEVER run commands that write files or change system state

Complete the user's search request efficiently and report your findings clearly.";

pub const PLAN_REMINDER: &str = "<system-reminder>\nPlan mode is active. You are in a read-only planning phase: investigate, ask questions, and propose a plan. Do not modify files, change configuration, or run commands that change system state. The edit and write tools are disabled.\n</system-reminder>";

/// Shell commands the review agent may run. No `git grep` (`-O<cmd>` runs a
/// program, and option bundling defeats a pattern for it); `grep` does that.
const REVIEW_SHELL: &[&str] = &[
    "git diff *",
    "git show *",
    "git log *",
    "git blame *",
    "git status *",
    "git ls-files *",
    "git rev-parse *",
    "git merge-base *",
    "ls *",
    "wc *",
];

/// Shell commands the explore agent may run: the review set plus searching.
const EXPLORE_SHELL: &[&str] = &[
    "git diff *",
    "git show *",
    "git log *",
    "git blame *",
    "git status *",
    "git ls-files *",
    "git rev-parse *",
    "git merge-base *",
    "git branch",
    "git branch --list *",
    "ls *",
    "wc *",
    "pwd",
    "cat *",
    "head *",
    "tail *",
    "grep *",
    "rg *",
    "find *",
    "tree *",
    "file *",
    "stat *",
    "du *",
    "which *",
    "basename *",
    "dirname *",
    "realpath *",
    "sort *",
    "cut *",
    "tr *",
];

/// Options that make an allowed read-only command run a program or write a
/// file: git's `--output` (`--ou` abbreviates it), `--ext-diff`,
/// `--textconv`, `--no-index` (reads outside the repository), `-c`/`-C`/
/// `--config-env` overrides; `find -exec`/`-delete`/`-fprint`; `rg --pre`;
/// `tree -o`; `file -C`; `sort -o`/`--compress-program`; `tail -f` (never
/// ends). Rules match the command
/// with quotes removed too, so `'--output'` is caught.
const READ_ONLY_DENIES: &[&str] = &[
    "git -*",
    "git * --ou*",
    "git * --ext*",
    "git * --textc*",
    "git * --no-index*",
    "git * --exec*",
    "git * -O*",
    "git * --open*",
    "find * -exec*",
    "find * -ok*",
    "find * -delete*",
    "find * -fprint*",
    "find * -fls*",
    "rg * --pre*",
    "tree * -o*",
    "file * -C*",
    "file * --compile*",
    "tail * -f*",
    "tail * -F*",
    "tail * --follow*",
    "sort * -o*",
    "sort * --o*",
    "sort * --compress*",
];

fn read_only_shell(rules: &mut Vec<Rule>, allowed: &[&str]) {
    for cmd in allowed {
        rules.push(Rule::new("shell", cmd, Effect::Allow));
    }
    for cmd in READ_ONLY_DENIES {
        rules.push(Rule::new("shell", cmd, Effect::Deny));
        // `find * -exec*` needs a word before the option; cover it first too.
        if let Some((head, option)) = cmd.split_once(" * ") {
            rules.push(Rule::new(
                "shell",
                &format!("{head} {option}"),
                Effect::Deny,
            ));
        }
    }
}

fn builtin(id: &str) -> Option<Agent> {
    let mut a = Agent {
        id: id.to_string(),
        name: String::new(),
        description: String::new(),
        mode: "primary".into(),
        system: None,
        model: None,
        steps: None,
        // Only the agent's own rules; `resolve` adds the base and config.
        permissions: Vec::new(),
    };
    match id {
        "build" => {
            a.name = "Build".into();
            a.description =
                "The default agent. Executes tools based on configured permissions.".into();
            a.permissions
                .push(Rule::new("question", "*", Effect::Allow));
        }
        "plan" => {
            a.name = "Plan".into();
            a.description = "Read-only planning agent. Investigates and proposes changes without editing files.".into();
            a.permissions
                .push(Rule::new("question", "*", Effect::Allow));
            a.permissions.push(Rule::new("edit", "*", Effect::Deny));
        }
        "general" => {
            a.name = "General".into();
            a.description = "General-purpose agent for researching complex questions and executing multi-step tasks. Use this agent to execute multiple units of work in parallel.".into();
            a.mode = "subagent".into();
            a.permissions.push(Rule::new("question", "*", Effect::Deny));
            a.permissions.push(Rule::new("subagent", "*", Effect::Deny));
        }
        "explore" => {
            a.name = "Explore".into();
            a.description = "Fast agent specialized for exploring codebases. Use this when you need to quickly find files by patterns (eg. \"src/components/**/*.tsx\"), search code for keywords (eg. \"API endpoints\"), or answer questions about the codebase (eg. \"how do API endpoints work?\"). When calling this agent, specify the desired thoroughness level: \"quick\" for basic searches, \"medium\" for moderate exploration, or \"very thorough\" for comprehensive analysis across multiple locations and naming conventions.".into();
            a.mode = "subagent".into();
            a.system = Some(PROMPT_EXPLORE.into());
            a.permissions.extend([
                Rule::new("*", "*", Effect::Deny),
                Rule::new("grep", "*", Effect::Allow),
                Rule::new("glob", "*", Effect::Allow),
                Rule::new("webfetch", "*", Effect::Allow),
                Rule::new("websearch", "*", Effect::Allow),
                Rule::new("read", "*", Effect::Allow),
                Rule::new("read", "*.env", Effect::Ask),
                Rule::new("read", "*.env.*", Effect::Ask),
                Rule::new("read", "*.env.example", Effect::Allow),
                Rule::new("subagent", "*", Effect::Deny),
                Rule::new("external_directory", "*", Effect::Allow),
            ]);
            // OpenCode allows every shell command here and relies on the
            // prompt; CodeSmith allows read-only ones (writes through
            // redirects are `edit`, which stays denied).
            read_only_shell(&mut a.permissions, EXPLORE_SHELL);
        }
        "review" => {
            a.name = "Review".into();
            a.description = "Read-only code reviewer: reads files and runs read-only git commands to check one aspect of a change, then reports findings with evidence. Used by /review and /calibrate.".into();
            a.mode = "subagent".into();
            a.system = Some(PROMPT_EXPLORE.into());
            a.permissions.extend([
                Rule::new("*", "*", Effect::Deny),
                Rule::new("read", "*", Effect::Allow),
                Rule::new("read", "*.env", Effect::Deny),
                Rule::new("read", "*.env.*", Effect::Deny),
                Rule::new("read", "*.env.example", Effect::Allow),
                Rule::new("grep", "*", Effect::Allow),
                Rule::new("glob", "*", Effect::Allow),
                Rule::new("external_directory", "*", Effect::Deny),
            ]);
            read_only_shell(&mut a.permissions, REVIEW_SHELL);
        }
        _ => return None,
    }
    Some(a)
}

pub const BUILTIN: [&str; 5] = ["build", "plan", "general", "explore", "review"];

/// Agents that can be delegated to, with descriptions.
pub fn subagents(config: &Config) -> Vec<(String, String)> {
    list(config)
        .into_iter()
        .filter_map(|id| resolve(config, &id))
        .filter(|a| a.mode == "subagent" || a.mode == "all")
        .map(|a| (a.id, a.description))
        .collect()
}

/// Resolve an agent: built-in definition (if any) overlaid by config.
///
/// Rules, in order (the last match decides): the base defaults, config-wide
/// `permissions`, the built-in agent's own rules, the agent's configured
/// `agents.<id>.permissions`, then [`Config::overrides`] (unattended
/// automation runs). Built-in agent rules are marked [`Rule::builtin`], so
/// they only restrict: a config-wide allow never widens plan, explore or
/// review, while a config-wide deny or ask still applies to them. Rules
/// configured for the agent itself override both.
pub fn resolve(config: &Config, id: &str) -> Option<Agent> {
    let cfg = config.agents.get(id);
    if cfg.map(|c| c.disabled).unwrap_or(false) {
        return None;
    }
    let mut agent = match (builtin(id), cfg) {
        (Some(a), _) => a,
        (None, Some(_)) => Agent {
            id: id.into(),
            name: id.into(),
            description: String::new(),
            mode: "primary".into(),
            system: None,
            model: None,
            steps: None,
            permissions: Vec::new(),
        },
        (None, None) => return None,
    };
    let own = std::mem::take(&mut agent.permissions);
    agent.permissions = base_rules();
    agent.permissions.extend(config.permissions.iter().cloned());
    agent
        .permissions
        .extend(own.into_iter().map(Rule::into_builtin));
    if let Some(c) = cfg {
        if let Some(d) = &c.description {
            agent.description = d.clone();
        }
        if let Some(m) = &c.mode {
            agent.mode = m.clone();
        }
        agent.system = c.system.clone().or(agent.system);
        agent.model = c.model.clone().or(agent.model);
        agent.steps = c.steps.or(agent.steps);
        agent.permissions.extend(c.permissions.iter().cloned());
    }
    agent.permissions.extend(config.overrides.iter().cloned());
    Some(agent)
}

/// All agent ids, built-in first.
pub fn list(config: &Config) -> Vec<String> {
    let mut ids: Vec<String> = BUILTIN.iter().map(|s| s.to_string()).collect();
    for k in config.agents.keys() {
        if !ids.contains(k) {
            ids.push(k.clone());
        }
    }
    ids.retain(|id| resolve(config, id).is_some());
    ids
}

/// Tools the agent may use at all: a tool is hidden only when every use of
/// its action is denied.
pub fn tools(agent: &Agent) -> Vec<Tool> {
    ALL.into_iter()
        .filter(|t| {
            let action = t.action();
            let blanket_deny = evaluate(action, "*", &agent.permissions) == Effect::Deny;
            // A narrower rule that allows (or asks) and is not overridden.
            let any_allowed = agent.permissions.iter().any(|r| {
                r.effect != Effect::Deny
                    && r.action != "*"
                    && crate::wildcard::matches(action, &r.action, false)
                    && r.resource != "*"
                    && evaluate(action, &r.resource, &agent.permissions) != Effect::Deny
            });
            !blanket_deny || any_allowed
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(v: serde_json::Value) -> Config {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn builtins() {
        let c = Config::default();
        let build = resolve(&c, "build").unwrap();
        assert_eq!(build.name, "Build");
        assert_eq!(evaluate("edit", "a.rs", &build.permissions), Effect::Allow);
        let plan = resolve(&c, "plan").unwrap();
        assert_eq!(evaluate("edit", "a.rs", &plan.permissions), Effect::Deny);
        assert_eq!(evaluate("read", "a.rs", &plan.permissions), Effect::Allow);
        assert!(resolve(&c, "nope").is_none());
        assert_eq!(
            list(&c),
            vec!["build", "plan", "general", "explore", "review"]
        );
        let rv = resolve(&c, "review").unwrap();
        for (action, res, want) in [
            ("read", "src/a.rs", Effect::Allow),
            ("read", ".env", Effect::Deny),
            ("edit", "a.rs", Effect::Deny),
            ("write", "a.rs", Effect::Deny),
            ("shell", "git diff HEAD", Effect::Allow),
            ("shell", "git log", Effect::Allow),
            ("shell", "git push origin main", Effect::Deny),
            ("shell", "rm -rf x", Effect::Deny),
            ("subagent", "general", Effect::Deny),
            // Options that run programs or write files.
            ("shell", "git grep -O'touch /tmp/p' foo", Effect::Deny),
            ("shell", "git grep foo", Effect::Deny),
            ("shell", "git diff --output=~/.zshrc", Effect::Deny),
            ("shell", "git log --output=/tmp/x", Effect::Deny),
            ("shell", "git log -p --out=/tmp/x", Effect::Deny),
            ("shell", "git diff --ext-diff", Effect::Deny),
            ("shell", "git show --textconv HEAD", Effect::Deny),
            ("shell", "git -c core.pager=sh log", Effect::Deny),
            ("shell", "git diff --no-index /etc/passwd x", Effect::Deny),
            ("shell", "git log --oneline -5", Effect::Allow),
            ("edit", "/Users/x/.zshrc", Effect::Deny),
        ] {
            assert_eq!(
                evaluate(action, res, &rv.permissions),
                want,
                "{action} {res}"
            );
        }
        assert_eq!(
            subagents(&c)
                .iter()
                .map(|s| s.0.as_str())
                .collect::<Vec<_>>(),
            ["general", "explore", "review"]
        );
    }

    #[test]
    fn explore_shell_is_read_only() {
        let ex = resolve(&Config::default(), "explore").unwrap();
        for (res, want) in [
            ("git log --oneline", Effect::Allow),
            ("rg -n foo src", Effect::Allow),
            ("find . -name '*.rs'", Effect::Allow),
            ("cat README.md", Effect::Allow),
            ("find . -exec touch {} ;", Effect::Deny),
            ("find . -delete", Effect::Deny),
            ("rg --pre ./evil foo", Effect::Deny),
            ("find -exec touch {} ;", Effect::Deny),
            ("tail -f log", Effect::Deny),
            ("tail my-file.txt", Effect::Allow),
            ("sort -o out in", Effect::Deny),
            ("tree -o out", Effect::Deny),
            ("touch x", Effect::Deny),
            ("rm -rf x", Effect::Deny),
            ("npm install", Effect::Deny),
            ("git grep -Ocmd x", Effect::Deny),
        ] {
            assert_eq!(evaluate("shell", res, &ex.permissions), want, "{res}");
        }
        assert_eq!(evaluate("edit", "a.rs", &ex.permissions), Effect::Deny);
    }

    #[test]
    fn tool_visibility() {
        let c = Config::default();
        let names = |a: &Agent| tools(a).iter().map(|t| t.name()).collect::<Vec<_>>();
        assert_eq!(
            names(&resolve(&c, "build").unwrap()),
            vec![
                "read",
                "write",
                "edit",
                "patch",
                "glob",
                "grep",
                "shell",
                "webfetch",
                "websearch",
                "todowrite",
                "subagent",
                "skill",
                "question",
                "lsp"
            ]
        );
        assert_eq!(
            names(&resolve(&c, "plan").unwrap()),
            vec![
                "read",
                "glob",
                "grep",
                "shell",
                "webfetch",
                "websearch",
                "todowrite",
                "subagent",
                "skill",
                "question",
                "lsp"
            ]
        );
        assert_eq!(
            names(&resolve(&c, "explore").unwrap()),
            vec!["read", "glob", "grep", "shell", "webfetch", "websearch"]
        );
        assert_eq!(
            names(&resolve(&c, "general").unwrap()),
            vec![
                "read",
                "write",
                "edit",
                "patch",
                "glob",
                "grep",
                "shell",
                "webfetch",
                "websearch",
                "todowrite",
                "skill",
                "lsp"
            ]
        );
        let c = cfg(
            json!({"agents": {"plan": {"permissions": [{"action": "edit", "resource": "docs/*", "effect": "allow"}]}}}),
        );
        assert!(
            names(&resolve(&c, "plan").unwrap()).contains(&"edit"),
            "a narrower allow keeps the tool"
        );
    }

    #[test]
    fn config_overrides_and_custom_agents() {
        let c = cfg(json!({
            "permissions": [{"action": "shell", "resource": "rm *", "effect": "deny"}],
            "agents": {
                "build": {"model": "omlx/default", "steps": 20},
                "reviewer": {"description": "Reviews", "system": "You review code.", "permissions": [{"action": "edit", "resource": "*", "effect": "deny"}]},
                "plan": {"disabled": true}
            }
        }));
        let b = resolve(&c, "build").unwrap();
        assert_eq!(b.model.as_deref(), Some("omlx/default"));
        assert_eq!(b.steps, Some(20));
        assert_eq!(
            evaluate("shell", "rm -rf x", &b.permissions),
            Effect::Deny,
            "project rules apply to every agent"
        );
        let r = resolve(&c, "reviewer").unwrap();
        assert_eq!(r.system.as_deref(), Some("You review code."));
        assert_eq!(evaluate("edit", "x", &r.permissions), Effect::Deny);
        assert!(resolve(&c, "plan").is_none());
        assert_eq!(
            list(&c),
            vec!["build", "general", "explore", "review", "reviewer"]
        );
    }

    #[test]
    fn config_wide_rules_do_not_widen_built_in_agents() {
        let c = cfg(json!({"permissions": [
            {"action": "edit", "resource": "*", "effect": "allow"},
            {"action": "shell", "resource": "*", "effect": "allow"},
            {"action": "*", "resource": "*", "effect": "allow"}
        ]}));
        for id in ["plan", "review", "explore"] {
            let a = resolve(&c, id).unwrap();
            assert_eq!(
                evaluate("edit", "src/a.rs", &a.permissions),
                Effect::Deny,
                "{id} edit"
            );
        }
        for id in ["review", "explore"] {
            let a = resolve(&c, id).unwrap();
            for cmd in ["rm -rf x", "git push origin main", "find . -delete"] {
                assert_eq!(
                    evaluate("shell", cmd, &a.permissions),
                    Effect::Deny,
                    "{id} {cmd}"
                );
            }
            assert_eq!(evaluate("shell", "git log", &a.permissions), Effect::Allow);
            assert_eq!(evaluate("webfetch", "https://x", &a.permissions), {
                if id == "explore" {
                    Effect::Allow
                } else {
                    Effect::Deny
                }
            });
        }
        let names = |a: &Agent| tools(a).iter().map(|t| t.name()).collect::<Vec<_>>();
        assert!(!names(&resolve(&c, "plan").unwrap()).contains(&"edit"));
        let narrow = cfg(json!({"permissions": [
            {"action": "edit", "resource": "docs/*", "effect": "allow"}
        ]}));
        let plan = resolve(&narrow, "plan").unwrap();
        assert_eq!(
            evaluate("edit", "docs/a.md", &plan.permissions),
            Effect::Deny
        );
        assert!(
            !names(&plan).contains(&"edit"),
            "a global allow the built-in deny overrides does not show the tool"
        );
        // The build agent is unrestricted, so global rules still decide.
        let b = resolve(&c, "build").unwrap();
        assert_eq!(evaluate("read", ".env", &b.permissions), Effect::Allow);
    }

    #[test]
    fn config_wide_restrictions_still_reach_built_in_agents() {
        let c = cfg(json!({"permissions": [
            {"action": "read", "resource": "secrets/*", "effect": "deny"},
            {"action": "shell", "resource": "git log*", "effect": "ask"},
            {"action": "question", "resource": "*", "effect": "deny"},
            {"action": "shell", "resource": "*", "effect": "deny"},
            {"action": "shell", "resource": "git status*", "effect": "allow"}
        ]}));
        let ex = resolve(&c, "explore").unwrap();
        assert_eq!(
            evaluate("read", "secrets/key", &ex.permissions),
            Effect::Deny
        );
        assert_eq!(evaluate("read", "src/a.rs", &ex.permissions), Effect::Allow);
        assert_eq!(evaluate("shell", "git log", &ex.permissions), Effect::Deny);
        assert_eq!(
            evaluate("shell", "git status", &ex.permissions),
            Effect::Allow,
            "a later global allow still lifts a global deny"
        );
        let rv = resolve(&c, "review").unwrap();
        assert_eq!(
            evaluate("read", "secrets/key", &rv.permissions),
            Effect::Deny
        );
        let b = resolve(&c, "build").unwrap();
        assert_eq!(evaluate("question", "*", &b.permissions), Effect::Deny);
    }

    #[test]
    fn agent_configured_rules_override_global_and_built_in_ones() {
        let c = cfg(json!({
            "permissions": [
                {"action": "edit", "resource": "*", "effect": "deny"},
                {"action": "read", "resource": "secrets/*", "effect": "deny"}
            ],
            "agents": {
                "plan": {"permissions": [{"action": "edit", "resource": "docs/*", "effect": "allow"}]},
                "explore": {"permissions": [
                    {"action": "read", "resource": "secrets/public/*", "effect": "allow"},
                    {"action": "shell", "resource": "cargo tree*", "effect": "allow"}
                ]},
                "build": {"permissions": [{"action": "edit", "resource": "*", "effect": "allow"}]}
            }
        }));
        let plan = resolve(&c, "plan").unwrap();
        assert_eq!(
            evaluate("edit", "docs/a.md", &plan.permissions),
            Effect::Allow
        );
        assert_eq!(
            evaluate("edit", "src/a.rs", &plan.permissions),
            Effect::Deny
        );
        let ex = resolve(&c, "explore").unwrap();
        assert_eq!(
            evaluate("read", "secrets/public/a", &ex.permissions),
            Effect::Allow
        );
        assert_eq!(evaluate("read", "secrets/a", &ex.permissions), Effect::Deny);
        assert_eq!(
            evaluate("shell", "cargo tree", &ex.permissions),
            Effect::Allow
        );
        let b = resolve(&c, "build").unwrap();
        assert_eq!(evaluate("edit", "src/a.rs", &b.permissions), Effect::Allow);
    }
}
