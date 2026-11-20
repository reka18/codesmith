//! Automations, as in OpenCode: prompts or Code Mode programs that run on a
//! schedule — once (`at`), on an interval (`every`) or by `cron` — without
//! anyone at the keyboard. They live in the session database; any running
//! CodeSmith (the UI, `serve`, `acp` or `codesmith automation serve`) runs
//! them, and a lease in the database keeps two processes from firing the
//! same run. Unattended runs follow the permission rules, but anything that
//! would ask is refused.

use crate::config::Loaded;
use crate::permission::{Fixed, Reply};
use crate::session::{Options, Session, Sink};
use crate::store::Store;
use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Trigger {
    /// Once: an instant with an offset, a wall-clock time in `tz` ("2026-10-27T09:00"), or "in 10 minutes".
    At {
        at: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tz: Option<String>,
    },
    /// Repeatedly: "90 seconds", "5 minutes", "2 hours", "1 day".
    Every {
        every: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tz: Option<String>,
    },
    /// A cron expression ("0 9 * * 1-5"), in `tz` (default: local time).
    Cron {
        cron: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tz: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Target {
    /// Send the prompt to this existing session.
    Session { session: String },
    /// Start a new session with this agent on every run.
    Subagent { subagent: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    Prompt {
        prompt: String,
        to: Target,
    },
    /// A Code Mode program over the MCP tools, with `input = {state, now, runs, args}`;
    /// it may return `{state, done, send: {prompt, to: {subagent}}}`.
    Code {
        code: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args: Option<Value>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Automation {
    pub id: String,
    pub name: String,
    pub directory: String,
    pub session_id: Option<String>,
    pub trigger: Trigger,
    pub action: Action,
    /// active, paused or done.
    pub status: String,
    pub next_due: Option<i64>,
    pub runs: u64,
    pub state: Value,
    pub last_outcome: Option<String>,
    pub last_run: Option<i64>,
    pub last_error: Option<String>,
    pub created: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Run {
    pub run: u64,
    pub at: i64,
    pub outcome: String,
    pub session_id: Option<String>,
    pub detail: String,
}

// ----- time ------------------------------------------------------------------

/// Milliseconds in "90 seconds", "5 minutes", "2h", "1 day", "30s", "1 week".
pub fn duration(s: &str) -> Result<i64> {
    let s = s.trim().to_lowercase();
    let split = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .ok_or_else(|| anyhow!("{s:?} has no unit; say e.g. \"5 minutes\""))?;
    let n: f64 = s[..split]
        .trim()
        .parse()
        .map_err(|_| anyhow!("invalid duration {s:?}"))?;
    let unit = s[split..].trim();
    let ms = match unit {
        "ms" | "millisecond" | "milliseconds" => 1.0,
        "s" | "sec" | "secs" | "second" | "seconds" => 1e3,
        "m" | "min" | "mins" | "minute" | "minutes" => 60e3,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600e3,
        "d" | "day" | "days" => 86400e3,
        "w" | "week" | "weeks" => 7.0 * 86400e3,
        _ => return Err(anyhow!("unknown duration unit {unit:?}")),
    };
    let v = (n * ms) as i64;
    if v < 60_000 {
        return Err(anyhow!("intervals must be at least a minute"));
    }
    Ok(v)
}

/// A time zone: an IANA name, or local time.
#[derive(Debug, Clone, Copy)]
pub enum Zone {
    Local,
    Tz(chrono_tz::Tz),
}

pub fn zone(tz: Option<&str>) -> Result<Zone> {
    match tz
        .map(str::trim)
        .filter(|t| !t.is_empty() && !t.eq_ignore_ascii_case("local"))
    {
        None => Ok(Zone::Local),
        Some(t) => t
            .parse::<chrono_tz::Tz>()
            .map(Zone::Tz)
            .map_err(|_| anyhow!("unknown time zone {t:?}")),
    }
}

fn wall_clock(z: Zone, dt: NaiveDateTime) -> Option<i64> {
    match z {
        Zone::Local => Local
            .from_local_datetime(&dt)
            .earliest()
            .map(|t| t.timestamp_millis()),
        Zone::Tz(tz) => tz
            .from_local_datetime(&dt)
            .earliest()
            .map(|t| t.timestamp_millis()),
    }
}

/// When an `at` fires, in milliseconds.
pub fn parse_at(s: &str, z: Zone, now: i64) -> Result<i64> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("in ") {
        return Ok(now + duration(rest).or_else(|_| duration_any(rest))?);
    }
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(t.timestamp_millis());
    }
    for f in [
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s, f) {
            return wall_clock(z, dt)
                .ok_or_else(|| anyhow!("{s} does not exist in that time zone"));
        }
    }
    Err(anyhow!(
        "invalid time {s:?}; use e.g. \"2026-10-27T09:00\" or \"in 10 minutes\""
    ))
}

/// Like `duration` but allowing under a minute (for "in 30 seconds").
fn duration_any(s: &str) -> Result<i64> {
    let s = s.trim().to_lowercase();
    let split = s
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| anyhow!("invalid duration"))?;
    let n: i64 = s[..split].parse()?;
    match s[split..].trim() {
        "s" | "sec" | "secs" | "second" | "seconds" => Ok(n * 1000),
        _ => Err(anyhow!("invalid duration {s:?}")),
    }
}

fn next_cron(expr: &str, z: Zone, after: i64) -> Result<i64> {
    let c = croner::Cron::from_str(expr).map_err(|e| anyhow!("invalid cron {expr:?}: {e}"))?;
    let t = Utc
        .timestamp_millis_opt(after)
        .single()
        .ok_or_else(|| anyhow!("bad time"))?;
    let next = match z {
        Zone::Local => c
            .find_next_occurrence(&t.with_timezone(&Local), false)
            .map(|d| d.timestamp_millis()),
        Zone::Tz(tz) => c
            .find_next_occurrence(&t.with_timezone(&tz), false)
            .map(|d| d.timestamp_millis()),
    };
    next.map_err(|e| anyhow!("cron {expr:?} never fires: {e}"))
}

/// The first due time of a new automation.
pub fn first_due(t: &Trigger, now: i64) -> Result<i64> {
    match t {
        Trigger::At { at, tz } => {
            let due = parse_at(at, zone(tz.as_deref())?, now)?;
            if due <= now {
                return Err(anyhow!("{at} is in the past"));
            }
            Ok(due)
        }
        Trigger::Every { every, tz } => {
            zone(tz.as_deref())?;
            Ok(now + duration(every)?)
        }
        Trigger::Cron { cron, tz } => next_cron(cron, zone(tz.as_deref())?, now),
    }
}

/// The due time after a run (None: it is finished).
pub fn following(t: &Trigger, now: i64) -> Result<Option<i64>> {
    match t {
        Trigger::At { .. } => Ok(None),
        Trigger::Every { every, .. } => Ok(Some(now + duration(every)?)),
        Trigger::Cron { cron, tz } => next_cron(cron, zone(tz.as_deref())?, now).map(Some),
    }
}

// ----- storage ---------------------------------------------------------------

fn ensure(c: &Connection) -> Result<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS automation (
            id TEXT PRIMARY KEY, name TEXT NOT NULL, directory TEXT NOT NULL, session_id TEXT,
            trigger TEXT NOT NULL, action TEXT NOT NULL, status TEXT NOT NULL, next_due INTEGER,
            runs INTEGER NOT NULL DEFAULT 0, state TEXT NOT NULL DEFAULT 'null',
            last_outcome TEXT, last_run INTEGER, last_error TEXT, created INTEGER NOT NULL);
         CREATE INDEX IF NOT EXISTS automation_due ON automation(status, next_due);
         CREATE TABLE IF NOT EXISTS automation_run (
            automation_id TEXT NOT NULL, run INTEGER NOT NULL, at INTEGER NOT NULL, outcome TEXT NOT NULL,
            session_id TEXT, detail TEXT NOT NULL DEFAULT '', PRIMARY KEY (automation_id, run));
         CREATE TABLE IF NOT EXISTS automation_lease (name TEXT PRIMARY KEY, holder TEXT NOT NULL, expires INTEGER NOT NULL);",
    )?;
    Ok(())
}

