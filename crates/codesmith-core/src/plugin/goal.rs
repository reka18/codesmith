//! The goal plugin, built in (OpenCode's `plugin/goal.ts`, MIT): `/goal
//! <objective>` keeps a session working until a reviewer, a side request
//! with the session's context, finds the objective met in the evidence the
//! conversation shows.
//!
//! When the session would go idle with an active goal, the reviewer answers
//! with one JSON verdict: `done` completes the goal, `continue` sends a
//! continuation in the same turn, `blocked` or `paused` pause it, and
//! `wait` (offered only while the session's own background shells run)
//! lets the session go idle until a shell's notice wakes it. The goal
//! pauses after too many rounds, after three rounds in a row without
//! progress (the same reason again, or no tool call since the last
//! kickoff or continuation), when the reviewer gives no usable verdict
//! twice, when the session fails or is stopped, and when nothing ran for
//! [`WAIT_LIMIT`] while it waited. A goal an earlier process left active
//! comes back paused. After a compaction, until a continuation restates the
//! objective, each request reminds the model of it (not saved).
//!
//! The objective is user input and the reviewer's reply is model output:
//! every prompt wraps the objective (escaped) as untrusted data, and the
//! verdict is parsed strictly; anything else counts as no verdict.
//!
//! Turn it off with `"plugins": {"goal": false}`.

use super::host::{Host, HostError};
use super::{
    BoxFuture, Command, CommandContext, Context, Event, Hook, IdleAction, Plugin, RequestKind,
    SessionIdle,
};
use codesmith_ai::{Message, Part, Role};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The goal plugin [`super::builtin`] installs, for a front end to show
/// a session's goal (OpenCode's goal RPC).
pub fn shared() -> &'static GoalPlugin {
    static SHARED: std::sync::OnceLock<GoalPlugin> = std::sync::OnceLock::new();
    SHARED.get_or_init(GoalPlugin::new)
}

/// The plugin's id, in config and storage.
pub const ID: &str = "goal";
pub const MAX_OBJECTIVE: usize = 4_000;
pub const MAX_ROUNDS: u32 = 50;
pub const MAX_STALLS: u32 = 3;
/// How long an active goal may sit idle, waiting on background work that
/// never reports, before it pauses.
pub const WAIT_LIMIT: Duration = Duration::from_secs(30 * 60);

const CONTINUATION: &str = include_str!("goal/continuation.txt");
const JUDGE: &str = include_str!("goal/judge.txt");
const KICKOFF: &str = include_str!("goal/kickoff.txt");
const REMINDER: &str = include_str!("goal/reminder.txt");
const USAGE: &str = "Usage: /goal <objective> | pause | resume | clear";
/// Storage key prefix of a session's goal. Children and forks start without one.
const PREFIX: &str = "session:";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Active,
    Paused,
    Complete,
}

/// A session's goal (OpenCode's `Goal.State`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Goal {
    pub id: String,
    pub objective: String,
    pub status: Status,
    pub rounds: u32,
    pub max_rounds: u32,
    /// The process that set (or resumed) it.
    pub boot: String,
    pub updated_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stalls: Option<u32>,
}

/// A goal written: the session's new goal, or `None` once cleared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub session_id: String,
    pub goal: Option<Goal>,
}

#[derive(Debug, Deserialize)]
struct Verdict {
    verdict: String,
    reason: String,
    #[serde(default)]
    next_step: Option<String>,
}

enum Judged {
    Verdict(Verdict),
    Overflow,
    Unusable,
}

/// This process: a goal armed by an earlier one comes back paused instead
/// of resuming on its own.
fn boot() -> &'static str {
    static BOOT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    BOOT.get_or_init(|| {
        use rand::Rng;
        format!("{:032x}", rand::thread_rng().gen::<u128>())
    })
}

fn now() -> i64 {
    crate::store::now()
}

struct Inner {
    wait_limit: Duration,
    changes: tokio::sync::broadcast::Sender<Change>,
    /// The wait timers of goals idle on background work, by session.
    timers: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    /// One compaction per round when the reviewer's request is too large:
    /// the goal and round it was made for, by session.
    compacted: Mutex<HashMap<String, String>>,
    swept: std::sync::atomic::AtomicBool,
}

/// The goal plugin.
#[derive(Clone)]
pub struct GoalPlugin(Arc<Inner>);

