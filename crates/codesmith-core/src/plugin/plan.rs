//! The plan plugin, built in (OpenCode's `plugin/plan.ts`, MIT): Plan mode
//! with a Plan directory.
//!
//! The `plan` agent may write plan files only in the Plan directory
//! ([`directory`], `plan` under the data folder), and nowhere else: its
//! rules ([`rules`]) deny `edit` everywhere but there, and, as in OpenCode,
//! let it reach folders outside the project (the Plan directory among them)
//! without an `external_directory` prompt, so it can read code it plans
//! around. Config rules come later and still win, so a configured
//! `external_directory` deny or ask still applies. Nothing creates the
//! directory until a plan file is written.
//!
//! Plan mode is announced in the conversation, not the system prompt, so
//! switching agents keeps the cached prefix: before a request, a session
//! whose agent is `plan` and whose history has no live enter reminder gets
//! one, and a session that left Plan mode gets a leave reminder ([`missing`]).
//! The session saves the reminder in its history, so compaction or an undo
//! that drops it is reconciled on the next request. Only a message that is
//! exactly a reminder counts: one quoted inside a larger message (a
//! compaction summary, say) does not.
//!
//! A blocked `edit`, `write` or `patch` call by the plan agent is reported
//! as a write outside the Plan directory, naming it ([`rewrite_blocked`]).
//!
//! Turn it off with `"plugins": {"plan": false}`: no reminders, and denied
//! writes report the plain permission error. The plan agent's rules stay.

use super::{Plugin, ToolCall};
use crate::permission::{Effect, Rule};
use codesmith_ai::{Message, Part, Role};
use std::path::{Path, PathBuf};

/// The plugin's id, in config.
pub const ID: &str = "plan";
/// The agent Plan mode belongs to.
pub const AGENT: &str = "plan";

/// The reminder that ends Plan mode.
pub const LEAVE: &str = "<system-reminder>\nYou are NO LONGER in Plan mode. The previous Plan restrictions no longer apply. Any Plan mode instructions from earlier in this conversation are no longer active.\n</system-reminder>";

/// The tools that write files, whose blocked failures name the Plan directory.
const MUTATIONS: [&str; 3] = ["edit", "write", "patch"];

/// Where the plan agent writes plan files: `plan` under the data folder
/// (OpenCode's `~/.opencode/plan`).
pub fn directory() -> PathBuf {
    crate::paths::data_dir().join("plan")
}

/// The reminder that starts Plan mode, naming the Plan directory `dir`.
pub fn enter(dir: &Path) -> String {
    format!(
        "<system-reminder>\nYou are in Plan mode. Discuss the plan with the user directly in the conversation. Do not create or update plan files unless the user explicitly asks you to; when they do, write them only in:\n{}\n\nDo not modify any other files or ask a subagent to do so.\n\nYou remain in Plan mode until the user switches agents. If the user asks you to implement changes, do not do so. Tell them they need to switch agents.\n</system-reminder>",
        dir.display()
    )
}

/// The names the rules use for `dir`: as given, and resolved when that
/// differs (a symlinked data folder), resolving the part that exists so a
/// directory not yet created resolves too.
fn names(dir: &Path) -> Vec<String> {
    let mut out = vec![dir.display().to_string()];
    let mut rest = Vec::new();
    let mut at = dir;
    let real = loop {
        if let Ok(real) = at.canonicalize() {
            break Some(rest.iter().rev().fold(real, |p: PathBuf, c| p.join(c)));
        }
        match (at.parent(), at.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                at = parent;
            }
            _ => break None,
        }
    };
    if let Some(real) = real.map(|r| r.display().to_string()) {
        if !out.contains(&real) {
            out.push(real);
        }
    }
    out
}

/// The plan agent's rules for Plan directory `dir`: `base` goes right after
/// the base defaults (reaching outside folders without asking, as
/// OpenCode's plan agent does, which config can still deny or make ask),
/// and `own` among the agent's built-in rules (no edits but plan files;
/// built-in, so they only restrict).
pub struct Rules {
    pub base: Vec<Rule>,
    pub own: Vec<Rule>,
}

pub fn rules(dir: &Path) -> Rules {
    let base = vec![Rule::new("external_directory", "*", Effect::Allow)];
    let mut own = vec![Rule::new("edit", "*", Effect::Deny)];
    for d in names(dir) {
        own.push(Rule::new("edit", &format!("{d}/*"), Effect::Allow));
    }
    Rules { base, own }
}