const COLS: &str = "id, name, directory, session_id, trigger, action, status, next_due, runs, state, last_outcome, last_run, last_error, created";

fn row(r: &rusqlite::Row) -> rusqlite::Result<(Automation, String, String, String)> {
    Ok((
        Automation {
            id: r.get(0)?,
            name: r.get(1)?,
            directory: r.get(2)?,
            session_id: r.get(3)?,
            trigger: Trigger::Every {
                every: String::new(),
                tz: None,
            },
            action: Action::Prompt {
                prompt: String::new(),
                to: Target::Subagent {
                    subagent: String::new(),
                },
            },
            status: r.get(6)?,
            next_due: r.get(7)?,
            runs: r.get::<_, i64>(8)? as u64,
            state: Value::Null,
            last_outcome: r.get(10)?,
            last_run: r.get(11)?,
            last_error: r.get(12)?,
            created: r.get(13)?,
        },
        r.get(4)?,
        r.get(5)?,
        r.get(9)?,
    ))
}

fn decode(raw: (Automation, String, String, String)) -> Result<Automation> {
    let (mut a, t, act, st) = raw;
    a.trigger = serde_json::from_str(&t)?;
    a.action = serde_json::from_str(&act)?;
    a.state = serde_json::from_str(&st).unwrap_or(Value::Null);
    Ok(a)
}