impl Default for GoalPlugin {
    fn default() -> Self {
        GoalPlugin::new()
    }
}

impl GoalPlugin {
    pub fn new() -> GoalPlugin {
        GoalPlugin::with_wait_limit(WAIT_LIMIT)
    }

    /// With another wait limit (tests).
    pub fn with_wait_limit(wait_limit: Duration) -> GoalPlugin {
        GoalPlugin(Arc::new(Inner {
            wait_limit,
            changes: tokio::sync::broadcast::channel(64).0,
            timers: Default::default(),
            compacted: Default::default(),
            swept: Default::default(),
        }))
    }

    /// Session `id`'s goal.
    pub fn get(&self, session_id: &str) -> Option<Goal> {
        self.0.read(session_id).ok().flatten()
    }

    /// Every goal written from now on (OpenCode publishes them over RPC).
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Change> {
        self.0.changes.subscribe()
    }

    /// Pause the goals an earlier process left active, and drop those of
    /// sessions that are gone. Runs once, when config is first loaded.
    pub fn sweep(&self) {
        let _ = self.0.sweep();
    }
}

fn host() -> Result<Arc<Host>, HostError> {
    super::host()
}

impl Inner {
    fn read(&self, session_id: &str) -> Result<Option<Goal>, HostError> {
        let v = host()?.storage(ID).get(&format!("{PREFIX}{session_id}"))?;
        Ok(v.and_then(|v| serde_json::from_value(v).ok()))
    }

    fn write(&self, session_id: &str, goal: Option<&Goal>) -> Result<(), HostError> {
        let h = host()?;
        let key = format!("{PREFIX}{session_id}");
        match goal {
            Some(g) => h
                .storage(ID)
                .set(&key, &serde_json::to_value(g).unwrap_or(Value::Null))?,
            None => h.storage(ID).remove(&key)?,
        }
        let _ = self.changes.send(Change {
            session_id: session_id.to_string(),
            goal: goal.cloned(),
        });
        Ok(())
    }

    /// The loop's own writes apply only to the goal they were decided on:
    /// a pause, clear or new goal that landed since wins.
    fn replace(&self, session_id: &str, expected: &Goal, next: &Goal) -> Result<bool, HostError> {
        let current = self.read(session_id)?;
        let same = current.as_ref().is_some_and(|c| {
            c.id == expected.id
                && c.updated_at == expected.updated_at
                && c.status == expected.status
        });
        if same {
            self.write(session_id, Some(next))?;
        }
        Ok(same)
    }

    fn settle(
        &self,
        session_id: &str,
        goal: &Goal,
        status: Status,
        reason: &str,
    ) -> Result<(), HostError> {
        let next = Goal {
            status,
            reason: Some(reason.to_string()),
            updated_at: now().max(goal.updated_at + 1),
            ..goal.clone()
        };
        self.replace(session_id, goal, &next).map(|_| ())
    }

    fn pause_active(&self, session_id: &str, reason: &str) -> Result<(), HostError> {
        match self.read(session_id)? {
            Some(g) if g.status == Status::Active => {
                self.settle(session_id, &g, Status::Paused, reason)
            }
            _ => Ok(()),
        }
    }

    fn sweep(&self) -> Result<(), HostError> {
        use std::sync::atomic::Ordering;
        if self.swept.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let h = host()?;
        for (key, value) in h.storage(ID).scan(PREFIX)? {
            let id = &key[PREFIX.len()..];
            if h.get(id).is_err() {
                h.storage(ID).remove(&key)?;
                continue;
            }
            let Ok(goal) = serde_json::from_value::<Goal>(value) else {
                continue;
            };
            if goal.status == Status::Active && goal.boot != boot() {
                self.settle(
                    id,
                    &goal,
                    Status::Paused,
                    "The server restarted while the goal was active",
                )?;
            }
        }
        Ok(())
    }

