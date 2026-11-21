//! Permission rules: an ordered list of `{action, resource, effect}`. The last
//! rule matching both the action and the resource decides; with no match the
//! user is asked. Ported from OpenCode's `Permission.evaluate`.
//!
//! One addition: a built-in agent's own rules (plan's edit deny, the
//! explore/review allowlists) are marked [`Rule::builtin`] and can only
//! restrict. The list is evaluated with and without them and the stricter
//! effect wins, so a config-wide allow cannot widen a read-only agent and a
//! config-wide deny or ask still reaches it.

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
    /// One of a built-in agent's own rules: it can tighten what the other
    /// rules allow but never loosen it (see the module docs). Not read from
    /// or written to config.
    #[serde(skip)]
    pub builtin: bool,
    /// A built-in secret guard ([`secret_guards`]): a floor under every
    /// other rule and saved approval. Guards are judged among themselves
    /// (the last matching guard decides), and when that is stricter than
    /// what the other rules give, it wins, unless the last matching
    /// configured rule is an allow that names the path itself (see
    /// [`evaluate`]). A guard's action is literal (no wildcard). Not read
    /// from or written to config.
    #[serde(skip)]
    pub guard: bool,
    /// A `tool.use` policy statement (see [`Policy`]): `resource` is an
    /// `action:resource` pattern, and the statement can deny but never
    /// grant. Not read from or written to config as a rule.
    #[serde(skip)]
    pub policy: Option<Arc<str>>,
}

impl Rule {
    pub fn new(action: &str, resource: &str, effect: Effect) -> Self {
        Rule {
            action: action.into(),
            resource: resource.into(),
            effect,
            builtin: false,
            guard: false,
            policy: None,
        }
    }

    /// This rule marked as a built-in agent rule ([`Rule::builtin`]).
    pub fn into_builtin(mut self) -> Self {
        self.builtin = true;
        self
    }

    /// This rule marked as a secret guard ([`Rule::guard`]).
    pub fn into_guard(mut self) -> Self {
        self.guard = true;
        self
    }
}

/// A policy statement from config (OpenCode's `ConfigPolicy`):
/// `{"action": "tool.use", "resource": "shell:git push *", "effect":
/// "deny", "message": "…"}`. For a permission request, each resource is
/// matched as `action:resource` against the `tool.use` statements, the last
/// match deciding; a deny refuses the request whatever the rules or saved
/// approvals say (asking included), and an allow only lifts an earlier
/// statement's deny: it never grants anything. Other actions
/// (`provider.use`, `mcp.use`, …) are OpenCode's managed policies and are
/// not read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    pub action: String,
    pub resource: String,
    pub effect: PolicyEffect,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PolicyEffect {
    Allow,
    Deny,
}

/// What a policy deny says when it names no message (OpenCode's).
pub const POLICY_MESSAGE: &str = "Blocked by configuration policy";

impl Policy {
    pub fn tool(effect: PolicyEffect, resource: &str) -> Policy {
        Policy {
            action: "tool.use".into(),
            resource: resource.into(),
            effect,
            message: None,
        }
    }

    /// The statement as a rule carried with an agent's rules, if it is a
    /// `tool.use` one.
    pub fn rule(&self) -> Option<Rule> {
        (self.action == "tool.use").then(|| Rule {
            action: "tool.use".into(),
            resource: self.resource.clone(),
            effect: match self.effect {
                PolicyEffect::Allow => Effect::Allow,
                PolicyEffect::Deny => Effect::Deny,
            },
            builtin: false,
            guard: false,
            policy: Some(self.message.as_deref().unwrap_or(POLICY_MESSAGE).into()),
        })
    }
}

/// The message of the policy statement that denies `action` on any of
/// `resources` (see [`Policy`]), if one does.
pub fn policy_denial(action: &str, resources: &[String], rules: &[Rule]) -> Option<String> {
    resources
        .iter()
        .find_map(|r| policy_denies(action, r, &[rules]))
}

/// The message of a policy deny that could refuse *some* `action` request,
/// for a request whose resource is not known (a computed command name, an
/// evaluated string, a write to a computed path): a deny statement whose
/// pattern can match `action:…`, unless a later allow statement lifts it
/// for every resource of the action (`*` or `action:*`).
pub fn policy_may_deny(action: &str, rules: &[Rule]) -> Option<String> {
    let ic = ignores_case(action);
    let key = format!("{action}:");
    let policies: Vec<&Rule> = rules.iter().filter(|r| r.policy.is_some()).collect();
    let from = policies
        .iter()
        .rposition(|r| {
            r.effect == Effect::Allow && (r.resource == "*" || r.resource == format!("{key}*"))
        })
        .map_or(0, |i| i + 1);
    policies[from..]
        .iter()
        .rev()
        .find(|r| r.effect == Effect::Deny && pattern_may_match_prefix(&r.resource, &key, ic))
        .map(|r| r.policy.as_deref().unwrap_or(POLICY_MESSAGE).to_string())
}

