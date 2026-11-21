//! Agents: a prompt, a model and permission rules. `build`, `plan`,
//! `general`, `explore` and `review` are built in (ported from OpenCode);
//! `agents` in config, and Markdown agent files in config folders
//! (`agents/**/*.md`, `modes/*.md`; see [`documents`]), add or override.
//!
//! Pinned to OpenCode's agent config by the `oc_agent_*` tests in
//! `tests/oc_config.rs` and `tests/oc_agents.rs` (ported from
//! `packages/core/test/agent.test.ts` and `config/agent.test.ts`).

use crate::config::{AgentRequest, Config};
use crate::markdown::Yaml;
use crate::permission::{base_rules, evaluate, Effect, Rule};
use crate::tool::{Tool, ALL};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

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
    /// Left out of the subagent list and never picked as the default.
    pub hidden: bool,
    /// A `#rrggbb` color, if configured.
    pub color: Option<String>,
    /// Request headers and body fields over the model's.
    pub request: AgentRequest,
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

/// The Plan mode reminder for a provider that keeps its own conversation
/// (Claude Agent), which the history's reminders do not reach: the plan
/// plugin's enter reminder.
pub fn plan_reminder() -> String {
    crate::plugin::plan::enter(&crate::plugin::plan::directory())
}

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
        hidden: false,
        color: None,
        request: AgentRequest::default(),
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
            a.description = "Read-only agent for exploring the codebase and planning work before implementation. Writes only plan files, in the Plan directory.".into();
            a.permissions
                .push(Rule::new("question", "*", Effect::Allow));
            a.permissions
                .extend(crate::plugin::plan::rules(&crate::plugin::plan::directory()).own);
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

/// Agents listed for delegation, with descriptions: those that may run as
/// a subagent and are not hidden.
pub fn subagents(config: &Config) -> Vec<(String, String)> {
    list(config)
        .into_iter()
        .filter_map(|id| resolve(config, &id))
        .filter(|a| a.mode != "primary" && !a.hidden)
        .map(|a| (a.id, a.description))
        .collect()
}

/// Whether `id` can run as a subagent: it exists and is not primary-only.
/// A hidden agent is not listed but can still be named.
pub fn delegable(config: &Config, id: &str) -> bool {
    resolve(config, id).is_some_and(|a| a.mode != "primary")
}

/// Whether the user can pick `a` as the session's agent unprompted (as the
/// default, or by cycling): a primary agent that is not hidden.
pub fn selectable(a: &Agent) -> bool {
    a.mode != "subagent" && !a.hidden
}

/// The agent a session starts with when none is named (OpenCode's
/// `selectedDefault`): `default_agent` when it is selectable, else `build`
/// when it is, else the first selectable agent, else `build`.
pub fn default_id(config: &Config) -> String {
    let ok = |id: &str| resolve(config, id).is_some_and(|a| selectable(&a));
    if let Some(d) = config.default_agent.as_deref().filter(|d| ok(d)) {
        return d.to_string();
    }
    if ok("build") {
        return "build".into();
    }
    list(config)
        .into_iter()
        .find(|id| ok(id))
        .unwrap_or_else(|| "build".into())
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
            hidden: false,
            color: None,
            request: AgentRequest::default(),
        },
        (None, None) => return None,
    };
    let own = std::mem::take(&mut agent.permissions);
    agent.permissions = base_rules();
    // The plan agent reaches its Plan directory without asking, unless
    // config says otherwise.
    if id == crate::plugin::plan::AGENT {
        agent
            .permissions
            .extend(crate::plugin::plan::rules(&crate::plugin::plan::directory()).base);
    }
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
        agent.hidden = c.hidden.unwrap_or(agent.hidden);
        agent.color = c.color.clone().or(agent.color);
        agent.request.headers.extend(c.request.headers.clone());
        agent.request.body.extend(c.request.body.clone());
        agent.permissions.extend(c.permissions.iter().cloned());
    }
    agent.permissions.extend(config.overrides.iter().cloned());
    // Policies deny whatever the rules say; they travel with the rules.
    agent
        .permissions
        .extend(config.policies.iter().filter_map(|p| p.rule()));
    Some(agent)
}