fn id() -> String {
    use rand::Rng;
    let t = crate::store::now();
    format!("aut_{t:x}{:04x}", rand::thread_rng().gen::<u16>())
}

pub struct New {
    pub name: String,
    pub directory: String,
    pub session_id: Option<String>,
    pub trigger: Trigger,
    pub action: Action,
}

pub fn create(db: &Store, n: New) -> Result<Automation> {
    let now = crate::store::now();
    let due = first_due(&n.trigger, now)?;
    if let Action::Code { code, .. } = &n.action {
        if code.trim().is_empty() {
            return Err(anyhow!("the code action needs a program"));
        }
    }
    let a = Automation {
        id: id(),
        name: n.name,
        directory: n.directory,
        session_id: n.session_id,
        trigger: n.trigger,
        action: n.action,
        status: "active".into(),
        next_due: Some(due),
        runs: 0,
        state: Value::Null,
        last_outcome: None,
        last_run: None,
        last_error: None,
        created: now,
    };
    db.with_conn(|c| {
        ensure(c)?;
        c.execute(
            &format!("INSERT INTO automation ({COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, 'null', NULL, NULL, NULL, ?9)"),
            params![a.id, a.name, a.directory, a.session_id, serde_json::to_string(&a.trigger)?, serde_json::to_string(&a.action)?, a.status, a.next_due, a.created],
        )?;
        Ok(())
    })?;
    Ok(a)
}