/// Whether `pattern` could match some input starting with `prefix`: the
/// pattern's literal head (before its first wildcard) and `prefix` agree as
/// far as both go. Over-approximates; that only refuses more.
fn pattern_may_match_prefix(pattern: &str, prefix: &str, ignore_case: bool) -> bool {
    let pattern = pattern.replace('\\', "/");
    let head = &pattern[..pattern.find(['*', '?']).unwrap_or(pattern.len())];
    let (head, prefix) = if ignore_case {
        (head.to_lowercase(), prefix.to_lowercase())
    } else {
        (head.to_string(), prefix.to_string())
    };
    head.starts_with(&prefix) || prefix.starts_with(&head)
}

fn policy_denies(action: &str, resource: &str, layers: &[&[Rule]]) -> Option<String> {
    let ic = ignores_case(action);
    let key = format!("{action}:{resource}");
    let last = layers
        .iter()
        .rev()
        .flat_map(|l| l.iter().rev())
        .filter(|r| r.policy.is_some())
        .find(|r| wildcard::matches(&key, &r.resource, ic))?;
    (last.effect == Effect::Deny)
        .then(|| last.policy.as_deref().unwrap_or(POLICY_MESSAGE).to_string())
}

fn strictness(e: Effect) -> u8 {
    match e {
        Effect::Allow => 0,
        Effect::Ask => 1,
        Effect::Deny => 2,
    }
}

/// Actions whose resources are file paths. On macOS these match without case,
/// because the default volume ignores case.
pub const PATH_ACTIONS: [&str; 3] = ["external_directory", "read", "edit"];

fn ignores_case(action: &str) -> bool {
    cfg!(target_os = "macos") && PATH_ACTIONS.contains(&action)
}

/// The effect of the last rule matching `action` and `resource`; `Ask` if
/// none. When built-in agent rules are present, the stricter of that and
/// the same evaluation without them. Secret guards ([`Rule::guard`]) are
/// then a floor (see [`evaluate_layers`]). Policy statements are not rules:
/// [`Gate`] checks them first.
pub fn evaluate(action: &str, resource: &str, rules: &[Rule]) -> Effect {
    evaluate_layers(action, resource, &[rules])
}

/// `evaluate` over `layers` concatenated in order, without copying them.
/// The first layer holds the configured rules; later ones (saved
/// approvals) can never lift a secret guard: only a configured allow that
/// names the path itself does (see [`explicit_for`]).
fn evaluate_layers(action: &str, resource: &str, layers: &[&[Rule]]) -> Effect {
    let ic = ignores_case(action);
    // A path rule written from the home folder (`~/notes/**`) matches the
    // absolute paths it names, as in OpenCode; other actions (a shell
    // command's text) match `~` as written.
    let home = PATH_ACTIONS
        .contains(&action)
        .then(dirs::home_dir)
        .flatten()
        .map(|h| h.display().to_string());
    let from_home = |pattern: &str| -> Option<String> {
        let home = home.as_deref()?;
        match pattern.strip_prefix("~/") {
            Some(rest) => Some(format!("{}/{rest}", home.trim_end_matches('/'))),
            None => (pattern == "~").then(|| home.to_string()),
        }
    };
    let matches_rule = |r: &Rule| {
        r.policy.is_none()
            && wildcard::matches(action, &r.action, false)
            && (wildcard::matches(resource, &r.resource, ic)
                || from_home(&r.resource).is_some_and(|p| wildcard::matches(resource, &p, ic)))
    };
    let hit = |r: &&Rule| !r.guard && matches_rule(r);
    let newest_first = || layers.iter().rev().flat_map(|l| l.iter().rev());
    let all = newest_first()
        .find(hit)
        .map(|r| r.effect)
        .unwrap_or(Effect::Ask);
    // Built-in agent rules only restrict: take the stricter of the result
    // with them and without them.
    let judged = if !layers.iter().any(|l| l.iter().any(|r| r.builtin)) {
        all
    } else {
        let others = newest_first()
            .filter(|r| !r.builtin)
            .find(hit)
            .map(|r| r.effect)
            .unwrap_or(Effect::Ask);
        if strictness(others) > strictness(all) {
            others
        } else {
            all
        }
    };
    // Secret guards: the last matching one is a floor.
    // Guards name their action literally, so most rules and every other
    // action are passed over without a pattern match.
    let Some(guard) = newest_first().find(|r| r.guard && r.action == action && matches_rule(r))
    else {
        return judged;
    };
    if strictness(guard.effect) <= strictness(judged) {
        return judged;
    }
    // Lifted only by the user's own configured allow naming the path (or
    // the guard's own pattern), when it is the last configured match.
    let configured = layers
        .first()
        .and_then(|l| l.iter().rev().filter(|r| !r.builtin).find(hit));
    let lifted = configured.is_some_and(|r| {
        r.effect == Effect::Allow && {
            let written = from_home(&r.resource).unwrap_or_else(|| r.resource.clone());
            explicit_for(&written, layers, &matches_rule)
        }
    });
    if lifted {
        judged
    } else {
        guard.effect
    }
}