/// All agent ids, built-in first.
pub fn list(config: &Config) -> Vec<String> {
    let mut ids: Vec<String> = BUILTIN.iter().map(|s| s.to_string()).collect();
    for k in config.agents.keys() {
        // `agents.title` configures session titles, not an agent.
        if !ids.contains(k) && k != crate::title::AGENT {
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

// --- Markdown agent files ---

/// Folders (under a config folder) whose `**/*.md` files are agents.
const AGENT_DIRS: [&str; 2] = ["agent", "agents"];
/// Folders whose top-level `*.md` files are primary agents (OpenCode's
/// legacy modes).
const MODE_DIRS: [&str; 2] = ["mode", "modes"];
/// Frontmatter fields an agent file has in OpenCode's current format.
const FIELDS: [&str; 11] = [
    "variant",
    "model",
    "request",
    "system",
    "description",
    "mode",
    "hidden",
    "color",
    "steps",
    "disabled",
    "permissions",
];
/// Fields only the current format has: a file with any of them is read in
/// that format, whatever else it contains.
const NATIVE: [&str; 4] = ["permissions", "request", "system", "disabled"];
/// Fields of OpenCode's legacy (V1) agent format; others become request
/// body options.
const LEGACY: [&str; 16] = [
    "name",
    "model",
    "variant",
    "temperature",
    "top_p",
    "prompt",
    "tools",
    "disable",
    "description",
    "mode",
    "hidden",
    "options",
    "color",
    "steps",
    "maxSteps",
    "permission",
];

/// The agents defined by Markdown files in config folder `dir`, as a config
/// layer `{"agents": {...}}`: `agent[s]/**/*.md` (the id is the path
/// below that folder without `.md`, so `agents/team/helper.md` is
/// `team/helper`), then `mode[s]/*.md` as primary agents. Files are read in
/// path order and merge like config layers; empty files, files that do not
/// parse and `node_modules` folders are skipped.
pub fn documents(dir: &Path) -> Value {
    let mut out = json!({});
    let mut files: Vec<(String, PathBuf, bool)> = Vec::new();
    for sub in AGENT_DIRS {
        let base = dir.join(sub);
        let mut found = Vec::new();
        markdown_files(&base, true, &mut found);
        found.sort();
        for p in found {
            if let Some(name) = file_name(&base, &p) {
                files.push((name, p, false));
            }
        }
    }
    for sub in MODE_DIRS {
        let base = dir.join(sub);
        let mut found = Vec::new();
        markdown_files(&base, false, &mut found);
        found.sort();
        for p in found {
            if let Some(name) = file_name(&base, &p) {
                files.push((name, p, true));
            }
        }
    }
    for (name, path, primary) in files {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        if let Some(agent) = from_markdown(&text, primary) {
            crate::config::merge(&mut out, json!({ "agents": { name: agent } }));
        }
    }
    out
}

fn markdown_files(dir: &Path, recurse: bool, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        // `metadata` follows symlinks, as OpenCode's scan does.
        let Ok(meta) = std::fs::metadata(&p) else {
            continue;
        };
        if meta.is_dir() {
            let skip = matches!(
                p.file_name().and_then(|n| n.to_str()),
                Some("node_modules" | ".git")
            );
            if recurse && !skip {
                markdown_files(&p, true, out);
            }
        } else if p.extension().and_then(|x| x.to_str()) == Some("md") {
            out.push(p);
        }
    }
}

fn file_name(base: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(base).ok()?.with_extension("");
    let parts: Vec<&str> = rel
        .components()
        .map(|c| c.as_os_str().to_str())
        .collect::<Option<_>>()?;
    Some(parts.join("/"))
}

/// One agent file as `agents.<id>` config: the body is the agent's prompt.
/// A file in OpenCode's legacy format (any field the current format lacks,
/// such as `temperature`, `tools`, `permission` or `prompt`, and none only
/// the current one has) is converted: `temperature`, `top_p`, `options`
/// and unknown fields become request body fields, `tools` and `permission`
/// maps become rules in source order (`bash` is `shell`, `write`/`patch`
/// are `edit`, `task` is `subagent`), `maxSteps` is `steps`, `disable` is
/// `disabled`. A separate `variant` joins a model that has none. `None`
/// when the frontmatter does not parse or a field has the wrong type.
pub fn from_markdown(text: &str, primary: bool) -> Option<Value> {
    let doc = crate::markdown::parse(text)?;
    let body = doc.content.trim().to_string();
    let data = doc.data;
    let keys: Vec<&str> = data.iter().map(|(k, _)| k.as_str()).collect();
    let unsupported = keys.iter().any(|k| !FIELDS.contains(k));
    let legacy = unsupported && !keys.iter().any(|k| NATIVE.contains(k));
    let get = |k: &str| data.iter().find(|(x, _)| x == k).map(|(_, v)| v);
    let model = match (get("model"), get("variant")) {
        (Some(Yaml::Str(m)), Some(Yaml::Str(v))) if !m.contains('#') && !v.contains('#') => {
            Some(Value::String(format!("{m}#{v}")))
        }
        (Some(m), _) => Some(m.to_json()),
        (None, _) => None,
    };
    let mut agent = if legacy {
        legacy_agent(&data, model)?
    } else {
        let mut m = Map::new();
        for (k, v) in &data {
            if FIELDS.contains(&k.as_str()) && k != "variant" {
                m.insert(k.clone(), v.to_json());
            }
        }
        match model {
            Some(v) => m.insert("model".into(), v),
            None => m.remove("model"),
        };
        m
    };
    if legacy {
        agent.remove("system");
    }
    if !body.is_empty() {
        agent.insert("system".into(), Value::String(body));
    }
    if primary {
        agent.insert("mode".into(), json!("primary"));
    }
    if let Some(mode) = agent.get("mode") {
        if !matches!(mode.as_str(), Some("primary" | "subagent" | "all")) {
            return None;
        }
    }
    let value = Value::Object(agent);
    // Wrong types fail here, as OpenCode's schema decode does.
    serde_json::from_value::<crate::config::AgentConfig>(value.clone()).ok()?;
    Some(value)
}

fn legacy_agent(data: &[(String, Yaml)], model: Option<Value>) -> Option<Map<String, Value>> {
    let get = |k: &str| data.iter().find(|(x, _)| x == k).map(|(_, v)| v);
    let mut body = Map::new();
    if let Some(o) = get("options") {
        let Value::Object(o) = o.to_json() else {
            return None;
        };
        body.extend(o);
    }
    for (k, v) in data {
        if !LEGACY.contains(&k.as_str()) {
            body.insert(k.clone(), v.to_json());
        }
    }
    for k in ["temperature", "top_p"] {
        match get(k) {
            Some(Yaml::Num(n)) => {
                body.insert(k.into(), Yaml::Num(*n).to_json());
            }
            Some(_) => return None,
            None => {}
        }
    }
    // Ordered (key, rule): a later key replaces an earlier one in place.
    let mut permission: Vec<(String, Yaml)> = Vec::new();
    let mut set = |k: String, v: Yaml| match permission.iter_mut().find(|(x, _)| *x == k) {
        Some(e) => e.1 = v,
        None => permission.push((k, v)),
    };
    if let Some(tools) = get("tools") {
        for (tool, enabled) in tools.as_map()? {
            let Yaml::Bool(on) = enabled else {
                return None;
            };
            let key = if tool == "write" || tool == "patch" {
                "edit"
            } else {
                tool
            };
            set(
                key.into(),
                Yaml::Str(if *on { "allow" } else { "deny" }.into()),
            );
        }
    }
    match get("permission") {
        Some(Yaml::Str(effect)) => set("*".into(), Yaml::Str(effect.clone())),
        Some(Yaml::Map(m)) => {
            for (k, v) in m {
                set(k.clone(), v.clone());
            }
        }
        Some(_) => return None,
        None => {}
    }
    let mut rules = Vec::new();
    for (key, rule) in permission {
        let action = match key.as_str() {
            "write" | "patch" | "apply_patch" => "edit",
            "task" => "subagent",
            "bash" => "shell",
            other => other,
        };
        let effect_ok = |e: &str| matches!(e, "allow" | "ask" | "deny");
        match rule {
            Yaml::Str(e) if effect_ok(&e) => {
                rules.push(json!({"action": action, "resource": "*", "effect": e}))
            }
            Yaml::Map(m) => {
                for (resource, e) in m {
                    match e {
                        Yaml::Str(e) if effect_ok(&e) => {
                            rules.push(json!({"action": action, "resource": resource, "effect": e}))
                        }
                        _ => return None,
                    }
                }
            }
            _ => return None,
        }
    }
    let mut out = Map::new();
    if let Some(m) = model {
        out.insert("model".into(), m);
    }
    if !body.is_empty() {
        out.insert("request".into(), json!({ "body": body }));
    }
    for k in ["description", "mode", "hidden"] {
        if let Some(v) = get(k) {
            out.insert(k.into(), v.to_json());
        }
    }
    if let Some(c) = get("color") {
        let c = c.as_str()?;
        let hex = c.len() == 7 && c.starts_with('#');
        out.insert(
            "color".into(),
            json!(if hex { c.to_string() } else { "#aaaaaa".into() }),
        );
    }
    if let Some(steps) = get("steps").or_else(|| get("maxSteps")) {
        out.insert("steps".into(), steps.to_json());
    }
    if let Some(d) = get("disable") {
        out.insert("disabled".into(), d.to_json());
    }
    if !rules.is_empty() {
        out.insert("permissions".into(), Value::Array(rules));
    }
    Some(out)
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
        // Plan writes plan files, so it has the writing tools.
        assert_eq!(
            names(&resolve(&c, "plan").unwrap()),
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
        let narrow = cfg(json!({"permissions": [
            {"action": "edit", "resource": "docs/*", "effect": "allow"}
        ]}));
        let plan = resolve(&narrow, "plan").unwrap();
        assert_eq!(
            evaluate("edit", "docs/a.md", &plan.permissions),
            Effect::Deny
        );
        let names = |a: &Agent| tools(a).iter().map(|t| t.name()).collect::<Vec<_>>();
        assert!(
            !names(&resolve(&c, "review").unwrap()).contains(&"edit"),
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