/// For a session in project `root` that contains the Plan directory (one
/// run from the home folder, say), the plan agent's edit allowance under
/// the project-relative names that files inside the project go by.
pub fn scope_to_root(agent: &str, rules: &mut Vec<Rule>, root: &Path, dir: &Path) {
    if agent != AGENT {
        return;
    }
    let roots = names(root);
    let mut extra = Vec::new();
    for d in names(dir) {
        let has = rules.iter().any(|r| {
            r.builtin
                && r.action == "edit"
                && r.effect == Effect::Allow
                && r.resource == format!("{d}/*")
        });
        if !has {
            continue;
        }
        for r in &roots {
            if let Ok(rel) = Path::new(&d).strip_prefix(r) {
                let rel = rel.display().to_string();
                if !rel.is_empty() {
                    extra
                        .push(Rule::new("edit", &format!("{rel}/*"), Effect::Allow).into_builtin());
                }
            }
        }
    }
    rules.extend(extra);
}

/// The text of `m` when it is a user message of exactly one text part.
fn sole_text(m: &Message) -> Option<&str> {
    match (&m.role, m.parts.as_slice()) {
        (Role::User, [Part::Text(t)]) => Some(t.text.as_str()),
        _ => None,
    }
}

/// The last Plan mode reminder in `messages`: `enter`, [`LEAVE`], or none.
fn last_reminder<'a>(messages: &[Message], enter: &'a str) -> Option<&'a str> {
    messages.iter().rev().find_map(|m| match sole_text(m) {
        Some(t) if t == enter => Some(enter),
        Some(t) if t == LEAVE => Some(LEAVE),
        _ => None,
    })
}

/// The reminder a request for `agent` over `messages` lacks, if any: the
/// enter reminder for the plan agent without a live one, the leave
/// reminder for another agent after a live enter reminder.
pub fn missing(agent: &str, messages: &[Message], dir: &Path) -> Option<String> {
    let enter = enter(dir);
    let last = last_reminder(messages, &enter);
    if agent == AGENT && last != Some(enter.as_str()) {
        Some(enter)
    } else if agent != AGENT && last == Some(enter.as_str()) {
        Some(LEAVE.to_string())
    } else {
        None
    }
}

/// [`missing`], inserted into a request's `messages` before the user's
/// prompt when it ends with one (where an agent switch puts it), else at
/// the end; returns the text to save.
pub fn reconcile(agent: &str, messages: &mut Vec<Message>, dir: &Path) -> Option<String> {
    let text = missing(agent, messages, dir)?;
    let prompt_last = messages.last().is_some_and(|m| {
        m.role == Role::User && !m.parts.iter().any(|p| matches!(p, Part::ToolResult { .. }))
    });
    let at = if prompt_last {
        messages.len() - 1
    } else {
        messages.len()
    };
    messages.insert(at, Message::user(text.clone()));
    Some(text)
}

/// Whether `output` is a tool failure a permission rule caused (OpenCode's
/// `Permission.BlockedError`), rather than a declined prompt or a failure
/// of the edit itself.
fn blocked(output: &str) -> bool {
    output.contains("Permission denied: a rule blocks ")
}

/// A plan agent's blocked write, reported as one outside the Plan directory.
pub fn rewrite_blocked(call: &ToolCall, output: &mut String, is_error: bool, dir: &Path) {
    if call.agent != AGENT || !is_error || !MUTATIONS.contains(&call.name.as_str()) {
        return;
    }
    if !blocked(output) {
        return;
    }
    *output = format!(
        "Cannot use {} to modify files outside the Plan directory: {}",
        call.name,
        dir.display()
    );
}

/// The plan plugin [`super::builtin`] installs.
pub struct PlanPlugin;

impl Plugin for PlanPlugin {
    fn id(&self) -> &str {
        ID
    }

    fn tool_after(&self, call: &ToolCall, output: &mut String, is_error: &mut bool) {
        rewrite_blocked(call, output, *is_error, &directory());
    }