/// Whether a configured allow written as `pattern` names a guarded path
/// explicitly: it has no wildcard (one path), or it is, letter for letter,
/// the pattern of a guard matching the request (`~/.ssh/*` written by
/// hand). `read * allow` or `read ~/* allow` is neither.
fn explicit_for(pattern: &str, layers: &[&[Rule]], matches_rule: &dyn Fn(&Rule) -> bool) -> bool {
    !pattern.contains(['*', '?'])
        || layers
            .iter()
            .flat_map(|l| l.iter())
            .any(|g| g.guard && g.resource == pattern && matches_rule(g))
}

/// Whether a secret guard ([`Rule::guard`]) applies to `action` on any of
/// `resources`: the last guard matching it asks or denies. "Always" saves
/// nothing for such a request, since no saved approval can lift a guard.
pub fn guarded(action: &str, resources: &[String], rules: &[Rule]) -> bool {
    let ic = ignores_case(action);
    resources.iter().any(|res| {
        rules
            .iter()
            .rev()
            .filter(|r| r.guard && r.action == action)
            .find(|r| wildcard::matches(res, &r.resource, ic))
            .is_some_and(|r| r.effect != Effect::Allow)
    })
}

/// Combined effect for a request over several resources: any deny denies,
/// otherwise any ask asks.
pub fn evaluate_all(action: &str, resources: &[String], rules: &[Rule]) -> Effect {
    evaluate_all_layers(action, resources, &[rules])
}

fn evaluate_all_layers(action: &str, resources: &[String], layers: &[&[Rule]]) -> Effect {
    let effects: Vec<Effect> = resources
        .iter()
        .map(|r| evaluate_layers(action, r, layers))
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
    /// A policy statement denies it; its message.
    Policy { message: String },
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
            Denied::Policy { message } => write!(f, "Permission denied: {message}"),
            Denied::Declined { feedback: None } => write!(f, "The user declined this request."),
            Denied::Declined { feedback: Some(fb) } => {
                write!(f, "The user declined this request and said: {fb}")
            }
        }
    }
}

impl std::error::Error for Denied {}

/// A project's saved approvals read once (see [`Gate::durable_now`]).
pub struct DurableNow(Vec<Rule>, Vec<Rule>);

/// Rules granted to a session from outside its configuration for as long as
/// something holds them (a browser attachment's: see
/// [`crate::plugin::browser`]). They are read afresh at every request and
/// judged like saved approvals: after the configured, built-in and policy
/// denies, which they never lift, and never over a secret guard. Only their
/// plain allows for the [`GRANTABLE`] actions count ([`Gate::granted`]).
pub trait Granted: Send + Sync {
    fn rules(&self) -> Vec<Rule>;
}

/// The actions a [`Granted`] source may grant: a browser attachment's
/// (`browser` and `skill`, see [`crate::plugin::browser::rules`]), each
/// matched literally, so a grant whose action is a pattern is ignored.
pub const GRANTABLE: [&str; 2] = ["browser", "skill"];

/// Approvals saved by "always" answers. Shared, so a session rebuilt from
/// the store (after an interrupt, undo or agent switch) keeps them.
pub type Approvals = Arc<Mutex<Vec<Rule>>>;

/// The rules in force for a session plus approvals saved during it. A
/// clone shares the saved approvals and the asker.
#[derive(Clone)]
pub struct Gate {
    rules: Vec<Rule>,
    saved: Approvals,
    asker: Arc<dyn Asker>,
    /// Where "always" answers are kept for the project, and its key.
    durable: Option<(Arc<crate::approvals::Book>, String)>,
    /// Rules granted for a while ([`Granted`]).
    granted: Option<Arc<dyn Granted>>,
}

impl Gate {
    pub fn new(rules: Vec<Rule>, asker: Arc<dyn Asker>) -> Self {
        Gate {
            rules,
            saved: Approvals::default(),
            asker,
            durable: None,
            granted: None,
        }
    }

    /// Judge requests with the rules `granted` holds at the time, too (see
    /// [`Granted`]), in place of any source set before.
    pub fn set_granted(&mut self, granted: Option<Arc<dyn Granted>>) {
        self.granted = granted;
    }

    /// The source of granted rules, to share with a copy of this gate.
    pub fn granted_source(&self) -> Option<Arc<dyn Granted>> {
        self.granted.clone()
    }

    /// The rules granted now: only plain allows for an action in
    /// [`GRANTABLE`], named literally, so a grant can add what the session
    /// may do only there, and never mark a rule built-in, a guard or a
    /// policy statement. A grant for `*`, `webfetch` or any other action
    /// is ignored.
    pub fn granted(&self) -> Vec<Rule> {
        let Some(g) = &self.granted else {
            return vec![];
        };
        g.rules()
            .into_iter()
            .filter(|r| {
                r.effect == Effect::Allow
                    && !r.builtin
                    && !r.guard
                    && r.policy.is_none()
                    && GRANTABLE.contains(&r.action.as_str())
            })
            .collect()
    }

