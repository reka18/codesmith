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

fn builtin(id: &str) -> Option<Agent> {
    let mut a = Agent {
        id: id.to_string(),
        name: String::new(),
        description: String::new(),
        mode: "primary".into(),
        system: None,
        model: None,
        steps: None,
        permissions: base_rules(),
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
                Rule::new("shell", "*", Effect::Allow),
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
        }
        _ => return None,
    }
    Some(a)
}

pub const BUILTIN: [&str; 4] = ["build", "plan", "general", "explore"];

/// Agents that can be delegated to, with descriptions.
pub fn subagents(config: &Config) -> Vec<(String, String)> {
    list(config)
        .into_iter()
        .filter_map(|id| resolve(config, &id))
        .filter(|a| a.mode == "subagent" || a.mode == "all")
        .map(|a| (a.id, a.description))
        .collect()
}

/// Resolve an agent: built-in definition (if any) overlaid by config. Config
/// rules come after the built-in ones and project-wide rules after those, so
/// they take precedence.
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
            permissions: base_rules(),
        },
        (None, None) => return None,
    };
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
    agent.permissions.extend(config.permissions.iter().cloned());
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
            let any_allowed = agent.permissions.iter().any(|r| {
                r.effect != Effect::Deny
                    && r.action != "*"
                    && crate::wildcard::matches(action, &r.action, false)
                    && r.resource != "*"
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
        assert_eq!(list(&c), vec!["build", "plan", "general", "explore"]);
        assert_eq!(
            subagents(&c)
                .iter()
                .map(|s| s.0.as_str())
                .collect::<Vec<_>>(),
            ["general", "explore"]
        );
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
                "glob",
                "grep",
                "shell",
                "webfetch",
                "websearch",
                "todowrite",
                "subagent",
                "skill",
                "question"
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
                "question"
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
                "glob",
                "grep",
                "shell",
                "webfetch",
                "websearch",
                "todowrite",
                "skill"
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
        assert_eq!(list(&c), vec!["build", "general", "explore", "reviewer"]);
    }
}
