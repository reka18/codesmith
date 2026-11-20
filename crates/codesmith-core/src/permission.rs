//! Permission rules: an ordered list of `{action, resource, effect}`. The last
//! rule matching both the action and the resource decides; with no match the
//! user is asked. Ported from OpenCode's `Permission.evaluate`.

use crate::wildcard;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effect {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub action: String,
    pub resource: String,
    pub effect: Effect,
}

impl Rule {
    pub fn new(action: &str, resource: &str, effect: Effect) -> Self {
        Rule {
            action: action.into(),
            resource: resource.into(),
            effect,
        }
    }
}

/// Actions whose resources are file paths. On macOS these match without case,
/// because the default volume ignores case.
pub const PATH_ACTIONS: [&str; 3] = ["external_directory", "read", "edit"];

fn ignores_case(action: &str) -> bool {
    cfg!(target_os = "macos") && PATH_ACTIONS.contains(&action)
}

/// The effect of the last rule matching `action` and `resource`; `Ask` if none.
pub fn evaluate(action: &str, resource: &str, rules: &[Rule]) -> Effect {
    let ic = ignores_case(action);
    rules
        .iter()
        .rev()
        .find(|r| {
            wildcard::matches(action, &r.action, false)
                && wildcard::matches(resource, &r.resource, ic)
        })
        .map(|r| r.effect)
        .unwrap_or(Effect::Ask)
}

/// Combined effect for a request over several resources: any deny denies,
/// otherwise any ask asks.
pub fn evaluate_all(action: &str, resources: &[String], rules: &[Rule]) -> Effect {
    let effects: Vec<Effect> = resources
        .iter()
        .map(|r| evaluate(action, r, rules))
        .collect();
    if effects.contains(&Effect::Deny) {
        Effect::Deny
    } else if effects.contains(&Effect::Ask) {
        Effect::Ask
    } else {
        Effect::Allow
    }
}

/// What the user answered when asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// Allow this one request.
    Once,
    /// Allow it and every later request matching the same rule this session.
    Always,
    /// Refuse; the optional feedback goes back to the model.
    Reject(Option<String>),
}

/// A request shown to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub action: String,
    pub resources: Vec<String>,
    /// Rules saved if the user answers `Always`.
    pub save: Vec<String>,
    /// One-line description for the prompt (e.g. the command or a diff summary).
    pub title: String,
}

/// Asks the user. Implemented by the CLI and, later, the TUI.
pub trait Asker: Send + Sync {
    fn ask(&self, req: &Request) -> Reply;
}

/// Answers every request with the same reply (for `--auto` and tests).
pub struct Fixed(pub Reply);

impl Asker for Fixed {
    fn ask(&self, _: &Request) -> Reply {
        self.0.clone()
    }
}

#[derive(Debug)]
pub enum Denied {
    /// A rule denies it.
    Blocked { action: String, resource: String },
    /// The user declined.
    Declined { feedback: Option<String> },
}

impl std::fmt::Display for Denied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Denied::Blocked { action, resource } => {
                write!(
                    f,
                    "Permission denied: a rule blocks {action} for {resource}"
                )
            }
            Denied::Declined { feedback: None } => write!(f, "The user declined this request."),
            Denied::Declined { feedback: Some(fb) } => {
                write!(f, "The user declined this request and said: {fb}")
            }
        }
    }
}

impl std::error::Error for Denied {}

/// The rules in force for a session plus approvals saved during it.
pub struct Gate {
    rules: Vec<Rule>,
    saved: Mutex<Vec<Rule>>,
    asker: Arc<dyn Asker>,
}

impl Gate {
    pub fn new(rules: Vec<Rule>, asker: Arc<dyn Asker>) -> Self {
        Gate {
            rules,
            saved: Mutex::new(Vec::new()),
            asker,
        }
    }

    /// Replace who answers permission requests (rules and "always" answers stay).
    pub fn set_asker(&mut self, asker: Arc<dyn Asker>) {
        self.asker = asker;
    }

    /// The asker, to share with child sessions.
    pub fn asker(&self) -> Arc<dyn Asker> {
        self.asker.clone()
    }

    pub fn rules(&self) -> Vec<Rule> {
        let mut all = self.rules.clone();
        all.extend(self.saved.lock().unwrap().iter().cloned());
        all
    }

    /// Allowed by rules alone, without asking.
    pub fn allows(&self, action: &str, resources: &[String]) -> bool {
        evaluate_all(action, resources, &self.rules()) == Effect::Allow
    }