    /// Keep "always" answers in `book` for the project `project`, and judge
    /// requests with the approvals saved there (read afresh each time, so
    /// an edit or removal applies at the next request).
    pub fn set_durable(&mut self, book: Arc<crate::approvals::Book>, project: String) {
        self.durable = Some((book, project));
    }

    /// The project's saved approvals, as rules.
    fn durable_rules(&self) -> Vec<Rule> {
        match &self.durable {
            Some((b, p)) => b.rules(p),
            None => vec![],
        }
    }

    /// The approvals saved so far; later "always" answers join them too.
    pub fn approvals(&self) -> Approvals {
        self.saved.clone()
    }

    /// Use `saved` (another gate's approvals) from now on. Approvals saved
    /// here before are kept in it.
    pub fn share_approvals(&mut self, saved: Approvals) {
        if Arc::ptr_eq(&self.saved, &saved) {
            return;
        }
        let mine = std::mem::take(&mut *self.saved.lock().unwrap());
        {
            let mut all = saved.lock().unwrap();
            for r in mine {
                if !all.contains(&r) {
                    all.push(r);
                }
            }
        }
        self.saved = saved;
    }

    /// Replace the configured rules; saved approvals and the asker stay.
    pub fn set_rules(&mut self, rules: Vec<Rule>) {
        self.rules = rules;
    }

    /// Replace who answers permission requests (rules and "always" answers stay).
    pub fn set_asker(&mut self, asker: Arc<dyn Asker>) {
        self.asker = asker;
    }

    /// The asker, to share with child sessions.
    pub fn asker(&self) -> Arc<dyn Asker> {
        self.asker.clone()
    }

    /// Configured rules, then saved approvals. To judge a request, use
    /// [`Gate::allows`] or [`Gate::assert`], which check configured denies
    /// first; evaluating this list alone lets an approval hide a deny.
    pub fn rules(&self) -> Vec<Rule> {
        let mut all = self.rules.clone();
        all.extend(self.saved.lock().unwrap().iter().cloned());
        all.extend(self.durable_rules());
        all
    }

    /// Whether a secret guard applies to the request (see [`guarded`]).
    pub fn guarded(&self, action: &str, resources: &[String]) -> bool {
        guarded(action, resources, &self.rules)
    }

    /// The configured rules (agent, config, session), without saved approvals.
    pub fn configured(&self) -> &[Rule] {
        &self.rules
    }

    /// Point the session's tool-output allowance at session `to` (see
    /// [`rescope_tool_output`]).
    pub fn rescope_tool_output(&mut self, from: &str, to: &str) {
        rescope_tool_output(&mut self.rules, from, to);
    }

    /// The approvals saved by "always" answers.
    pub fn saved(&self) -> Vec<Rule> {
        let mut all = self.saved.lock().unwrap().clone();
        all.extend(self.durable_rules());
        all
    }

    /// Allowed by rules alone, without asking. A configured deny wins over
    /// a saved approval, as in [`Gate::assert`].
    pub fn allows(&self, action: &str, resources: &[String]) -> bool {
        self.allows_given(action, resources, &self.durable_now())
    }

    /// The project's saved approvals as they are now, to judge many
    /// resources of one request (a search's files) with
    /// [`Gate::allows_given`] without reading them again for each.
    pub fn durable_now(&self) -> DurableNow {
        DurableNow(self.durable_rules(), self.granted())
    }

    /// [`Gate::allows`] with the project's saved approvals read before
    /// ([`Gate::durable_now`]).
    pub fn allows_given(&self, action: &str, resources: &[String], durable: &DurableNow) -> bool {
        if policy_denial(action, resources, &self.rules).is_some() {
            return false;
        }
        if resources
            .iter()
            .any(|r| evaluate(action, r, &self.rules) == Effect::Deny)
        {
            return false;
        }
        let saved = self.saved.lock().unwrap();
        evaluate_all_layers(
            action,
            resources,
            &[&self.rules, &saved, &durable.1, &durable.0],
        ) == Effect::Allow
    }