    async fn judge(&self, idle: &SessionIdle<'_>, goal: &Goal, shells: &[String]) -> Judged {
        let background = if shells.is_empty() {
            "No background shells are running.".to_string()
        } else {
            let mut lines = vec!["Background shells still running:".to_string()];
            lines.extend(shells.iter().map(|c| format!("- {c}")));
            lines.push(String::new());
            lines.push("Answer \"wait\" only when the goal needs the result of one of these shells. Never wait for a long-running server such as a dev server.".into());
            lines.join("\n")
        };
        let wait = if shells.is_empty() {
            ""
        } else {
            "{\"verdict\":\"wait\",\"reason\":\"<the running shell whose result the goal needs>\"}\n"
        };
        let prompt = render(
            JUDGE,
            &[
                ("objective", &escape(&goal.objective)),
                ("background", &background),
                ("wait", wait),
            ],
        );
        for _ in 0..2 {
            match idle.generate(&prompt).await {
                Err(super::GenerateError::Overflow(_)) => return Judged::Overflow,
                Err(super::GenerateError::Failed(_)) => {}
                Ok(text) => match parse_verdict(&text) {
                    // "wait" is offered only while a shell runs; without
                    // one, nothing would wake the session to judge again.
                    Some(v) if v.verdict == "wait" && shells.is_empty() => {}
                    Some(v) => return Judged::Verdict(v),
                    None => {}
                },
            }
        }
        Judged::Unusable
    }

    async fn idle(&self, idle: &SessionIdle<'_>) -> Result<IdleAction, HostError> {
        let id = idle.session_id;
        let Some(goal) = self.read(id)?.filter(|g| g.status == Status::Active) else {
            return Ok(IdleAction::Idle);
        };
        if goal.boot != boot() {
            self.settle(
                id,
                &goal,
                Status::Paused,
                "The server restarted while the goal was active",
            )?;
            return Ok(IdleAction::Idle);
        }
        if goal.rounds >= goal.max_rounds {
            let why = format!("Reached the limit of {} rounds", goal.max_rounds);
            self.settle(id, &goal, Status::Paused, &why)?;
            return Ok(IdleAction::Idle);
        }
        let shells: Vec<String> = idle
            .background
            .iter()
            .map(|b| match b {
                super::Background::Shell { command, .. } => command.clone(),
            })
            .collect();
        let judged = self.judge(idle, &goal, &shells).await;
        if !matches!(judged, Judged::Overflow) {
            self.compacted.lock().unwrap().remove(id);
        }
        // A pause, clear or new goal may have landed while the reviewer ran.
        let Some(latest) = self
            .read(id)?
            .filter(|g| g.id == goal.id && g.status == Status::Active)
        else {
            return Ok(IdleAction::Idle);
        };
        let verdict = match judged {
            Judged::Overflow => {
                let key = format!("{}:{}", latest.id, latest.rounds);
                let mut compacted = self.compacted.lock().unwrap();
                if compacted.get(id) == Some(&key) {
                    compacted.remove(id);
                    drop(compacted);
                    self.settle(
                        id,
                        &latest,
                        Status::Paused,
                        "The goal reviewer's request is too large for the model",
                    )?;
                    return Ok(IdleAction::Idle);
                }
                compacted.insert(id.to_string(), key);
                // The session compacts, then runs this hook again on the
                // smaller history.
                return Ok(IdleAction::Compact);
            }
            Judged::Unusable => {
                self.settle(
                    id,
                    &latest,
                    Status::Paused,
                    "The goal reviewer gave no usable verdict",
                )?;
                return Ok(IdleAction::Idle);
            }
            Judged::Verdict(v) => v,
        };
        match verdict.verdict.as_str() {
            "wait" => return Ok(IdleAction::Idle),
            "done" => {
                self.settle(id, &latest, Status::Complete, &verdict.reason)?;
                return Ok(IdleAction::Idle);
            }
            "continue" => {}
            _ => {
                self.settle(id, &latest, Status::Paused, &verdict.reason)?;
                return Ok(IdleAction::Idle);
            }
        }
        let stalled = same_reason(latest.reason.as_deref(), &verdict.reason)
            || !used_tools(idle.messages, &latest.objective);
        let stalls = if stalled {
            latest.stalls.unwrap_or(0) + 1
        } else {
            0
        };
        if stalls >= MAX_STALLS {
            let why = format!("Not making progress: {}", verdict.reason);
            self.settle(id, &latest, Status::Paused, &why)?;
            return Ok(IdleAction::Idle);
        }
        let next = Goal {
            rounds: latest.rounds + 1,
            stalls: Some(stalls),
            reason: Some(verdict.reason.clone()),
            updated_at: now().max(latest.updated_at + 1),
            ..latest.clone()
        };
        if !self.replace(id, &latest, &next)? {
            return Ok(IdleAction::Idle);
        }
        let next_step = verdict.next_step.map(|s| format!("Next step: {s}"));
        Ok(IdleAction::Continue(continuation(
            &next,
            &[Some(verdict.reason), next_step],
        )))
    }