pub fn list(db: &Store, directory: Option<&str>) -> Result<Vec<Automation>> {
    db.with_conn(|c| {
        ensure(c)?;
        let mut st = c.prepare(&format!(
            "SELECT {COLS} FROM automation WHERE (?1 IS NULL OR directory = ?1) ORDER BY created"
        ))?;
        let rows = st
            .query_map(params![directory], row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(decode).collect()
    })
}

pub fn get(db: &Store, id: &str) -> Result<Option<Automation>> {
    db.with_conn(|c| {
        ensure(c)?;
        let r = c
            .query_row(
                &format!("SELECT {COLS} FROM automation WHERE id = ?1"),
                [id],
                row,
            )
            .optional()?;
        r.map(decode).transpose()
    })
}

/// Pause, resume (recomputing the due time) or remove.
pub fn set_status(db: &Store, id: &str, status: &str) -> Result<Automation> {
    let a = get(db, id)?.ok_or_else(|| anyhow!("no automation {id}"))?;
    let next = match status {
        "active" => Some(if matches!(a.trigger, Trigger::At { .. }) {
            a.next_due.ok_or_else(|| anyhow!("{id} already ran"))?
        } else {
            first_due(&a.trigger, crate::store::now())?
        }),
        _ => a.next_due,
    };
    db.with_conn(|c| {
        Ok(c.execute(
            "UPDATE automation SET status = ?2, next_due = ?3 WHERE id = ?1",
            params![id, status, next],
        )?)
    })?;
    get(db, id)?.ok_or_else(|| anyhow!("no automation {id}"))
}

pub fn remove(db: &Store, id: &str) -> Result<bool> {
    db.with_conn(|c| {
        ensure(c)?;
        c.execute("DELETE FROM automation_run WHERE automation_id = ?1", [id])?;
        Ok(c.execute("DELETE FROM automation WHERE id = ?1", [id])? > 0)
    })
}

pub fn runs(db: &Store, id: &str) -> Result<Vec<Run>> {
    db.with_conn(|c| {
        ensure(c)?;
        let mut st = c.prepare("SELECT run, at, outcome, session_id, detail FROM automation_run WHERE automation_id = ?1 ORDER BY run")?;
        let rows = st.query_map([id], |r| Ok(Run { run: r.get::<_, i64>(0)? as u64, at: r.get(1)?, outcome: r.get(2)?, session_id: r.get(3)?, detail: r.get(4)? }))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    })
}

/// Take the scheduler lease for `ttl` ms; true if `holder` has it.
pub fn lease(db: &Store, holder: &str, ttl: i64) -> Result<bool> {
    let now = crate::store::now();
    db.with_conn(|c| {
        ensure(c)?;
        c.execute(
            "INSERT INTO automation_lease (name, holder, expires) VALUES ('scheduler', ?1, ?2)
             ON CONFLICT(name) DO UPDATE SET holder = ?1, expires = ?2 WHERE automation_lease.expires < ?3 OR automation_lease.holder = ?1",
            params![holder, now + ttl, now],
        )?;
        let h: String = c.query_row("SELECT holder FROM automation_lease WHERE name = 'scheduler'", [], |r| r.get(0))?;
        Ok(h == holder)
    })
}

/// Due automations, each claimed for one run: its run count goes up and its
/// next due time moves on (or it is marked done), all in one transaction.
pub fn claim_due(db: &Store, now: i64) -> Result<Vec<(Automation, i64)>> {
    db.with_conn(|c| {
        ensure(c)?;
        let tx = c.unchecked_transaction()?;
        let due: Vec<Automation> = {
            let mut st = tx.prepare(&format!("SELECT {COLS} FROM automation WHERE status = 'active' AND next_due IS NOT NULL AND next_due <= ?1 ORDER BY next_due"))?;
            let rows = st.query_map([now], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter().map(decode).collect::<Result<_>>()?
        };
        let mut out = Vec::new();
        for mut a in due {
            let was_due = a.next_due.unwrap_or(now);
            let next = following(&a.trigger, now).unwrap_or(None);
            let status = if next.is_none() { "done" } else { "active" };
            tx.execute("UPDATE automation SET runs = runs + 1, next_due = ?2, status = ?3, last_run = ?4 WHERE id = ?1", params![a.id, next, status, now])?;
            a.runs += 1;
            a.next_due = next;
            a.status = status.into();
            out.push((a, was_due));
        }
        tx.commit()?;
        Ok(out)
    })
}

fn finish(
    db: &Store,
    a: &Automation,
    outcome: &str,
    session: Option<&str>,
    detail: &str,
    state: Option<&Value>,
    done: bool,
) -> Result<()> {
    db.with_conn(|c| {
        c.execute(
            "INSERT OR REPLACE INTO automation_run (automation_id, run, at, outcome, session_id, detail) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![a.id, a.runs as i64, crate::store::now(), outcome, session, detail.chars().take(4000).collect::<String>()],
        )?;
        let err = (outcome == "failed").then_some(detail);
        c.execute("UPDATE automation SET last_outcome = ?2, last_error = ?3 WHERE id = ?1", params![a.id, outcome, err])?;
        if let Some(s) = state {
            c.execute("UPDATE automation SET state = ?2 WHERE id = ?1", params![a.id, s.to_string()])?;
        }
        if done {
            c.execute("UPDATE automation SET status = 'done', next_due = NULL WHERE id = ?1", [&a.id])?;
        }
        Ok(())
    })
}

// ----- running ---------------------------------------------------------------

pub type Loader = Arc<dyn Fn(&Path) -> Result<Loaded> + Send + Sync>;

struct Quiet(String);
impl Sink for Quiet {
    fn text(&mut self, d: &str) {
        self.0.push_str(d);
    }
}

const UNATTENDED: &str = "This automation runs unattended, so nothing that needs approval can run. Adjust the permission rules to allow it, or do it in an interactive session.";

pub struct Runner {
    pub db: PathBuf,
    pub load: Loader,
    pub holder: String,
}

impl Runner {
    pub fn new(db: PathBuf, load: Loader) -> Runner {
        Runner {
            db,
            load,
            holder: format!("{}-{}", std::process::id(), id()),
        }
    }

    /// Fire everything due now (if this process holds the lease). Returns how many fired.
    pub async fn tick(&self) -> Result<usize> {
        let db = Store::open(&self.db)?;
        if !lease(&db, &self.holder, 60_000)? {
            return Ok(0);
        }
        let now = crate::store::now();
        let due = claim_due(&db, now)?;
        let n = due.len();
        let jobs = due
            .into_iter()
            .map(|(a, was)| self.fire(a, now - was > 60_000));
        futures_util::future::join_all(jobs).await;
        Ok(n)
    }

    /// Tick every 15 seconds until the process ends.
    pub async fn run_forever(self) {
        loop {
            if let Err(e) = self.tick().await {
                eprintln!("automation: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(15)).await;
        }
    }

    async fn session(
        &self,
        loaded: &Loaded,
        dir: &Path,
        resume: Option<String>,
        agent: Option<String>,
        title: String,
        parent: Option<String>,
    ) -> Result<Session> {
        Session::new(
            loaded,
            dir.to_path_buf(),
            Options {
                agent,
                model: None,
                resume,
                title: Some(title),
                parent,
                questioner: None,
                mcp: None,
            },
            Arc::new(Fixed(Reply::Reject(Some(UNATTENDED.into())))),
            Some(Store::open(&self.db)?),
        )
    }

    async fn prompt(
        &self,
        a: &Automation,
        loaded: &Loaded,
        prompt: &str,
        to: &Target,
    ) -> Result<(String, String)> {
        let dir = Path::new(&a.directory);
        let mut s = match to {
            Target::Session { session } => {
                self.session(
                    loaded,
                    dir,
                    Some(session.clone()),
                    None,
                    a.name.clone(),
                    None,
                )
                .await?
            }
            Target::Subagent { subagent } => {
                self.session(
                    loaded,
                    dir,
                    None,
                    Some(subagent.clone()),
                    format!("{} #{}", a.name, a.runs),
                    a.session_id.clone(),
                )
                .await?
            }
        };
        let mut out = Quiet(String::new());
        s.prompt(prompt, &mut out).await?;
        Ok((s.id.clone(), out.0))
    }

    async fn fire(&self, a: Automation, late: bool) {
        let db = match Store::open(&self.db) {
            Ok(d) => d,
            Err(e) => return eprintln!("automation {}: {e:#}", a.id),
        };
        let ok = if late { "late" } else { "fired" };
        let r: Result<()> = async {
            let loaded = (self.load)(Path::new(&a.directory)).with_context(|| format!("loading config in {}", a.directory))?;
            match &a.action {
                Action::Prompt { prompt, to } => {
                    let (sid, answer) = self.prompt(&a, &loaded, prompt, to).await?;
                    finish(&db, &a, ok, Some(&sid), &answer, None, false)
                }
                Action::Code { code, args } => {
                    let mcp = crate::mcp::Mcp::connect(&loaded.config.mcp, &loaded.root).await;
                    let entries: Vec<crate::codemode::Entry> = mcp.tools.iter().map(|t| crate::codemode::Entry { tool: t.name.clone(), namespace: t.server.clone(), name: t.remote.clone(), description: t.description.clone(), schema: t.schema.clone() }).collect();
                    let input = json!({"state": a.state, "now": crate::store::now(), "runs": a.runs, "args": args});
                    let program = format!("const input = {input};\n{code}");
                    let gate = crate::permission::Gate::new(crate::agent::resolve(&loaded.config, "build").map(|b| b.permissions).unwrap_or_default(), Arc::new(Fixed(Reply::Reject(Some(UNATTENDED.into())))));
                    let out = crate::codemode::execute(&program, &entries, crate::codemode::MAX_TOOL_CALLS, crate::codemode::TIMEOUT, |tool, input| {
                        let mcp = mcp.clone();
                        let allowed = gate.assert(&crate::permission::Request { action: tool.clone(), resources: vec!["*".into()], save: vec![], title: format!("Run MCP tool {tool}") });
                        async move {
                            allowed.map_err(|e| e.to_string())?;
                            match mcp.call(&tool, input).await {
                                Ok((text, false)) => Ok(text),
                                Ok((text, true)) => Err(text),
                                Err(e) => Err(format!("{e:#}")),
                            }
                        }
                    })
                    .await;
                    let result = out.result.clone().map_err(|e| anyhow!("{e}"))?;
                    let state = result.get("state");
                    let done = result["done"].as_bool() == Some(true);
                    let mut detail = out.render().0;
                    let mut session = None;
                    if let Some(send) = result.get("send").filter(|s| s.is_object()) {
                        let prompt = send["prompt"].as_str().filter(|p| !p.trim().is_empty()).ok_or_else(|| anyhow!("send.prompt must be text"))?;
                        let to = match send["to"]["subagent"].as_str() {
                            Some(s) => Target::Subagent { subagent: s.into() },
                            None => match &a.session_id {
                                Some(s) => Target::Session { session: s.clone() },
                                None => Target::Subagent { subagent: "build".into() },
                            },
                        };
                        let (sid, answer) = self.prompt(&a, &loaded, prompt, &to).await?;
                        session = Some(sid);
                        detail.push_str(&format!("\n\nSent: {prompt}\n{answer}"));
                    }
                    finish(&db, &a, ok, session.as_deref(), &detail, state, done)
                }
            }
        }
        .await;
        if let Err(e) = r {
            let _ = finish(&db, &a, "failed", None, &format!("{e:#}"), None, false);
        }
    }
}

pub const TOOL: &str = "automation";

pub fn tool_spec() -> (String, Value) {
    let description = "Schedule work to happen later, without the user present: a prompt sent to this session, or to a new session with a given agent on each run, once (`at`), on an interval (`every`) or on a `cron` schedule. Also list, pause, resume or remove this project's automations. Runs are unattended: anything that would need the user's approval is refused, so schedule only work the permission rules allow. Use it when the user asks for something recurring or delayed (\"check the build every 10 minutes\", \"remind me at 5pm\").";
    let params = json!({
        "type": "object",
        "properties": {
            "action": {"type": "string", "enum": ["create", "list", "pause", "resume", "remove"]},
            "name": {"type": "string", "description": "Short name (create)"},
            "trigger": {"type": "object", "description": "When it runs (create)", "properties": {
                "type": {"type": "string", "enum": ["at", "every", "cron"]},
                "at": {"type": "string", "description": "\"in 10 minutes\", \"2026-10-27T09:00\" or an RFC 3339 time"},
                "every": {"type": "string", "description": "\"10 minutes\", \"2 hours\", \"1 day\""},
                "cron": {"type": "string", "description": "Five-field cron, e.g. \"0 9 * * 1-5\""},
                "tz": {"type": "string", "description": "IANA time zone (default: local)"}
            }, "required": ["type"]},
            "prompt": {"type": "string", "description": "What to do on each run (create)"},
            "agent": {"type": "string", "description": "Run each time in a new session with this agent instead of this session (create)"},
            "id": {"type": "string", "description": "The automation (pause, resume, remove)"}
        },
        "required": ["action"]
    });
    (description.into(), params)
}

/// Render automations for the model or the terminal.
pub fn table(list: &[Automation]) -> String {
    if list.is_empty() {
        return "No automations.".into();
    }
    list.iter()
        .map(|a| {
            let what = match &a.action {
                Action::Prompt { prompt, to } => format!(
                    "{} → {}",
                    prompt.lines().next().unwrap_or(""),
                    match to {
                        Target::Session { session } => session.clone(),
                        Target::Subagent { subagent } => format!("new {subagent} session"),
                    }
                ),
                Action::Code { .. } => "Code Mode program".into(),
            };
            format!(
                "{}  {}  [{}]  {}  next {}  runs {}{}  — {what}",
                a.id,
                a.name,
                a.status,
                describe(&a.trigger),
                when(a.next_due),
                a.runs,
                a.last_outcome
                    .as_deref()
                    .map(|o| format!(" (last {o})"))
                    .unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Start the scheduler in the background unless config turns it off.
pub fn spawn_scheduler(loaded: &Loaded) {
    if loaded.config.automations == Some(false) {
        return;
    }
    let load: Loader = Arc::new(|d: &Path| crate::config::load(d));
    tokio::spawn(Runner::new(crate::store::default_path(), load).run_forever());
}

/// A one-line description of when an automation runs.
pub fn describe(t: &Trigger) -> String {
    let tz = |z: &Option<String>| z.as_deref().map(|z| format!(" ({z})")).unwrap_or_default();
    match t {
        Trigger::At { at, tz: z } => format!("at {at}{}", tz(z)),
        Trigger::Every { every, tz: z } => format!("every {every}{}", tz(z)),
        Trigger::Cron { cron, tz: z } => format!("cron \"{cron}\"{}", tz(z)),
    }
}

pub fn when(ms: Option<i64>) -> String {
    ms.and_then(|m| Local.timestamp_millis_opt(m).single())
        .map(|t| t.format("%Y-%m-%d %H:%M %:z").to_string())
        .unwrap_or_else(|| "—".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_and_times() {
        assert_eq!(duration("5 minutes").unwrap(), 300_000);
        assert_eq!(duration("2h").unwrap(), 7_200_000);
        assert_eq!(duration("1 day").unwrap(), 86_400_000);
        assert_eq!(duration("1.5 hours").unwrap(), 5_400_000);
        assert!(
            duration("30 seconds").is_err(),
            "intervals of at least a minute"
        );
        assert!(duration("5 fortnights").is_err());
        let now = 1_000_000;
        assert_eq!(
            parse_at("in 10 minutes", Zone::Local, now).unwrap(),
            now + 600_000
        );
        assert_eq!(
            parse_at("in 30 seconds", Zone::Local, now).unwrap(),
            now + 30_000
        );
        assert_eq!(
            parse_at("2026-01-01T00:00:00Z", Zone::Local, now).unwrap(),
            1_767_225_600_000
        );
        let paris = zone(Some("Europe/Paris")).unwrap();
        assert_eq!(
            parse_at("2026-01-01T09:00", paris, now).unwrap(),
            1_767_254_400_000,
            "09:00 in Paris is 08:00 UTC in winter"
        );
        assert!(zone(Some("Mars/Olympus")).is_err());
        assert!(parse_at("tomorrowish", Zone::Local, now).is_err());
    }

    #[test]
    fn schedules() {
        let jan1 = 1_767_225_600_000; // 2026-01-01T00:00Z, a Thursday
        let c = Trigger::Cron {
            cron: "0 9 * * 1-5".into(),
            tz: Some("UTC".into()),
        };
        assert_eq!(first_due(&c, jan1).unwrap(), jan1 + 9 * 3_600_000);
        assert_eq!(
            following(&c, jan1 + 9 * 3_600_000).unwrap(),
            Some(jan1 + 33 * 3_600_000),
            "next weekday 09:00"
        );
        assert!(first_due(
            &Trigger::Cron {
                cron: "61 * * * *".into(),
                tz: None
            },
            jan1
        )
        .is_err());
        let e = Trigger::Every {
            every: "10 minutes".into(),
            tz: None,
        };
        assert_eq!(following(&e, jan1).unwrap(), Some(jan1 + 600_000));
        assert_eq!(
            following(
                &Trigger::At {
                    at: "x".into(),
                    tz: None
                },
                jan1
            )
            .unwrap(),
            None
        );
        assert!(first_due(
            &Trigger::At {
                at: "2000-01-01T00:00:00Z".into(),
                tz: None
            },
            jan1
        )
        .unwrap_err()
        .to_string()
        .contains("past"));
    }

    fn new(trigger: Trigger, action: Action) -> New {
        New {
            name: "n".into(),
            directory: "/p".into(),
            session_id: None,
            trigger,
            action,
        }
    }

    #[test]
    fn storage_claims_and_leases() {
        let db = Store::memory().unwrap();
        let prompt = Action::Prompt {
            prompt: "check".into(),
            to: Target::Subagent {
                subagent: "build".into(),
            },
        };
        let a = create(
            &db,
            new(
                Trigger::Every {
                    every: "5 minutes".into(),
                    tz: None,
                },
                prompt.clone(),
            ),
        )
        .unwrap();
        let once = create(
            &db,
            new(
                Trigger::At {
                    at: "in 1 minutes".into(),
                    tz: None,
                },
                prompt.clone(),
            ),
        )
        .unwrap();
        assert!(create(
            &db,
            new(
                Trigger::Every {
                    every: "nope".into(),
                    tz: None
                },
                prompt.clone()
            )
        )
        .is_err());
        assert_eq!(list(&db, Some("/p")).unwrap().len(), 2);
        assert_eq!(list(&db, Some("/q")).unwrap().len(), 0);

        let later = crate::store::now() + 10 * 60_000;
        let due = claim_due(&db, later).unwrap();
        assert_eq!(due.len(), 2);
        assert!(
            claim_due(&db, later).unwrap().is_empty(),
            "a claimed run is not claimed twice"
        );
        let a2 = get(&db, &a.id).unwrap().unwrap();
        assert_eq!(
            (a2.runs, a2.status.as_str(), a2.next_due),
            (1, "active", Some(later + 300_000))
        );
        assert_eq!(
            get(&db, &once.id).unwrap().unwrap().status,
            "done",
            "an `at` runs once"
        );

        finish(
            &db,
            &a2,
            "fired",
            Some("ses_x"),
            "ok",
            Some(&json!({"n": 1})),
            false,
        )
        .unwrap();
        let a3 = get(&db, &a.id).unwrap().unwrap();
        assert_eq!(
            (a3.last_outcome.as_deref(), a3.state.clone()),
            (Some("fired"), json!({"n": 1}))
        );
        assert_eq!(
            runs(&db, &a.id).unwrap()[0].session_id.as_deref(),
            Some("ses_x")
        );

        assert_eq!(set_status(&db, &a.id, "paused").unwrap().status, "paused");
        assert!(
            claim_due(&db, later + 3_600_000).unwrap().is_empty(),
            "paused automations do not run"
        );
        assert_eq!(set_status(&db, &a.id, "active").unwrap().status, "active");
        assert!(remove(&db, &a.id).unwrap());
        assert!(get(&db, &a.id).unwrap().is_none());

        assert!(lease(&db, "one", 60_000).unwrap());
        assert!(
            !lease(&db, "two", 60_000).unwrap(),
            "only one scheduler at a time"
        );
        assert!(lease(&db, "one", 60_000).unwrap(), "the holder renews");
        assert!(
            lease(&db, "one", -1).unwrap(),
            "the holder can let it lapse"
        );
        assert!(
            lease(&db, "two", 60_000).unwrap(),
            "an expired lease is taken over"
        );
    }

    #[test]
    fn config_shapes() {
        let a: Action = serde_json::from_value(
            json!({"type": "prompt", "prompt": "hi", "to": {"session": "ses_1"}}),
        )
        .unwrap();
        assert_eq!(
            a,
            Action::Prompt {
                prompt: "hi".into(),
                to: Target::Session {
                    session: "ses_1".into()
                }
            }
        );
        let t: Trigger =
            serde_json::from_value(json!({"type": "cron", "cron": "*/5 * * * *", "tz": "UTC"}))
                .unwrap();
        assert_eq!(describe(&t), "cron \"*/5 * * * *\" (UTC)");
    }
}