    /// Refuse only when a configured rule denies the request; never asks.
    /// Used for follow-up requests the user already approved the start of,
    /// such as `webfetch` redirect targets (OpenCode's `assertNotDenied`).
    pub fn assert_not_denied(&self, action: &str, resources: &[String]) -> Result<(), Denied> {
        for r in resources {
            if evaluate(action, r, &self.rules) == Effect::Deny {
                return Err(Denied::Blocked {
                    action: action.into(),
                    resource: r.clone(),
                });
            }
        }
        Ok(())
    }

    /// Allow, ask, or refuse. Configured denies are checked before saved
    /// approvals, so "always allow" can never override an explicit deny.
    pub fn assert(&self, req: &Request) -> Result<(), Denied> {
        for r in &req.resources {
            if evaluate(&req.action, r, &self.rules) == Effect::Deny {
                return Err(Denied::Blocked {
                    action: req.action.clone(),
                    resource: r.clone(),
                });
            }
        }
        match evaluate_all(&req.action, &req.resources, &self.rules()) {
            Effect::Allow => Ok(()),
            Effect::Deny => Err(Denied::Blocked {
                action: req.action.clone(),
                resource: req.resources.join(", "),
            }),
            Effect::Ask => match crate::plugin::active()
                .permission(req)
                .unwrap_or_else(|| self.asker.ask(req))
            {
                Reply::Once => Ok(()),
                Reply::Always => {
                    let mut saved = self.saved.lock().unwrap();
                    for s in safe_saves(req) {
                        saved.push(Rule::new(&req.action, &s, Effect::Allow));
                    }
                    Ok(())
                }
                Reply::Reject(feedback) => Err(Denied::Declined { feedback }),
            },
        }
    }
}

/// The rules "always" saves for `req`. An `external_directory` approval is
/// saved for the directory (`<dir>/*`), except when that directory is `/`,
/// the home directory or above it: approving `~/.zshrc` must not approve all
/// of `$HOME`. Then the file itself is saved, and a directory not at all.
fn safe_saves(req: &Request) -> Vec<String> {
    if req.action != "external_directory" {
        return req.save.clone();
    }
    let home = dirs::home_dir().map(|h| h.display().to_string().replace('\\', "/"));
    let broad = |base: &str| {
        let base = base.replace('\\', "/");
        let base = base.trim_end_matches('/');
        base.is_empty()
            || base.contains('*')
            || base.ends_with(':')
            || home
                .as_deref()
                .is_some_and(|h| h == base || h.starts_with(&format!("{base}/")))
    };
    let mut out = Vec::new();
    for s in &req.save {
        let base = s.strip_suffix("/*").unwrap_or(s);
        if s == "*" || broad(base) {
            for r in &req.resources {
                if !r.ends_with("/*") && !out.contains(r) {
                    out.push(r.clone());
                }
            }
        } else if !out.contains(s) {
            out.push(s.clone());
        }
    }
    out
}