    /// Refuse only when a configured rule denies the request; never asks.
    /// Used for follow-up requests the user already approved the start of,
    /// such as `webfetch` redirect targets (OpenCode's `assertNotDenied`).
    pub fn assert_not_denied(&self, action: &str, resources: &[String]) -> Result<(), Denied> {
        if let Some(message) = policy_denial(action, resources, &self.rules) {
            return Err(Denied::Policy { message });
        }
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
        if let Some(message) = policy_denial(&req.action, &req.resources, &self.rules) {
            return Err(Denied::Policy { message });
        }
        for r in &req.resources {
            if evaluate(&req.action, r, &self.rules) == Effect::Deny {
                return Err(Denied::Blocked {
                    action: req.action.clone(),
                    resource: r.clone(),
                });
            }
        }
        let durable = self.durable_rules();
        // Granted rules are judged here, after the configured denies above:
        // like a saved approval, a grant cannot lift one or a secret guard.
        let granted = self.granted();
        let effect = {
            let saved = self.saved.lock().unwrap();
            evaluate_all_layers(
                &req.action,
                &req.resources,
                &[&self.rules, &saved, &granted, &durable],
            )
        };
        match effect {
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
                    // No saved approval lifts a secret guard: save nothing.
                    let saves = if self.guarded(&req.action, &req.resources) {
                        Vec::new()
                    } else {
                        safe_saves(req)
                    };
                    // Kept for the project when there is a database; else
                    // for this session. A failed write keeps it for the
                    // session.
                    let kept = match &self.durable {
                        Some((b, p)) => b.add(p, &req.action, &saves).is_ok(),
                        None => false,
                    };
                    if !kept {
                        let mut saved = self.saved.lock().unwrap();
                        for s in saves {
                            saved.push(Rule::new(&req.action, &s, Effect::Allow));
                        }
                    }
                    Ok(())
                }
                Reply::Reject(feedback) => Err(Denied::Declined { feedback }),
            },
        }
    }
}

/// What answering "always" to `req` saves, as resource patterns for its
/// action: shown in permission prompts.
pub fn always_saves(req: &Request) -> Vec<String> {
    safe_saves(req)
}

/// A one-line description of [`always_saves`], for prompts.
pub fn always_summary(req: &Request) -> String {
    let saves = always_saves(req);
    if saves.is_empty() {
        "always: saves nothing (same as once)".into()
    } else {
        format!("always saves {}: {}", req.action, saves.join(", "))
    }
}