    /// Pause the goal if nothing runs in the session or below it for the
    /// wait limit (a hung shell, a missed wake).
    fn watch(self: &Arc<Self>, session_id: &str) {
        let Some(goal) = self
            .read(session_id)
            .ok()
            .flatten()
            .filter(|g| g.status == Status::Active)
        else {
            return;
        };
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let (me, id, limit) = (self.clone(), session_id.to_string(), self.wait_limit);
        let task = rt.spawn(async move {
            tokio::time::sleep(limit).await;
            let why = format!(
                "Nothing ran for {} minutes while the goal waited on background work",
                limit.as_secs() / 60
            );
            let _ = me.settle(&id, &goal, Status::Paused, &why);
            me.timers.lock().unwrap().remove(&id);
        });
        if let Some(old) = self
            .timers
            .lock()
            .unwrap()
            .insert(session_id.to_string(), task)
        {
            old.abort();
        }
    }

    fn unwatch(&self, session_id: &str) {
        if let Some(t) = self.timers.lock().unwrap().remove(session_id) {
            t.abort();
        }
    }

    /// A step anywhere below a waiting goal restarts its timer, so a
    /// healthy subagent working past the limit keeps its goal.
    fn touch(self: &Arc<Self>, session_id: &str) {
        if self.timers.lock().unwrap().is_empty() {
            return;
        }
        let Ok(h) = host() else {
            return;
        };
        let mut current = h.get(session_id).ok().and_then(|s| s.parent_id);
        while let Some(id) = current {
            if self.timers.lock().unwrap().contains_key(&id) {
                return self.watch(&id);
            }
            current = h.get(&id).ok().and_then(|s| s.parent_id);
        }
    }
}

impl Plugin for GoalPlugin {
    fn id(&self) -> &str {
        ID
    }

    fn config(&self, _config: &mut crate::config::Config, _options: &Value) {
        self.sweep();
    }

    fn commands(&self) -> Vec<Arc<dyn Command>> {
        vec![Arc::new(GoalCommand(self.0.clone()))]
    }

    fn wants(&self, hook: Hook, session_id: &str) -> bool {
        match hook {
            Hook::Prompt | Hook::Http => false,
            Hook::Context | Hook::SessionIdle => self
                .0
                .read(session_id)
                .ok()
                .flatten()
                .is_some_and(|g| g.status == Status::Active),
        }
    }

    fn session_idle<'a>(&'a self, idle: &'a SessionIdle<'a>) -> BoxFuture<'a, IdleAction> {
        Box::pin(async move {
            match self.0.idle(idle).await {
                Ok(a) => a,
                Err(_) => {
                    let _ = self
                        .0
                        .pause_active(idle.session_id, "The goal stopped after an error");
                    IdleAction::Idle
                }
            }
        })
    }

    /// Until a continuation restates the objective, a compacted history may
    /// not show it: add it near the tail, unsaved, so the cached prefix
    /// stays the same.
    fn context(&self, ctx: &mut Context) {
        if ctx.kind != RequestKind::Primary {
            return;
        }
        let Some(goal) = self
            .0
            .read(&ctx.session_id)
            .ok()
            .flatten()
            .filter(|g| g.status == Status::Active)
        else {
            return;
        };
        let block = objective_block(&goal.objective);
        if ctx
            .messages
            .iter()
            .any(|m| m.message.role == Role::User && text_of(&m.message).contains(&block))
        {
            return;
        }
        let at = match ctx.messages.last() {
            Some(m) if m.message.role == Role::User => ctx.messages.len() - 1,
            _ => ctx.messages.len(),
        };
        ctx.messages.insert(
            at,
            super::ContextMessage {
                message: Message::user(render(
                    REMINDER,
                    &[("objective", &escape(&goal.objective))],
                )),
                metadata: None,
            },
        );
    }

    fn event(&self, e: &Event) {
        match e {
            Event::TurnStart { session_id, .. } => self.0.unwatch(session_id),
            Event::StepEnd { session_id, .. } => self.0.touch(session_id),
            Event::TurnEnd {
                session_id,
                error: None,
            } => self.0.watch(session_id),
            Event::TurnEnd {
                session_id,
                error: Some(error),
            } => {
                let _ = self
                    .0
                    .pause_active(session_id, &format!("The session failed: {error}"));
            }
            _ => {}
        }
    }

    fn interrupted(&self, session_id: &str) {
        self.0.unwatch(session_id);
        let _ = self.0.pause_active(session_id, "Stopped by the user");
    }
}