/// Every agent starts from these (OpenCode's `Agent.Info.default`, with
/// `external_directory` asking as in OpenCode's original defaults; the
/// current OpenCode core allows it). CodeSmith's own scratch and tool-output
/// directories stay allowed.
pub fn base_rules() -> Vec<Rule> {
    let mut rules = vec![
        Rule::new("*", "*", Effect::Allow),
        Rule::new("external_directory", "*", Effect::Ask),
    ];
    for dir in [crate::paths::tmp_dir(), crate::paths::tool_output_dir()] {
        let d = dir.display().to_string();
        rules.push(Rule::new("external_directory", &d, Effect::Allow));
        rules.push(Rule::new(
            "external_directory",
            &format!("{d}/*"),
            Effect::Allow,
        ));
    }
    rules.extend([
        Rule::new("read", "*.env", Effect::Ask),
        Rule::new("read", "*.env.*", Effect::Ask),
        Rule::new("read", "*.env.example", Effect::Allow),
    ]);
    rules
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn req(action: &str, res: &[&str]) -> Request {
        Request {
            action: action.into(),
            resources: res.iter().map(|s| s.to_string()).collect(),
            save: vec!["*".into()],
            title: String::new(),
        }
    }

    #[test]
    fn last_match_wins() {
        let rules = vec![
            Rule::new("*", "*", Effect::Allow),
            Rule::new("shell", "rm *", Effect::Deny),
        ];
        assert_eq!(evaluate("shell", "rm -rf x", &rules), Effect::Deny);
        assert_eq!(evaluate("shell", "ls", &rules), Effect::Allow);
        let rules = vec![
            Rule::new("shell", "rm *", Effect::Deny),
            Rule::new("*", "*", Effect::Allow),
        ];
        assert_eq!(evaluate("shell", "rm -rf x", &rules), Effect::Allow);
    }

    #[test]
    fn no_match_asks() {
        assert_eq!(evaluate("edit", "a.rs", &[]), Effect::Ask);
        assert_eq!(
            evaluate("edit", "a.rs", &[Rule::new("read", "*", Effect::Allow)]),
            Effect::Ask
        );
    }

    #[test]
    fn base_rules_guard_env_files() {
        let r = base_rules();
        assert_eq!(evaluate("read", "src/main.rs", &r), Effect::Allow);
        assert_eq!(evaluate("read", ".env", &r), Effect::Ask);
        assert_eq!(evaluate("read", "config/.env.local", &r), Effect::Ask);
        assert_eq!(evaluate("read", ".env.example", &r), Effect::Allow);
        assert_eq!(evaluate("edit", ".env", &r), Effect::Allow);
        assert_eq!(evaluate("shell", "cat .env", &r), Effect::Allow);
    }

    #[test]
    fn base_rules_ask_before_leaving_the_project() {
        let r = base_rules();
        assert_eq!(evaluate("external_directory", "/etc/*", &r), Effect::Ask);
        let home = dirs::home_dir().unwrap().display().to_string();
        assert_eq!(
            evaluate("external_directory", &format!("{home}/.zshrc"), &r),
            Effect::Ask
        );
        for dir in [crate::paths::tmp_dir(), crate::paths::tool_output_dir()] {
            let d = dir.display().to_string();
            assert_eq!(evaluate("external_directory", &d, &r), Effect::Allow);
            assert_eq!(
                evaluate("external_directory", &format!("{d}/x/*"), &r),
                Effect::Allow
            );
        }
        assert_eq!(evaluate("shell", "ls", &r), Effect::Allow);
    }

    #[test]
    fn always_never_saves_home_or_root_for_external_access() {
        let home = dirs::home_dir().unwrap().display().to_string();
        let saved = |resources: &[String], save: &[String]| {
            let g = Gate::new(vec![], Arc::new(Fixed(Reply::Always)));
            g.assert(&Request {
                action: "external_directory".into(),
                resources: resources.to_vec(),
                save: save.to_vec(),
                title: String::new(),
            })
            .unwrap();
            let out: Vec<String> = g
                .saved
                .lock()
                .unwrap()
                .iter()
                .map(|r| r.resource.clone())
                .collect();
            out
        };
        let file = format!("{home}/.zshrc");
        assert_eq!(
            saved(std::slice::from_ref(&file), &[format!("{home}/*")]),
            [file]
        );
        assert_eq!(saved(&["/x".into()], &["/*".into()]), ["/x"]);
        let parent = std::path::Path::new(&home)
            .parent()
            .unwrap()
            .display()
            .to_string();
        assert_eq!(
            saved(&[format!("{parent}/y")], &[format!("{parent}/*")]),
            [format!("{parent}/y")]
        );
        // A directory resource is not saved at all.
        assert!(saved(&[format!("{home}/*")], &[format!("{home}/*")]).is_empty());
        // A specific directory is saved as usual.
        assert_eq!(
            saved(&[format!("{home}/src/a.rs")], &[format!("{home}/src/*")]),
            [format!("{home}/src/*")]
        );
        assert_eq!(
            saved(&["/etc/hosts".into()], &["/etc/*".into()]),
            ["/etc/*"]
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn path_actions_ignore_case_on_macos() {
        let r = base_rules();
        assert_eq!(evaluate("read", ".ENV", &r), Effect::Ask);
        let s = vec![
            Rule::new("*", "*", Effect::Allow),
            Rule::new("shell", "RM *", Effect::Deny),
        ];
        assert_eq!(
            evaluate("shell", "rm x", &s),
            Effect::Allow,
            "commands stay case-sensitive"
        );
    }

    #[test]
    fn combined_effects() {
        let r = base_rules();
        let both = vec!["a.rs".to_string(), ".env".to_string()];
        assert_eq!(evaluate_all("read", &both, &r), Effect::Ask);
        let deny = vec![
            Rule::new("*", "*", Effect::Allow),
            Rule::new("read", "secret", Effect::Deny),
        ];
        assert_eq!(
            evaluate_all("read", &["a".into(), "secret".into()], &deny),
            Effect::Deny
        );
        assert_eq!(evaluate_all("read", &[], &deny), Effect::Allow);
    }

    struct Counting(Arc<AtomicUsize>, Reply);
    impl Asker for Counting {
        fn ask(&self, _: &Request) -> Reply {
            self.0.fetch_add(1, Ordering::SeqCst);
            self.1.clone()
        }
    }

    #[test]
    fn allowed_requests_never_ask() {
        let n = Arc::new(AtomicUsize::new(0));
        let g = Gate::new(
            base_rules(),
            Arc::new(Counting(n.clone(), Reply::Reject(None))),
        );
        assert!(g.assert(&req("read", &["a.rs"])).is_ok());
        assert_eq!(n.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn always_saves_an_approval() {
        let n = Arc::new(AtomicUsize::new(0));
        let g = Gate::new(vec![], Arc::new(Counting(n.clone(), Reply::Always)));
        assert!(g.assert(&req("edit", &["a.rs"])).is_ok());
        assert!(g.assert(&req("edit", &["b.rs"])).is_ok());
        assert_eq!(
            n.load(Ordering::SeqCst),
            1,
            "the saved * rule covers the second edit"
        );
        assert!(g.allows("edit", &["c.rs".into()]));
        assert!(!g.allows("shell", &["ls".into()]));
    }

    #[test]
    fn once_does_not_save() {
        let n = Arc::new(AtomicUsize::new(0));
        let g = Gate::new(vec![], Arc::new(Counting(n.clone(), Reply::Once)));
        g.assert(&req("edit", &["a.rs"])).unwrap();
        g.assert(&req("edit", &["a.rs"])).unwrap();
        assert_eq!(n.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn reject_carries_feedback() {
        let g = Gate::new(
            vec![],
            Arc::new(Fixed(Reply::Reject(Some("use git mv".into())))),
        );
        let e = g.assert(&req("shell", &["mv a b"])).unwrap_err();
        assert_eq!(
            e.to_string(),
            "The user declined this request and said: use git mv"
        );
        let g = Gate::new(vec![], Arc::new(Fixed(Reply::Reject(None))));
        assert_eq!(
            g.assert(&req("shell", &["x"])).unwrap_err().to_string(),
            "The user declined this request."
        );
    }

    #[test]
    fn saved_approval_cannot_override_a_deny() {
        let rules = vec![
            Rule::new("*", "*", Effect::Ask),
            Rule::new("shell", "rm *", Effect::Deny),
        ];
        let g = Gate::new(rules, Arc::new(Fixed(Reply::Always)));
        g.assert(&req("shell", &["ls"])).unwrap();
        assert!(matches!(
            g.assert(&req("shell", &["rm -rf /"])),
            Err(Denied::Blocked { .. })
        ));
    }

    #[test]
    fn deny_never_asks() {
        let n = Arc::new(AtomicUsize::new(0));
        let rules = vec![Rule::new("edit", "*", Effect::Deny)];
        let g = Gate::new(rules, Arc::new(Counting(n.clone(), Reply::Once)));
        assert!(g.assert(&req("edit", &["a"])).is_err());
        assert_eq!(n.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn effects_serialize_lowercase() {
        let r: Rule = serde_json::from_value(
            serde_json::json!({"action": "shell", "resource": "git *", "effect": "ask"}),
        )
        .unwrap();
        assert_eq!(r, Rule::new("shell", "git *", Effect::Ask));
    }

    // ---------------------------------------------------------------------
    // Ported from OpenCode's packages/core/test/permission.test.ts (MIT).
    // One `oc_permission_*` test per OpenCode case, in the same order.
    //
    // Intentional differences:
    // - CodeSmith asks synchronously through an `Asker`; there is no pending
    //   request queue, request IDs, bus events, metadata or nested-call rows,
    //   so the cases about listing, sibling rejection, pending auto-approval
    //   and JSON encoding of pending requests have no counterpart.
    // - "Always" approvals are kept for the session (`Gate`), not stored per
    //   project in a database, and cannot be edited.
    // - Custom agents start from the base rules (`* allow`, `.env` asks),
    //   not from an empty rule set; a session without an agent uses `build`
    //   (resolved in the CLI, not here).
    // - The shell scanner impact cases run CodeSmith's scanner (the "native"
    //   column of OpenCode's table); PowerShell fixtures are not ported.
    // ---------------------------------------------------------------------

    fn assertion(action: &str, res: &[&str], save: &[&str]) -> Request {
        Request {
            action: action.into(),
            resources: res.iter().map(|s| s.to_string()).collect(),
            save: save.iter().map(|s| s.to_string()).collect(),
            title: String::new(),
        }
    }

    /// What a gate does with `req`: allow without asking, ask, or deny.
    fn outcome(g: &Gate, n: &AtomicUsize, req: &Request) -> Effect {
        let before = n.load(Ordering::SeqCst);
        match g.assert(req) {
            Err(Denied::Blocked { .. }) => Effect::Deny,
            _ if n.load(Ordering::SeqCst) > before => Effect::Ask,
            _ => Effect::Allow,
        }
    }

    fn counting(rules: Vec<Rule>, reply: Reply) -> (Gate, Arc<AtomicUsize>) {
        let n = Arc::new(AtomicUsize::new(0));
        (Gate::new(rules, Arc::new(Counting(n.clone(), reply))), n)
    }

    fn allow_all(action: &str) -> Vec<Rule> {
        vec![Rule::new(action, "*", Effect::Allow)]
    }

    #[test]
    fn oc_permission_returns_the_evaluated_effect_and_only_asks_when_needed() {
        let req = assertion("read", &["src/index.ts"], &[]);
        let (g, n) = counting(allow_all("read"), Reply::Once);
        assert_eq!(outcome(&g, &n, &req), Effect::Allow);
        let (g, n) = counting(vec![Rule::new("read", "*", Effect::Deny)], Reply::Once);
        assert_eq!(outcome(&g, &n, &req), Effect::Deny);
        let (g, n) = counting(vec![], Reply::Once);
        assert_eq!(outcome(&g, &n, &req), Effect::Ask);
    }

    #[test]
    fn oc_permission_evaluates_against_an_explicit_agent() {
        let mut config = crate::config::Config::default();
        let reviewer = |rules: Vec<Rule>| crate::config::AgentConfig {
            permissions: rules,
            ..Default::default()
        };
        config.agents.insert(
            "reviewer".into(),
            reviewer(vec![Rule::new("read", "*", Effect::Deny)]),
        );
        let rules =
            |c: &crate::config::Config, id: &str| crate::agent::resolve(c, id).unwrap().permissions;
        assert_eq!(
            evaluate("read", "src/index.ts", &rules(&config, "build")),
            Effect::Allow
        );
        assert_eq!(
            evaluate("read", "src/index.ts", &rules(&config, "reviewer")),
            Effect::Deny
        );
        config.agents.insert("reviewer".into(), reviewer(vec![]));
        // Custom agents start from the base rules, which allow (see header).
        assert_eq!(
            evaluate("read", "src/index.ts", &rules(&config, "reviewer")),
            Effect::Allow
        );
    }

    #[test]
    fn oc_permission_allows_and_denies_from_explicit_rules_without_asking() {
        let req = assertion("read", &["src/index.ts"], &[]);
        let (g, n) = counting(allow_all("read"), Reply::Reject(None));
        assert!(g.assert(&req).is_ok());
        let (g, _) = counting(vec![Rule::new("read", "*", Effect::Deny)], Reply::Once);
        assert!(matches!(g.assert(&req), Err(Denied::Blocked { .. })));
        assert_eq!(n.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn oc_permission_checks_for_a_deny_without_asking() {
        let res = vec!["src/index.ts".to_string()];
        let (g, n) = counting(vec![], Reply::Reject(None));
        assert!(g.assert_not_denied("read", &res).is_ok());
        assert_eq!(n.load(Ordering::SeqCst), 0);
        let (g, _) = counting(vec![Rule::new("read", "*", Effect::Deny)], Reply::Once);
        assert!(matches!(
            g.assert_not_denied("read", &res),
            Err(Denied::Blocked { .. })
        ));
    }

    #[test]
    fn oc_permission_reports_whether_a_request_is_already_allowed() {
        let res = vec!["src/index.ts".to_string()];
        let (g, n) = counting(vec![], Reply::Once);
        assert!(!g.allows("read", &res));
        assert_eq!(n.load(Ordering::SeqCst), 0);
        assert!(counting(allow_all("read"), Reply::Once)
            .0
            .allows("read", &res));
        assert!(
            !counting(vec![Rule::new("read", "*", Effect::Deny)], Reply::Once)
                .0
                .allows("read", &res)
        );
    }

    #[test]
    fn oc_permission_matches_windows_rules_against_slash_normalized_resources() {
        let rules = vec![Rule::new(
            "external_directory",
            "C:\\Users\\x\\proj\\*",
            Effect::Allow,
        )];
        assert_eq!(
            evaluate("external_directory", "C:/Users/x/proj/src/*", &rules),
            Effect::Allow
        );
        assert_eq!(
            evaluate("external_directory", "C:/Users/x/other/*", &rules),
            Effect::Ask
        );
    }

    #[test]
    fn oc_permission_matches_file_rules_without_case_where_files_ignore_it() {
        let rules = vec![
            Rule::new("*", "*", Effect::Allow),
            Rule::new("edit", ".opencode/*", Effect::Ask),
            Rule::new("read", "*.env", Effect::Deny),
            Rule::new("shell", "git push*", Effect::Ask),
        ];
        let files = cfg!(target_os = "macos") || cfg!(windows);
        let expect = |yes: Effect| if files { yes } else { Effect::Allow };
        assert_eq!(
            evaluate("edit", ".OPENCODE/plugins/guard.ts", &rules),
            expect(Effect::Ask)
        );
        assert_eq!(
            evaluate("read", "Secrets.ENV", &rules),
            expect(Effect::Deny)
        );
        assert_eq!(
            evaluate("shell", "GIT PUSH origin dev", &rules),
            if cfg!(windows) {
                Effect::Ask
            } else {
                Effect::Allow
            }
        );
        assert!(crate::wildcard::matches(".OPENCODE/x", ".opencode/*", true));
        assert_eq!(
            crate::wildcard::matches(".OPENCODE/x", ".opencode/*", false),
            cfg!(windows)
        );
    }

    #[test]
    fn oc_permission_allows_managed_output_reads_without_external_access() {
        let rules = vec![
            Rule::new("*", "*", Effect::Deny),
            Rule::new("read", "*", Effect::Allow),
        ];
        assert_eq!(evaluate("read", "tool_123", &rules), Effect::Allow);
        assert_eq!(
            evaluate("external_directory", "/tmp/tool-output/*", &rules),
            Effect::Deny
        );
    }

    #[test]
    fn oc_permission_evaluates_shell_with_normal_rule_semantics() {
        let req = assertion("shell", &["pwd"], &[]);
        let (g, n) = counting(allow_all("*"), Reply::Once);
        assert_eq!(outcome(&g, &n, &req), Effect::Allow);
        let (g, n) = counting(vec![], Reply::Once);
        assert_eq!(outcome(&g, &n, &req), Effect::Ask);
    }

    #[test]
    fn oc_permission_session_rules_after_agent_rules_before_saved_approvals() {
        let agent = allow_all("*");
        let session = vec![Rule::new("edit", "/original/**", Effect::Deny)];
        let rules: Vec<Rule> = agent.into_iter().chain(session).collect();
        assert_eq!(
            evaluate("edit", "/original/src/index.ts", &rules),
            Effect::Deny
        );

        let pwd = assertion("shell", &["pwd"], &["pwd"]);
        let (g, _) = counting(vec![Rule::new("shell", "*", Effect::Deny)], Reply::Once);
        g.saved
            .lock()
            .unwrap()
            .push(Rule::new("shell", "pwd", Effect::Allow));
        assert!(matches!(g.assert(&pwd), Err(Denied::Blocked { .. })));
        let (g, n) = counting(vec![Rule::new("shell", "*", Effect::Ask)], Reply::Once);
        g.saved
            .lock()
            .unwrap()
            .push(Rule::new("shell", "pwd", Effect::Allow));
        assert_eq!(outcome(&g, &n, &pwd), Effect::Allow);
    }

    #[test]
    fn oc_permission_saved_approvals_keep_configured_deny_precedence() {
        let pwd = assertion("shell", &["pwd"], &["pwd"]);
        let (g, n) = counting(vec![], Reply::Always);
        assert_eq!(outcome(&g, &n, &pwd), Effect::Ask);
        assert_eq!(outcome(&g, &n, &pwd), Effect::Allow);
        let (g, _) = counting(vec![Rule::new("shell", "*", Effect::Deny)], Reply::Always);
        g.saved
            .lock()
            .unwrap()
            .push(Rule::new("shell", "pwd", Effect::Allow));
        assert!(matches!(g.assert(&pwd), Err(Denied::Blocked { .. })));
    }

    #[test]
    fn oc_permission_resolves_an_asked_permission_once() {
        let (g, n) = counting(vec![], Reply::Once);
        g.assert(&assertion("read", &["src/index.ts"], &[]))
            .unwrap();
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn oc_permission_declining_fails_the_request() {
        let (g, _) = counting(vec![], Reply::Reject(None));
        assert!(matches!(
            g.assert(&assertion("read", &["src/index.ts"], &[])),
            Err(Denied::Declined { feedback: None })
        ));
    }

    #[test]
    fn oc_permission_always_saves_the_requested_patterns() {
        let (g, n) = counting(vec![], Reply::Always);
        g.assert(&assertion("read", &["src/index.ts"], &["src/*"]))
            .unwrap();
        assert_eq!(
            *g.saved.lock().unwrap(),
            vec![Rule::new("read", "src/*", Effect::Allow)]
        );
        assert_eq!(
            outcome(&g, &n, &assertion("read", &["src/next.ts"], &[])),
            Effect::Allow
        );
        assert_eq!(
            outcome(&g, &n, &assertion("read", &["docs/a.md"], &[])),
            Effect::Ask
        );
    }

    /// Effect of the scanned `command` under configured `rules` and `saved`
    /// approvals, as the shell tool asks it.
    fn shell_effect(command: &str, rules: Vec<Rule>, saved: &[&str]) -> (Effect, Vec<String>) {
        let parsed =
            crate::tool::shell_scan::analyze(command, std::path::Path::new("/project")).unwrap();
        assert!(!parsed.commands.is_empty(), "{command}");
        assert!(parsed.directories.is_empty(), "{command}");
        let (resources, save): (Vec<String>, Vec<String>) = parsed.commands.into_iter().unzip();
        let (g, n) = counting(rules, Reply::Once);
        for s in saved {
            g.saved
                .lock()
                .unwrap()
                .push(Rule::new("shell", s, Effect::Allow));
        }
        let req = Request {
            action: "shell".into(),
            resources,
            save: save.clone(),
            title: String::new(),
        };
        (outcome(&g, &n, &req), save)
    }

    #[test]
    fn oc_permission_shell_scanner_impact() {
        use Effect::{Allow, Ask, Deny};
        struct Fixture {
            command: &'static str,
            approved: &'static [&'static str],
            exact: &'static [&'static str],
            denied: &'static str,
            saved: Effect,
            exact_effect: Effect,
            denied_effect: Effect,
        }
        let fixtures = [
            Fixture {
                command: "time -p git status",
                approved: &["time *"],
                exact: &["time -p git status"],
                denied: "time -p git status",
                saved: Allow,
                exact_effect: Allow,
                denied_effect: Deny,
            },
            Fixture {
                command: "coproc git status",
                approved: &["coproc *"],
                exact: &["coproc git status"],
                denied: "coproc git status",
                saved: Allow,
                exact_effect: Allow,
                denied_effect: Deny,
            },
            Fixture {
                command: "export X=value; unset X; git status",
                approved: &["git status *"],
                exact: &["git status"],
                denied: "export *",
                saved: Allow,
                exact_effect: Allow,
                denied_effect: Allow,
            },
            // CodeSmith: a declaration whose value is not static is checked
            // as a command (OpenCode skips it), so `export *` rules apply.
            Fixture {
                command: "export VERSION=$(git describe --tags); npm run build",
                approved: &["export *", "git describe *", "npm run build *"],
                exact: &[
                    "export VERSION=$(git describe --tags)",
                    "git describe --tags",
                    "npm run build",
                ],
                denied: "export *",
                saved: Allow,
                exact_effect: Allow,
                denied_effect: Deny,
            },
            Fixture {
                command: "export VERSION=$(git describe --tags); npm run build",
                approved: &["export *", "git describe *", "npm run build *"],
                exact: &[
                    "export VERSION=$(git describe --tags)",
                    "git describe --tags",
                    "npm run build",
                ],
                denied: "git describe *",
                saved: Allow,
                exact_effect: Allow,
                denied_effect: Deny,
            },
            Fixture {
                command: "printf ok && git status > output",
                approved: &["printf *", "git status *"],
                exact: &["printf ok", "git status"],
                denied: "git status *",
                saved: Allow,
                exact_effect: Ask,
                denied_effect: Deny,
            },
            Fixture {
                command: "printf ok | cat < input > output",
                approved: &["printf *", "cat *"],
                exact: &["printf ok", "cat"],
                denied: "cat *",
                saved: Allow,
                exact_effect: Ask,
                denied_effect: Deny,
            },
            Fixture {
                command: "FOO=bar > output; printf done",
                approved: &["printf *"],
                exact: &["printf done"],
                denied: "FOO=bar > output",
                saved: Ask,
                exact_effect: Ask,
                denied_effect: Deny,
            },
            Fixture {
                command: "VERSION=$(git describe --tags) > build/version.txt",
                approved: &["git describe *"],
                exact: &["git describe --tags"],
                denied: "VERSION=$(git describe --tags) > build/version.txt",
                saved: Ask,
                exact_effect: Ask,
                denied_effect: Deny,
            },
            Fixture {
                command: "git $(printf diff) --stat",
                approved: &["git *", "printf *"],
                exact: &["git $(printf diff) --stat", "printf diff"],
                denied: "git $(printf diff) --stat",
                saved: Allow,
                exact_effect: Allow,
                denied_effect: Deny,
            },
            Fixture {
                command: "true && cat > secret",
                approved: &["true *", "cat *"],
                exact: &["true", "cat"],
                denied: "* > secret*",
                saved: Allow,
                exact_effect: Ask,
                denied_effect: Deny,
            },
            Fixture {
                command: ">> ~/.zshrc <<< 'x'",
                approved: &[">> *"],
                exact: &[">> ~/.zshrc <<< 'x'"],
                denied: ">> *",
                saved: Allow,
                exact_effect: Allow,
                denied_effect: Deny,
            },
        ];
        for f in fixtures {
            let c = f.command;
            let exact_rules = || {
                f.exact
                    .iter()
                    .map(|r| Rule::new("shell", r, Allow))
                    .collect::<Vec<_>>()
            };
            assert_eq!(shell_effect(c, vec![], &[]).0, Ask, "{c}: no approval");
            assert_eq!(
                shell_effect(c, vec![], &["*"]).0,
                Allow,
                "{c}: saved wildcard"
            );
            assert_eq!(
                shell_effect(c, vec![], f.approved).0,
                f.saved,
                "{c}: saved approvals"
            );
            assert_eq!(
                shell_effect(c, exact_rules(), &[]).0,
                f.exact_effect,
                "{c}: exact configured"
            );
            assert_eq!(
                shell_effect(c, vec![], f.exact).0,
                f.exact_effect,
                "{c}: exact saved"
            );
            assert_eq!(
                shell_effect(c, vec![Rule::new("shell", f.denied, Deny)], &["*"]).0,
                f.denied_effect,
                "{c}: configured deny"
            );
        }
    }

    #[test]
    fn oc_permission_always_covers_repeats_and_narrows_prefix_scope() {
        use Effect::{Allow, Ask};
        let fixtures: &[(&str, &[&str], &str, Effect)] = &[
            ("npm run 123", &["npm run 123 *"], "npm run build", Ask),
            // CodeSmith: a prefix holding an option is saved exactly, not as
            // `aws --cli-read-timeout 60 *` (OpenCode), which would cover any
            // aws command (as `git -c *` covers any git config).
            (
                "aws --cli-read-timeout 60 s3 ls",
                &["aws --cli-read-timeout 60 s3 ls"],
                "aws --cli-read-timeout 60 ec2 describe-instances",
                Ask,
            ),
            (
                "git $(printf diff) --stat",
                &["git $(printf diff) *", "printf *"],
                "git --stat",
                Ask,
            ),
            (
                "printf ok && git status > output",
                &["printf *", "git status *"],
                "git status --short",
                Allow,
            ),
            (
                "FOO=bar > output; printf done",
                &["FOO=bar > *", "printf *"],
                "FOO=bar rm -rf /",
                Ask,
            ),
        ];
        let request = |command: &str| {
            let parsed =
                crate::tool::shell_scan::analyze(command, std::path::Path::new("/project"))
                    .unwrap();
            let (resources, save) = parsed.commands.into_iter().unzip();
            Request {
                action: "shell".into(),
                resources,
                save,
                title: String::new(),
            }
        };
        for (command, grants, next, next_effect) in fixtures {
            let n = Arc::new(AtomicUsize::new(0));
            let g = Gate::new(vec![], Arc::new(Counting(n.clone(), Reply::Always)));
            assert_eq!(outcome(&g, &n, &request(command)), Ask);
            let mut saved: Vec<String> = g
                .saved
                .lock()
                .unwrap()
                .iter()
                .map(|r| r.resource.clone())
                .collect();
            saved.sort();
            saved.dedup();
            let mut want: Vec<String> = grants.iter().map(|s| s.to_string()).collect();
            want.sort();
            assert_eq!(saved, want, "{command}");
            assert_eq!(
                outcome(&g, &n, &request(command)),
                Allow,
                "{command}: repeat"
            );
            assert_eq!(outcome(&g, &n, &request(next)), *next_effect, "{next}");
        }
    }
}
