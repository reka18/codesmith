//! Permission rules: an ordered list of `{action, resource, effect}`. The last
//! rule matching both the action and the resource decides; with no match the
//! user is asked. Ported from OpenCode's `Permission.evaluate`.

use crate::wildcard;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

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
    asker: Box<dyn Asker>,
}

impl Gate {
    pub fn new(rules: Vec<Rule>, asker: Box<dyn Asker>) -> Self {
        Gate {
            rules,
            saved: Mutex::new(Vec::new()),
            asker,
        }
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
            Effect::Ask => match self.asker.ask(req) {
                Reply::Once => Ok(()),
                Reply::Always => {
                    let mut saved = self.saved.lock().unwrap();
                    for s in &req.save {
                        saved.push(Rule::new(&req.action, s, Effect::Allow));
                    }
                    Ok(())
                }
                Reply::Reject(feedback) => Err(Denied::Declined { feedback }),
            },
        }
    }
}

/// Every agent starts from these (OpenCode's `Agent.Info.default`).
pub fn base_rules() -> Vec<Rule> {
    vec![
        Rule::new("*", "*", Effect::Allow),
        Rule::new("read", "*.env", Effect::Ask),
        Rule::new("read", "*.env.*", Effect::Ask),
        Rule::new("read", "*.env.example", Effect::Allow),
    ]
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
            Box::new(Counting(n.clone(), Reply::Reject(None))),
        );
        assert!(g.assert(&req("read", &["a.rs"])).is_ok());
        assert_eq!(n.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn always_saves_an_approval() {
        let n = Arc::new(AtomicUsize::new(0));
        let g = Gate::new(vec![], Box::new(Counting(n.clone(), Reply::Always)));
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
        let g = Gate::new(vec![], Box::new(Counting(n.clone(), Reply::Once)));
        g.assert(&req("edit", &["a.rs"])).unwrap();
        g.assert(&req("edit", &["a.rs"])).unwrap();
        assert_eq!(n.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn reject_carries_feedback() {
        let g = Gate::new(
            vec![],
            Box::new(Fixed(Reply::Reject(Some("use git mv".into())))),
        );
        let e = g.assert(&req("shell", &["mv a b"])).unwrap_err();
        assert_eq!(
            e.to_string(),
            "The user declined this request and said: use git mv"
        );
        let g = Gate::new(vec![], Box::new(Fixed(Reply::Reject(None))));
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
        let g = Gate::new(rules, Box::new(Fixed(Reply::Always)));
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
        let g = Gate::new(rules, Box::new(Counting(n.clone(), Reply::Once)));
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
}