/// `/goal <objective> | pause | resume | clear`.
struct GoalCommand(Arc<Inner>);

impl Command for GoalCommand {
    fn name(&self) -> &str {
        "goal"
    }

    fn description(&self) -> Option<&str> {
        Some("keep working until an objective is met [pause|resume|clear]")
    }

    fn execute<'a>(&'a self, ctx: CommandContext) -> BoxFuture<'a, Result<Option<String>, String>> {
        Box::pin(async move { self.run(&ctx).map_err(|e| e.to_string()) })
    }
}

#[derive(Debug)]
enum CommandError {
    Usage(String),
    Host(HostError),
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommandError::Usage(m) => f.write_str(m),
            CommandError::Host(e) => e.fmt(f),
        }
    }
}

impl From<HostError> for CommandError {
    fn from(e: HostError) -> Self {
        CommandError::Host(e)
    }
}

impl GoalCommand {
    fn run(&self, ctx: &CommandContext) -> Result<Option<String>, CommandError> {
        let g = &self.0;
        let id = &ctx.session_id;
        let text = unquote(ctx.args.trim());
        let control = text.to_lowercase();
        let goal = g.read(id)?;
        let fail = |m: &str| Err(CommandError::Usage(m.to_string()));
        if text.is_empty() {
            return match goal {
                Some(goal) => fail(&format!("{}\n{USAGE}", describe(&goal))),
                None => fail(USAGE),
            };
        }
        match control.as_str() {
            "pause" => {
                let Some(goal) = goal.filter(|g| g.status == Status::Active) else {
                    return fail("No active goal to pause.");
                };
                // The user's command wins over the loop: no guard.
                g.write(
                    id,
                    Some(&Goal {
                        status: Status::Paused,
                        reason: Some("Paused by the user".into()),
                        updated_at: now().max(goal.updated_at + 1),
                        ..goal
                    }),
                )?;
                Ok(None)
            }
            "resume" => {
                let Some(goal) = goal.filter(|g| g.status == Status::Paused) else {
                    return fail("No paused goal to resume.");
                };
                let resumed = Goal {
                    id: goal.id,
                    objective: goal.objective,
                    status: Status::Active,
                    // The resume continuation is the first round of the new budget.
                    rounds: 1,
                    max_rounds: goal.max_rounds,
                    boot: boot().to_string(),
                    updated_at: now().max(goal.updated_at + 1),
                    reason: None,
                    stalls: None,
                };
                g.write(id, Some(&resumed))?;
                Ok(Some(continuation(
                    &resumed,
                    &[Some("The user resumed the goal.".into())],
                )))
            }
            "clear" => {
                if goal.is_none() {
                    return fail("No goal is set.");
                }
                g.write(id, None)?;
                Ok(None)
            }
            _ => {
                let len = text.chars().count();
                if len > MAX_OBJECTIVE {
                    return fail(&format!(
                        "A goal objective is limited to {MAX_OBJECTIVE} characters (got {len})."
                    ));
                }
                let created = Goal {
                    id: format!("goal_{}", &crate::session::new_id()[4..]),
                    objective: text.to_string(),
                    status: Status::Active,
                    rounds: 0,
                    max_rounds: MAX_ROUNDS,
                    boot: boot().to_string(),
                    updated_at: now(),
                    reason: None,
                    stalls: None,
                };
                g.write(id, Some(&created))?;
                // Titles come from the first prompt, and the kickoff is
                // not the user's words: name a stored, untitled session here.
                let h = host()?;
                if let Ok(info) = h.get(id) {
                    if info.title.is_none() {
                        h.set_title(id, &truncate(text, 60))?;
                    }
                }
                Ok(Some(render(KICKOFF, &[("objective", &escape(text))])))
            }
        }
    }
}

fn continuation(goal: &Goal, feedback: &[Option<String>]) -> String {
    let feedback: Vec<&str> = feedback
        .iter()
        .flatten()
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .collect();
    render(
        CONTINUATION,
        &[
            ("objective", &escape(&goal.objective)),
            ("feedback", &feedback.join("\n")),
        ],
    )
}