/// The rules "always" saves for `req`. An `external_directory` approval is
/// saved for the directory (`<dir>/*`), except when that directory is `/`,
/// the home directory or above it: approving `~/.zshrc` must not approve all
/// of `$HOME`. Then the file itself is saved, and a directory not at all.
fn safe_saves(req: &Request) -> Vec<String> {
    if matches!(req.action.as_str(), "webfetch" | "openapi") {
        return req
            .save
            .iter()
            .filter(|s| exact_origin(s))
            .cloned()
            .collect();
    }
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
        if s == "*" || broad(base) || shared_dir(std::path::Path::new(base)) {
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

/// Whether a `webfetch` (`<url>`) or `openapi` (`<METHOD> <url>`) pattern
/// names one origin: a scheme and host with no `*` or `?` in them. Any
/// other is never saved by "always" (`https://*/*` would allow every URL).
fn exact_origin(pattern: &str) -> bool {
    let url = pattern.split_once(' ').map_or(pattern, |(_, u)| u);
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    // Up to the first `/`: a `?` before it is a wildcard in the host.
    let authority = rest.split('/').next().unwrap_or("");
    !scheme.is_empty()
        && !authority.is_empty()
        && !scheme.contains(['*', '?'])
        && !authority.contains(['*', '?'])
        && !pattern.starts_with('*')
}

/// Whether other local users can put files in `dir`: a system temporary
/// directory, or (for a directory that does not exist yet, its nearest
/// existing ancestor) one that is world-writable or owned by a user other
/// than this one or root. Approving a file there must not approve the
/// directory: someone else decides what else is in it.
pub fn shared_dir(dir: &std::path::Path) -> bool {
    const TMP: [&str; 4] = ["/tmp", "/private/tmp", "/var/tmp", "/private/var/tmp"];
    let shown = dir.display().to_string();
    if TMP.contains(&shown.trim_end_matches('/')) {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let Some(m) = dir.ancestors().find_map(|d| std::fs::metadata(d).ok()) else {
            return false;
        };
        // SAFETY: geteuid has no preconditions.
        let me = unsafe { libc::geteuid() };
        m.mode() & 0o002 != 0 || (m.uid() != me && m.uid() != 0)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Narrow the base rules' tool-output allowance (see [`base_rules`]) to one
/// session's own directory, in place, so rule order is kept.
pub fn scope_tool_output(rules: &mut [Rule], session_id: &str) {
    let all = crate::paths::tool_output_dir().display().to_string();
    let mine = crate::paths::session_output_dir(session_id)
        .display()
        .to_string();
    let all_glob = format!("{all}/*");
    for r in rules.iter_mut() {
        if r.action == "external_directory" {
            if r.resource == all {
                r.resource = mine.clone();
            } else if r.resource == all_glob {
                r.resource = format!("{mine}/*");
            }
        }
    }
}

/// Move a scoping made by [`scope_tool_output`] from session `from` to
/// session `to`, for a session given another id after it was made.
pub fn rescope_tool_output(rules: &mut [Rule], from: &str, to: &str) {
    let old = crate::paths::session_output_dir(from).display().to_string();
    let new = crate::paths::session_output_dir(to).display().to_string();
    let old_glob = format!("{old}/*");
    for r in rules.iter_mut() {
        if r.action == "external_directory" {
            if r.resource == old {
                r.resource = new.clone();
            } else if r.resource == old_glob {
                r.resource = format!("{new}/*");
            }
        }
    }
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
    rules.extend(secret_guards());
    rules.extend([
        // Generating images and videos spends money with the provider: ask.
        Rule::new("experimental_image_generate", "*", Effect::Ask),
        Rule::new("experimental_video_generate", "*", Effect::Ask),
    ]);
    // Reference checkouts are read-only to tools: CodeSmith runs git in
    // them, and a planted config or hook must not get there.
    for dir in reference_cache_dirs() {
        rules.push(Rule::new("edit", &dir, Effect::Deny));
        rules.push(Rule::new("edit", &format!("{dir}/*"), Effect::Deny));
    }
    rules
}

/// Built-in `read` guards on files that hold secrets, for every agent
/// ([`Rule::guard`]): reading them asks, whatever the other rules say, and
/// no saved approval or "always" answer lifts that; only a configured allow
/// naming the path does. `.env` files (not `.env.example`), Git
/// credentials anywhere, and in the home folder `~/.ssh`, `~/.aws`,
/// `~/.gnupg`, `~/.minisign`, `~/.netrc`, GitHub CLI, Docker and
/// Kubernetes credentials, `~/.npmrc` and `~/.pypirc`; and CodeSmith's own
/// saved keys (`auth.json`, `mcp-auth.json`) and global config file. Each
/// folder is named as written and as resolved, so a symlink to it, or a
/// path through one, is caught by its real name.
pub fn secret_guards() -> Vec<Rule> {
    let resolved = |dirs: Vec<std::path::PathBuf>| -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        for d in dirs {
            let real = d.canonicalize().ok();
            for d in std::iter::once(d).chain(real) {
                if !out.contains(&d) {
                    out.push(d);
                }
            }
        }
        out
    };
    let mut homes: Vec<std::path::PathBuf> = dirs::home_dir().into_iter().collect();
    homes = resolved(homes);
    let mut configs = vec![crate::paths::config_dir()];
    // The GitHub CLI keeps its token under the XDG config folder.
    let gh: Vec<std::path::PathBuf> = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(|v| std::path::PathBuf::from(v).join("gh"))
        .into_iter()
        .collect();
    configs = resolved(configs);
    secret_guards_for(
        &homes,
        &resolved(vec![crate::paths::data_dir()]),
        &configs,
        &resolved(gh),
    )
}

/// [`secret_guards`] for the given home, data, config and extra GitHub CLI
/// folders (each as written and resolved).
pub fn secret_guards_for(
    homes: &[std::path::PathBuf],
    data: &[std::path::PathBuf],
    config: &[std::path::PathBuf],
    gh: &[std::path::PathBuf],
) -> Vec<Rule> {
    let ask = |r: &str| Rule::new("read", r, Effect::Ask).into_guard();
    let shown = |p: &std::path::Path| p.display().to_string().trim_end_matches('/').to_string();
    // `.env.example` is the one carve-out; folder guards come after it, so
    // `~/.ssh/x.env.example` still asks.
    let mut rules = vec![
        ask("*.env"),
        ask("*.env.*"),
        Rule::new("read", "*.env.example", Effect::Allow).into_guard(),
        ask(".git-credentials"),
        ask("*/.git-credentials"),
    ];
    for h in homes {
        let h = shown(h);
        for dir in [".ssh", ".aws", ".gnupg", ".minisign"] {
            rules.push(ask(&format!("{h}/{dir}")));
            rules.push(ask(&format!("{h}/{dir}/*")));
        }
        for file in [
            ".netrc",
            ".config/gh/hosts.yml",
            ".docker/config.json",
            ".kube/config",
            ".npmrc",
            ".pypirc",
        ] {
            rules.push(ask(&format!("{h}/{file}")));
        }
    }
    for g in gh {
        rules.push(ask(&format!("{}/hosts.yml", shown(g))));
    }
    for d in data {
        let d = shown(d);
        // `*` after the name: the `.bak` copies and lock files too.
        rules.push(ask(&format!("{d}/auth.json*")));
        rules.push(ask(&format!("{d}/mcp-auth.json*")));
    }
    for c in config {
        let c = shown(c);
        for name in crate::config::NAMES {
            rules.push(ask(&format!("{c}/{name}")));
        }
    }
    let mut out: Vec<Rule> = Vec::new();
    for r in rules {
        if !out.contains(&r) {
            out.push(r);
        }
    }
    out
}

/// The reference checkout cache and its state, as written and resolved.
fn reference_cache_dirs() -> Vec<String> {
    let mut out = Vec::new();
    for dir in [
        crate::paths::data_dir().join("repos"),
        crate::paths::data_dir().join("repos-state"),
    ] {
        let real = dir.canonicalize().ok();
        for d in std::iter::once(dir).chain(real) {
            let d = d.display().to_string();
            if !out.contains(&d) {
                out.push(d);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// The judgment `Gate::allows` made before saved approvals could be
    /// read once per search: every layer concatenated, read afresh.
    fn allows_naive(g: &Gate, action: &str, resources: &[String]) -> bool {
        policy_denial(action, resources, g.configured()).is_none()
            && !resources
                .iter()
                .any(|r| evaluate(action, r, g.configured()) == Effect::Deny)
            && evaluate_all(action, resources, &g.rules()) == Effect::Allow
    }

    fn book() -> (tempfile::TempDir, Arc<crate::approvals::Book>) {
        let d = tempfile::tempdir().unwrap();
        let b = crate::approvals::Book::open(&d.path().join("sessions.db")).unwrap();
        (d, Arc::new(b))
    }

    /// Saved approvals read once (`durable_now`) judge exactly as reading
    /// them for each check does, on seeded random rules, policies, saved
    /// approvals and requests.
    #[test]
    fn durable_snapshot_judges_as_reading_each_time() {
        let (_d, book) = book();
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut rnd = move |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        let actions = ["read", "edit", "shell"];
        let patterns = [
            "*",
            "src/*",
            "src/**",
            "*.rs",
            "*.env",
            "/p/secrets/*",
            "docs/a.md",
            "~/notes/*",
            "git *",
            "rm *",
        ];
        let resources = [
            "src/a.rs",
            "src/x/b.ts",
            "docs/a.md",
            ".env",
            "/p/secrets/k",
            "git status",
            "rm -rf x",
            "README.md",
        ];
        let effects = [Effect::Allow, Effect::Ask, Effect::Deny];
        let mut seen = [0usize; 2];
        for round in 0..300 {
            let project = format!("p{round}");
            let mut rules = Vec::new();
            for _ in 0..rnd(8) {
                let r = Rule::new(
                    actions[rnd(3)],
                    patterns[rnd(patterns.len())],
                    effects[rnd(3)],
                );
                rules.push(if rnd(4) == 0 { r.into_builtin() } else { r });
            }
            if rnd(3) == 0 {
                let e = [PolicyEffect::Allow, PolicyEffect::Deny][rnd(2)];
                let res = format!("{}:{}", actions[rnd(3)], patterns[rnd(patterns.len())]);
                rules.extend(Policy::tool(e, &res).rule());
            }
            for _ in 0..rnd(3) {
                let saves: Vec<String> = (0..1 + rnd(3))
                    .map(|_| patterns[rnd(patterns.len())].to_string())
                    .collect();
                book.add(&project, actions[rnd(3)], &saves).unwrap();
            }
            let mut g = Gate::new(rules, Arc::new(Fixed(Reply::Once)));
            g.set_durable(book.clone(), project);
            let now = g.durable_now();
            for _ in 0..10 {
                let action = actions[rnd(3)];
                let res: Vec<String> = (0..1 + rnd(3))
                    .map(|_| resources[rnd(resources.len())].to_string())
                    .collect();
                let want = allows_naive(&g, action, &res);
                seen[want as usize] += 1;
                assert_eq!(g.allows(action, &res), want, "{action} {res:?}");
                assert_eq!(g.allows_given(action, &res, &now), want, "{action} {res:?}");
            }
        }
        // Both answers came up often enough to mean something.
        assert!(seen[0] > 100 && seen[1] > 100, "{seen:?}");
    }

    /// A snapshot keeps the approvals as they were when it was taken; a
    /// plain check, or a new snapshot, sees an approval added or removed
    /// since.
    #[test]
    fn durable_snapshot_is_taken_once_and_checks_read_afresh() {
        let (_d, book) = book();
        let mut g = Gate::new(
            vec![Rule::new("read", "*", Effect::Ask)],
            Arc::new(Fixed(Reply::Once)),
        );
        g.set_durable(book.clone(), "p".into());
        let res = vec!["src/a.rs".to_string()];
        let before = g.durable_now();
        assert!(!g.allows("read", &res));
        let saved = book.add("p", "read", &["*.rs".into()]).unwrap();
        assert!(g.allows("read", &res));
        assert!(!g.allows_given("read", &res, &before));
        let after = g.durable_now();
        assert!(g.allows_given("read", &res, &after));
        // Another project's approvals never count.
        book.add("q", "read", &["*".into()]).unwrap();
        assert!(!g.allows("read", &["b.ts".to_string()]));
        book.remove(&saved[0].id).unwrap();
        assert!(!g.allows("read", &res));
        assert!(g.allows_given("read", &res, &after));
        assert!(!g.allows_given("read", &res, &g.durable_now()));
    }

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
    fn base_rules_ask_before_paid_media_generation() {
        let r = base_rules();
        assert_eq!(
            evaluate("experimental_image_generate", "*", &r),
            Effect::Ask
        );
        assert_eq!(
            evaluate("experimental_video_generate", "*", &r),
            Effect::Ask
        );
        assert_eq!(evaluate("websearch", "*", &r), Effect::Allow);
    }

    #[test]
    fn base_rules_keep_reference_checkouts_read_only() {
        let r = base_rules();
        let repos = crate::paths::data_dir().join("repos");
        let file = repos.join("github.com/o/r/.git/config");
        for res in [repos.display().to_string(), file.display().to_string()] {
            assert_eq!(evaluate("edit", &res, &r), Effect::Deny, "{res}");
            assert_eq!(evaluate("read", &res, &r), Effect::Allow, "{res}");
        }
        let state = crate::paths::data_dir().join("repos-state/x.json");
        assert_eq!(
            evaluate("edit", &state.display().to_string(), &r),
            Effect::Deny
        );
        assert_eq!(evaluate("edit", "src/main.rs", &r), Effect::Allow);
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

    /// "Always" for a URL saves a rule for one origin only: a pattern with
    /// a wildcard in its scheme or host is never saved.
    #[test]
    fn always_never_saves_a_wildcard_origin() {
        let saves = |action: &str, save: &[&str]| {
            safe_saves(&Request {
                action: action.into(),
                resources: vec!["x".into()],
                save: save.iter().map(|s| s.to_string()).collect(),
                title: String::new(),
            })
        };
        assert_eq!(
            saves("webfetch", &["https://a.dev", "https://a.dev/*"]),
            ["https://a.dev", "https://a.dev/*"]
        );
        assert_eq!(
            saves("openapi", &["GET https://a.dev/*"]),
            ["GET https://a.dev/*"]
        );
        for wild in [
            "*",
            "https://*github.com",
            "https://*github.com/*",
            "https://*/*",
            "https://a?/*",
            "http*://a.dev/*",
            "https://a.dev:*/*",
            "a.dev/*",
        ] {
            assert!(saves("webfetch", &[wild]).is_empty(), "{wild}");
            assert!(
                saves("openapi", &[&format!("GET {wild}")]).is_empty(),
                "{wild}"
            );
        }
        assert!(saves("openapi", &["* https://a.dev/*"]).is_empty());
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
    fn shared_approvals_outlive_the_gate_that_saved_them() {
        let n = Arc::new(AtomicUsize::new(0));
        let rules = vec![Rule::new("*", "*", Effect::Ask)];
        let first = Gate::new(rules.clone(), Arc::new(Fixed(Reply::Always)));
        first.assert(&req("shell", &["ls"])).unwrap();
        let kept = first.approvals();
        drop(first);
        // A rebuilt gate with a fresh approval of its own, then the kept ones.
        let mut second = Gate::new(rules.clone(), Arc::new(Fixed(Reply::Always)));
        second.assert(&req("edit", &["a.rs"])).unwrap();
        second.share_approvals(kept.clone());
        second.set_asker(Arc::new(Counting(n.clone(), Reply::Reject(None))));
        second.assert(&req("shell", &["ls"])).unwrap();
        second.assert(&req("edit", &["a.rs"])).unwrap();
        assert_eq!(n.load(Ordering::SeqCst), 0, "both approvals hold");
        // Sharing the same approvals again changes nothing.
        second.share_approvals(kept.clone());
        assert_eq!(kept.lock().unwrap().len(), 2);
        // A new answer joins the shared list.
        let mut third = Gate::new(rules, Arc::new(Fixed(Reply::Always)));
        third.share_approvals(kept.clone());
        third.assert(&req("websearch", &["x"])).unwrap();
        assert_eq!(kept.lock().unwrap().len(), 3);
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
    // - "Always" approvals are kept per project in the session database
    //   (`crate::approvals`), as in OpenCode; a gate without one keeps them
    //   for the session (`tests/oc_saved_approvals.rs`).
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

#[cfg(test)]
mod scoping {
    use super::*;

    #[test]
    fn a_session_reaches_only_its_own_tool_output() {
        let mut rules = base_rules();
        scope_tool_output(&mut rules, "ses_a");
        let root = crate::paths::tool_output_dir().display().to_string();
        let mine = crate::paths::session_output_dir("ses_a")
            .display()
            .to_string();
        let theirs = crate::paths::session_output_dir("ses_b")
            .display()
            .to_string();
        assert_eq!(
            evaluate("external_directory", &format!("{mine}/tool_1"), &rules),
            Effect::Allow
        );
        assert_eq!(
            evaluate("external_directory", &format!("{theirs}/tool_1"), &rules),
            Effect::Ask
        );
        assert_eq!(
            evaluate("external_directory", &format!("{root}/tool_1"), &rules),
            Effect::Ask
        );
        assert_eq!(
            crate::paths::session_output_dir("../x"),
            crate::paths::tool_output_dir().join("___x")
        );
    }

    #[test]
    fn allows_checks_configured_denies_before_saved_approvals() {
        let g = Gate::new(
            vec![
                Rule::new("*", "*", Effect::Allow),
                Rule::new("edit", "secret/*", Effect::Deny),
            ],
            Arc::new(Fixed(Reply::Once)),
        );
        g.approvals()
            .lock()
            .unwrap()
            .push(Rule::new("edit", "*", Effect::Allow));
        assert!(!g.allows("edit", &["secret/a".into()]));
        assert!(g.allows("edit", &["open/a".into()]));
    }
}