    fn wants(&self, hook: super::Hook, _session_id: &str) -> bool {
        // Its reminders are saved by the session, not added by a hook.
        !matches!(
            hook,
            super::Hook::Context | super::Hook::Prompt | super::Hook::Http
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::evaluate;

    fn dir() -> PathBuf {
        PathBuf::from("/home/plan-test/.local/share/codesmith/plan")
    }

    fn text(m: &Message) -> &str {
        sole_text(m).unwrap()
    }

    /// The plan agent's rules as `agent::resolve` builds them.
    fn plan_rules() -> Vec<Rule> {
        let r = rules(&dir());
        let mut all = crate::permission::base_rules();
        all.extend(r.base);
        all.extend(r.own.into_iter().map(Rule::into_builtin));
        all
    }

    fn call(tool: &str, agent: &str) -> ToolCall {
        ToolCall {
            session_id: "ses_plan_test".into(),
            id: "call_plan_tool".into(),
            name: tool.into(),
            input: serde_json::json!({}),
            agent: agent.into(),
        }
    }

    // plan.test.ts: "injects enter and leave reminders on agent switches"
    #[test]
    fn oc_plan_injects_enter_and_leave_reminders_on_agent_switches() {
        let mut history = vec![Message::user("hello")];
        let enter = missing("plan", &history, &dir()).unwrap();
        assert!(enter.contains("You are in Plan mode"));
        assert!(enter
            .contains("Do not create or update plan files unless the user explicitly asks you to"));
        assert!(enter.contains(&dir().display().to_string()));
        assert!(enter.contains("Do not modify any other files"));
        history.push(Message::user(enter));
        let leave = missing("build", &history, &dir()).unwrap();
        assert!(leave.contains("NO LONGER in Plan mode"));
    }

    // plan.test.ts: "reconciles a missing enter reminder into the request and persists it"
    #[test]
    fn oc_plan_reconciles_a_missing_enter_reminder_into_the_request() {
        let mut messages = vec![Message::user("what agent are you?")];
        let saved = reconcile("plan", &mut messages, &dir());
        assert_eq!(messages.len(), 2);
        // Before the user's prompt, where agent-switch reminders land.
        assert!(text(&messages[0]).contains("You are in Plan mode"));
        assert_eq!(text(&messages[1]), "what agent are you?");
        assert_eq!(saved.as_deref(), Some(text(&messages[0])));
    }

    // plan.test.ts: "does nothing when the transcript already has a live enter reminder"
    #[test]
    fn oc_plan_does_nothing_with_a_live_enter_reminder() {
        let mut messages = vec![Message::user(enter(&dir())), Message::user("hello")];
        assert_eq!(reconcile("plan", &mut messages, &dir()), None);
        assert_eq!(messages.len(), 2);
    }

    // plan.test.ts: "reconciles a stale enter reminder with a leave reminder"
    #[test]
    fn oc_plan_reconciles_a_stale_enter_reminder_with_a_leave_reminder() {
        let mut messages = vec![
            Message::user(enter(&dir())),
            Message::user("ok implement it"),
        ];
        let saved = reconcile("build", &mut messages, &dir());
        assert_eq!(messages.len(), 3);
        assert!(text(&messages[1]).contains("NO LONGER in Plan mode"));
        assert_eq!(saved.as_deref(), Some(LEAVE));
    }

    // plan.test.ts: "does nothing for non-plan sessions without plan history"
    #[test]
    fn oc_plan_does_nothing_for_other_agents_without_plan_history() {
        let mut messages = vec![Message::user("hello")];
        assert_eq!(reconcile("build", &mut messages, &dir()), None);
        assert_eq!(messages.len(), 1);
    }

    // plan.test.ts: "does nothing when a leave reminder already follows the enter reminder"
    #[test]
    fn oc_plan_does_nothing_when_a_leave_reminder_follows_the_enter_reminder() {
        let mut messages = vec![
            Message::user(enter(&dir())),
            Message::user(LEAVE),
            Message::user("continue"),
        ];
        assert_eq!(reconcile("build", &mut messages, &dir()), None);
        assert_eq!(messages.len(), 3);
    }

    // plan.test.ts: "treats reminder text quoted inside a larger message as not live"
    #[test]
    fn oc_plan_treats_a_quoted_reminder_as_not_live() {
        let mut messages = vec![Message::user(format!(
            "<conversation-checkpoint>\n{}\n</conversation-checkpoint>",
            enter(&dir())
        ))];
        assert!(reconcile("plan", &mut messages, &dir()).is_some());
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn reminders_after_tool_results_go_last() {
        let mut messages = vec![
            Message::user("go"),
            Message {
                role: Role::User,
                parts: vec![Part::ToolResult {
                    id: "c".into(),
                    name: "read".into(),
                    content: "x".into(),
                    is_error: false,
                    media: vec![],
                }],
            },
        ];
        reconcile("plan", &mut messages, &dir()).unwrap();
        assert!(text(&messages[2]).contains("You are in Plan mode"));
    }

    // plan.test.ts: "does not create the Plan directory during activation"
    #[test]
    fn oc_plan_does_not_create_the_plan_directory_during_activation() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("plan");
        let _ = rules(&dir);
        let _ = enter(&dir);
        let _ = missing("plan", &[], &dir);
        assert!(!dir.exists());
    }

    // plan.test.ts: "allows edits only inside the Plan directory"
    #[test]
    fn oc_plan_allows_edits_only_inside_the_plan_directory() {
        let rules = plan_rules();
        let inside = dir().join("work.md").display().to_string();
        assert_eq!(evaluate("edit", &inside, &rules), Effect::Allow);
        assert_eq!(
            evaluate("edit", "/workspace/source.ts", &rules),
            Effect::Deny
        );
        assert_eq!(evaluate("edit", "source.ts", &rules), Effect::Deny);
    }

    // plan.test.ts: "allows external directories without asking"
    #[test]
    fn oc_plan_allows_external_directories_without_asking() {
        let rules = plan_rules();
        let glob = format!("{}/*", dir().display());
        assert_eq!(evaluate("external_directory", &glob, &rules), Effect::Allow);
        assert_eq!(
            evaluate("external_directory", "/outside/*", &rules),
            Effect::Allow
        );
        // Reaching a folder is not writing to it: edits there stay denied.
        assert_eq!(evaluate("edit", "/outside/x.rs", &rules), Effect::Deny);
    }

    #[test]
    fn a_configured_external_directory_rule_still_wins_for_the_plan_agent() {
        for (effect, want) in [(Effect::Deny, Effect::Deny), (Effect::Ask, Effect::Ask)] {
            let mut rules = crate::permission::base_rules();
            let r = super::rules(&dir());
            rules.extend(r.base);
            rules.push(Rule::new("external_directory", "/outside/*", effect));
            rules.extend(r.own.into_iter().map(Rule::into_builtin));
            assert_eq!(evaluate("external_directory", "/outside/*", &rules), want);
            assert_eq!(
                evaluate("external_directory", "/elsewhere/*", &rules),
                Effect::Allow
            );
        }
    }

    #[test]
    fn other_agents_still_ask_before_leaving_the_project() {
        assert_eq!(
            evaluate(
                "external_directory",
                "/outside/*",
                &crate::permission::base_rules()
            ),
            Effect::Ask
        );
    }

    #[test]
    fn a_configured_deny_still_covers_the_plan_directory() {
        let mut rules = crate::permission::base_rules();
        let r = super::rules(&dir());
        rules.extend(r.base);
        rules.push(Rule::new("edit", "*", Effect::Deny));
        rules.push(Rule::new("external_directory", "*", Effect::Deny));
        rules.extend(r.own.into_iter().map(Rule::into_builtin));
        let inside = dir().join("work.md").display().to_string();
        assert_eq!(evaluate("edit", &inside, &rules), Effect::Deny);
        let glob = format!("{}/*", dir().display());
        assert_eq!(evaluate("external_directory", &glob, &rules), Effect::Deny);
    }

    // plan.test.ts: "rewrites blocked mutation failures with the Plan directory"
    #[test]
    fn oc_plan_rewrites_blocked_mutation_failures_with_the_plan_directory() {
        for tool in MUTATIONS {
            let mut out = "Permission denied: a rule blocks edit for source.ts".to_string();
            rewrite_blocked(&call(tool, "plan"), &mut out, true, &dir());
            assert!(out.contains("outside the Plan directory"), "{out}");
            assert!(out.contains(&dir().display().to_string()), "{out}");
            assert!(out.contains(tool), "{out}");
        }
        // Other agents keep the permission error.
        let mut out = "Permission denied: a rule blocks edit for source.ts".to_string();
        rewrite_blocked(&call("edit", "build"), &mut out, true, &dir());
        assert!(out.starts_with("Permission denied"));
    }

    // plan.test.ts: "preserves mutation failures unrelated to permissions"
    #[test]
    fn oc_plan_preserves_mutation_failures_unrelated_to_permissions() {
        for msg in ["oldString was not found", "The user declined this request."] {
            let mut out = msg.to_string();
            rewrite_blocked(&call("edit", "plan"), &mut out, true, &dir());
            assert_eq!(out, msg);
        }
    }
}