/// Every goal prompt wraps the objective the same way, so this marks the
/// objective as visible to the model.
fn objective_block(objective: &str) -> String {
    format!(
        "<untrusted_objective>\n{}\n</untrusted_objective>",
        escape(objective)
    )
}

fn text_of(m: &Message) -> String {
    m.parts
        .iter()
        .filter_map(|p| match p {
            Part::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether the model called a tool since the goal's last kickoff or
/// continuation; without either in view (after a compaction), the round
/// counts as progress.
fn used_tools(messages: &[Message], objective: &str) -> bool {
    let block = objective_block(objective);
    let Some(start) = messages
        .iter()
        .rposition(|m| m.role == Role::User && text_of(m).contains(&block))
    else {
        return true;
    };
    messages[start + 1..].iter().any(|m| {
        m.role == Role::Assistant && m.parts.iter().any(|p| matches!(p, Part::ToolCall { .. }))
    })
}

fn same_reason(previous: Option<&str>, next: &str) -> bool {
    let normalize = |t: &str| {
        t.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    };
    previous.is_some_and(|p| normalize(p) == normalize(next))
}

fn describe(goal: &Goal) -> String {
    let status = match goal.status {
        Status::Active => "Goal active",
        Status::Paused => "Goal paused",
        Status::Complete => "Goal achieved",
    };
    let mut out = format!("{status}: {}", goal.objective);
    if let Some(r) = goal.reason.as_deref().filter(|r| !r.is_empty()) {
        out.push('\n');
        out.push_str(r);
    }
    out
}

/// Reviewers sometimes wrap the JSON in a code fence or add a sentence.
fn parse_verdict(text: &str) -> Option<Verdict> {
    let (start, end) = (text.find('{')?, text.rfind('}')?);
    if end < start {
        return None;
    }
    let v: Verdict = serde_json::from_str(&text[start..=end]).ok()?;
    matches!(
        v.verdict.as_str(),
        "done" | "continue" | "blocked" | "paused" | "wait"
    )
    .then_some(v)
}

/// `/goal "Ship the release"` and `/goal Ship the release` set the same objective.
fn unquote(text: &str) -> &str {
    let mut chars = text.chars();
    let (Some(first), Some(last)) = (chars.next(), text.chars().last()) else {
        return text;
    };
    let close = match first {
        '"' => '"',
        '\'' => '\'',
        '“' => '”',
        '‘' => '’',
        _ => return text,
    };
    if text.chars().count() < 2 || last != close {
        return text;
    }
    text[first.len_utf8()..text.len() - last.len_utf8()].trim()
}

/// One pass, so an objective that contains a placeholder is not expanded again.
fn render(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        match after.find('}') {
            Some(j) => match values.iter().find(|(k, _)| *k == &after[..j]) {
                Some((_, v)) => {
                    out.push_str(v);
                    rest = &after[j + 1..];
                }
                None => {
                    out.push_str("${");
                    rest = after;
                }
            },
            None => {
                out.push_str(&rest[i..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

fn truncate(text: &str, length: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > length {
        let mut s: String = line.chars().take(length - 1).collect();
        s.push('…');
        s
    } else {
        line
    }
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fenced_and_wrapped_verdicts() {
        let v =
            parse_verdict("```json\n{\"verdict\":\"done\",\"reason\":\"All pass.\"}\n```").unwrap();
        assert_eq!(
            (v.verdict.as_str(), v.reason.as_str()),
            ("done", "All pass.")
        );
        assert!(parse_verdict("Looks good to me.").is_none());
        assert!(parse_verdict("{\"verdict\":\"maybe\",\"reason\":\"x\"}").is_none());
    }

    #[test]
    fn renders_in_one_pass_and_unquotes() {
        assert_eq!(
            render("a ${x} ${y}", &[("x", "${y}"), ("y", "2")]),
            "a ${y} 2"
        );
        assert_eq!(unquote("\"Ship it\""), "Ship it");
        assert_eq!(unquote("“Ship it”"), "Ship it");
        assert_eq!(unquote("\"Ship it'"), "\"Ship it'");
        assert_eq!(escape("<a & b>"), "&lt;a &amp; b&gt;");
        assert!(same_reason(Some(" Two  tests fail. "), "two tests fail."));
    }
}
