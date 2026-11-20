//! `codesmith stats`: usage across sessions — activity, prompts, steps,
//! tokens, cost, models and tool reliability — in OpenCode's layout.
//! Computed from the session store; nothing leaves the machine.

use crate::config::Cost;
use crate::store::{Info, Store};
use anyhow::Context;
use anyhow::Result;
use chrono::{Datelike, Duration, Local, NaiveDate, TimeZone};
use codesmith_ai::{Role, Usage};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Debug, Clone, Default)]
pub struct Query {
    /// Milliseconds since the epoch; `None` for all time.
    pub from: Option<i64>,
    pub to: i64,
    /// Only sessions in this directory or below it.
    pub directory: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct ModelStat {
    pub model: String,
    pub steps: u64,
    pub tokens: Usage,
    pub cost: f64,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct ToolStat {
    pub name: String,
    pub calls: u64,
    pub succeeded: u64,
    pub failed: u64,
    /// Median duration in ms (only for calls recorded with timings).
    pub p50_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Stats {
    pub from: i64,
    pub to: i64,
    pub sessions: u64,
    pub subagents: u64,
    pub prompts: u64,
    pub steps: u64,
    pub tokens: Usage,
    pub cost: f64,
    pub models: Vec<ModelStat>,
    pub tools: Vec<ToolStat>,
    /// Prompts plus steps per local date (YYYY-MM-DD).
    pub activity: BTreeMap<String, u64>,
    pub active_days: u64,
    pub streak: u64,
}

fn total(u: &Usage) -> u64 {
    u.input + u.output + u.cache_read + u.cache_write
}

fn add(a: &mut Usage, b: &Usage) {
    a.input += b.input;
    a.output += b.output;
    a.cache_read += b.cache_read;
    a.cache_write += b.cache_write;
}

pub fn price(u: &Usage, c: &Cost) -> f64 {
    (u.input as f64 * c.input
        + u.output as f64 * c.output
        + u.cache_read as f64 * c.cache_read
        + u.cache_write as f64 * c.cache_write)
        / 1_000_000.0
}

/// Messages the user typed (not tool results or CodeSmith's own notes).
fn is_prompt(text: &str) -> bool {
    let t = text.trim_start();
    ![
        "<shell ",
        "<system-reminder>",
        "<conversation-checkpoint>",
        "<subagent",
    ]
    .iter()
    .any(|p| t.starts_with(p))
        && !crate::runner::is_engine_message(text)
}

fn date(ms: i64) -> String {
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

/// Longest run of consecutive dates.
fn streak(days: &BTreeSet<NaiveDate>) -> u64 {
    let (mut best, mut run, mut prev): (u64, u64, Option<NaiveDate>) = (0, 0, None);
    for d in days {
        run = if prev.is_some_and(|p| p + Duration::days(1) == *d) {
            run + 1
        } else {
            1
        };
        best = best.max(run);
        prev = Some(*d);
    }
    best
}

/// What `compute` needs of a log record. Messages are read only for their
/// role, typed text and tool results, so stored tool output, file contents
/// and media are skipped rather than copied.
enum Lite {
    Message { role: Role, parts: Vec<LitePart> },
    Step { model: String, usage: Usage },
    Tool { name: String, ms: u64, error: bool },
    Other,
}

enum LitePart {
    Text(String),
    ToolResult { name: String, is_error: bool },
    Other,
}

/// A record body, as `store::Record` writes it, with only the fields used here.
#[derive(serde::Deserialize)]
struct Body<'a> {
    #[serde(borrow)]
    kind: std::borrow::Cow<'a, str>,
    #[serde(borrow)]
    message: Option<LiteMessage<'a>>,
    model: Option<String>,
    usage: Option<Usage>,
    name: Option<String>,
    ms: Option<u64>,
    error: Option<bool>,
}

#[derive(serde::Deserialize)]
struct LiteMessage<'a> {
    role: Role,
    #[serde(borrow)]
    parts: Vec<RawPart<'a>>,
}

#[derive(serde::Deserialize)]
struct RawPart<'a> {
    #[serde(rename = "type", borrow)]
    kind: std::borrow::Cow<'a, str>,
    #[serde(borrow)]
    data: Option<&'a serde_json::value::RawValue>,
}

#[derive(serde::Deserialize)]
struct ToolResultData {
    name: String,
    is_error: bool,
}

fn lite(body: &str) -> Result<Lite> {
    let b: Body = serde_json::from_str(body)?;
    let missing = |f: &str| anyhow::anyhow!("missing field `{f}`");
    Ok(match b.kind.as_ref() {
        "message" => {
            let m = b.message.ok_or_else(|| missing("message"))?;
            let mut parts = Vec::with_capacity(m.parts.len());
            for p in m.parts {
                let data = p.data.map(|d| d.get()).unwrap_or("null");
                parts.push(match p.kind.as_ref() {
                    "text" => LitePart::Text(serde_json::from_str(data)?),
                    "tool_result" => {
                        let r: ToolResultData = serde_json::from_str(data)?;
                        LitePart::ToolResult {
                            name: r.name,
                            is_error: r.is_error,
                        }
                    }
                    _ => LitePart::Other,
                });
            }
            Lite::Message {
                role: m.role,
                parts,
            }
        }
        "step" => Lite::Step {
            model: b.model.ok_or_else(|| missing("model"))?,
            usage: b.usage.ok_or_else(|| missing("usage"))?,
        },
        "tool" => Lite::Tool {
            name: b.name.ok_or_else(|| missing("name"))?,
            ms: b.ms.ok_or_else(|| missing("ms"))?,
            error: b.error.ok_or_else(|| missing("error"))?,
        },
        _ => Lite::Other,
    })
}

/// A session's records as (created, record), in order.
fn lite_entries(db: &Store, id: &str) -> Result<Vec<(i64, Lite)>> {
    db.with_conn(|conn| {
        let mut stmt = conn.prepare_cached(
            "SELECT seq, created, body FROM record WHERE session_id = ?1 ORDER BY seq",
        )?;
        let mut rows = stmt.query([id])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            let seq: i64 = r.get(0)?;
            let created: i64 = r.get(1)?;
            let body = r.get_ref(2)?.as_str()?;
            out.push((
                created,
                lite(body).with_context(|| format!("record {seq} of {id}"))?,
            ));
        }
        Ok(out)
    })
}

pub fn compute(db: &Store, q: &Query, cost: &dyn Fn(&str) -> Option<Cost>) -> Result<Stats> {
    let mut s = Stats {
        to: q.to,
        ..Default::default()
    };
    let mut earliest = i64::MAX;
    let mut models: HashMap<String, ModelStat> = HashMap::new();
    let mut tools: HashMap<String, (ToolStat, Vec<u64>)> = HashMap::new();
    let in_range = |t: i64| q.from.is_none_or(|f| t >= f) && t < q.to;
    let sessions: Vec<Info> = db.list(None, true)?;
    for info in sessions.iter().filter(|i| {
        q.directory.as_deref().is_none_or(|d| {
            i.directory == d
                || i.directory
                    .starts_with(&format!("{}/", d.trim_end_matches('/')))
        })
    }) {
        let entries = lite_entries(db, &info.id)?;
        let has_steps = entries.iter().any(|e| matches!(e.1, Lite::Step { .. }));
        let has_tools = entries.iter().any(|e| matches!(e.1, Lite::Tool { .. }));
        let mut active = false;
        for (created, record) in entries.iter().filter(|e| in_range(e.0)) {
            let mut tick = 0;
            match record {
                Lite::Message { role, parts } => {
                    for p in parts {
                        match (role, p) {
                            (Role::User, LitePart::Text(t))
                                if is_prompt(t) && info.parent_id.is_none() =>
                            {
                                s.prompts += 1;
                                tick += 1;
                            }
                            (Role::User, LitePart::ToolResult { name, is_error }) if !has_tools => {
                                let t = &mut tools
                                    .entry(name.clone())
                                    .or_insert_with(|| {
                                        (
                                            ToolStat {
                                                name: name.clone(),
                                                ..Default::default()
                                            },
                                            vec![],
                                        )
                                    })
                                    .0;
                                t.calls += 1;
                                if *is_error {
                                    t.failed += 1
                                } else {
                                    t.succeeded += 1
                                }
                            }
                            _ => {}
                        }
                    }
                    if *role == Role::Assistant && !has_steps {
                        s.steps += 1;
                        tick += 1;
                        models
                            .entry(info.model.clone())
                            .or_insert_with(|| ModelStat {
                                model: info.model.clone(),
                                ..Default::default()
                            })
                            .steps += 1;
                    }
                }
                Lite::Step { model, usage } => {
                    s.steps += 1;
                    tick += 1;
                    add(&mut s.tokens, usage);
                    let m = models.entry(model.clone()).or_insert_with(|| ModelStat {
                        model: model.clone(),
                        ..Default::default()
                    });
                    m.steps += 1;
                    add(&mut m.tokens, usage);
                }
                Lite::Tool { name, ms, error } => {
                    let (t, d) = tools.entry(name.clone()).or_insert_with(|| {
                        (
                            ToolStat {
                                name: name.clone(),
                                ..Default::default()
                            },
                            vec![],
                        )
                    });
                    t.calls += 1;
                    if *error {
                        t.failed += 1
                    } else {
                        t.succeeded += 1
                    }
                    d.push(*ms);
                }
                Lite::Other => {}
            }
            active = true;
            earliest = earliest.min(*created);
            if tick > 0 {
                *s.activity.entry(date(*created)).or_default() += tick;
            }
        }
        // Sessions from before per-step records: use the session's totals.
        if !has_steps && in_range(info.created) {
            add(&mut s.tokens, &info.usage);
            add(
                &mut models
                    .entry(info.model.clone())
                    .or_insert_with(|| ModelStat {
                        model: info.model.clone(),
                        ..Default::default()
                    })
                    .tokens,
                &info.usage,
            );
        }
        if active {
            if info.parent_id.is_some() {
                s.subagents += 1
            } else {
                s.sessions += 1
            }
        }
    }
    s.from = q
        .from
        .unwrap_or(if earliest == i64::MAX { q.to } else { earliest });
    let mut models: Vec<ModelStat> = models
        .into_values()
        .filter(|m| m.steps > 0 || total(&m.tokens) > 0)
        .collect();
    for m in &mut models {
        m.cost = cost(&m.model).map(|c| price(&m.tokens, &c)).unwrap_or(0.0);
        s.cost += m.cost;
    }
    models.sort_by(|a, b| {
        total(&b.tokens)
            .cmp(&total(&a.tokens))
            .then(b.steps.cmp(&a.steps))
            .then(a.model.cmp(&b.model))
    });
    s.models = models;
    let mut tools: Vec<ToolStat> = tools
        .into_values()
        .map(|(mut t, mut d)| {
            d.sort_unstable();
            t.p50_ms = (!d.is_empty()).then(|| d[d.len() / 2]);
            t
        })
        .collect();
    tools.sort_by(|a, b| b.calls.cmp(&a.calls).then(a.name.cmp(&b.name)));
    s.tools = tools;
    let days: BTreeSet<NaiveDate> = s
        .activity
        .keys()
        .filter_map(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
        .collect();
    s.active_days = days.len() as u64;
    s.streak = streak(&days);
    Ok(s)
}

// ----- rendering -------------------------------------------------------------

pub struct Render {
    pub label: String,
    pub scope: String,
    pub models: bool,
    pub tools: bool,
    pub cost: bool,
    pub limit: usize,
    pub color: bool,
    pub width: usize,
}

fn style(s: &str, code: &str, on: bool) -> String {
    if on {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

pub fn number(v: u64) -> String {
    let trim = |x: f64| format!("{x:.1}").trim_end_matches(".0").to_string();
    match v {
        v if v >= 1_000_000_000 => format!("{}b", trim(v as f64 / 1e9)),
        v if v >= 1_000_000 => format!("{}m", trim(v as f64 / 1e6)),
        v if v >= 1_000 => format!("{}k", trim(v as f64 / 1e3)),
        v => v.to_string(),
    }
}

fn percent(v: f64) -> String {
    if v >= 10.0 {
        format!("{v:.1}%")
    } else {
        format!("{v:.2}%")
    }
}

fn duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!(
            "{}s",
            format!("{:.1}", ms as f64 / 1000.0).trim_end_matches(".0")
        )
    }
}

fn count(v: u64, noun: &str, color: bool) -> String {
    format!(
        "{} {noun}{}",
        style(&number(v), "1", color),
        if v == 1 { "" } else { "s" }
    )
}

fn level(v: u64, max: u64) -> i32 {
    if v == 0 || max == 0 {
        0
    } else {
        ((v * 4).div_ceil(max)).clamp(1, 4) as i32
    }
}

fn paint(level: i32, color: bool) -> String {
    if color {
        let c = ["238", "22", "28", "34", "40"][level as usize];
        format!("\x1b[38;5;{c}m■\x1b[0m")
    } else {
        ["·", "░", "▒", "▓", "█"][level as usize].to_string()
    }
}

/// A GitHub-style calendar: one column per week (Monday first), clipped to the latest weeks that fit.
fn calendar(s: &Stats, color: bool, width: usize) -> Vec<String> {
    let to_ms = s.to.saturating_sub(1);
    let (Some(from), Some(to)) = (
        Local.timestamp_millis_opt(s.from).single(),
        Local.timestamp_millis_opt(to_ms).single(),
    ) else {
        return vec![];
    };
    let (first, last) = (from.date_naive(), to.date_naive());
    let start = first - Duration::days(first.weekday().num_days_from_monday() as i64);
    let mut weeks = ((last - start).num_days() / 7 + 1) as usize;
    let max_weeks = width.saturating_sub(4).max(4);
    let mut origin = start;
    let clipped = weeks > max_weeks;
    if clipped {
        origin = start + Duration::weeks((weeks - max_weeks) as i64);
        weeks = max_weeks;
    }
    let max = s.activity.values().copied().max().unwrap_or(0);
    let mut months = String::new();
    let mut last_month = 0;
    for w in 0..weeks {
        let d = origin + Duration::weeks(w as i64);
        if d.month() != last_month && months.chars().count() <= w {
            let label = d.format("%b").to_string();
            while months.chars().count() < w {
                months.push(' ');
            }
            if w + 3 <= weeks || months.trim().is_empty() {
                months.push_str(&label);
            }
            last_month = d.month();
        }
    }
    let mut out = vec![
        style(
            &if clipped {
                format!("activity · last {weeks} weeks")
            } else {
                "activity".into()
            },
            "1;36",
            color,
        ),
        format!("   {}", style(months.trim_end(), "2", color)),
    ];
    for (i, name) in ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"]
        .iter()
        .enumerate()
    {
        let mut row = format!("{} ", style(name, "2", color));
        for w in 0..weeks {
            let d = origin + Duration::days((w * 7 + i) as i64);
            if d < first || d > last {
                row.push(' ');
            } else {
                row.push_str(&paint(
                    level(
                        *s.activity
                            .get(&d.format("%Y-%m-%d").to_string())
                            .unwrap_or(&0),
                        max,
                    ),
                    color,
                ));
            }
        }
        out.push(row.trim_end().to_string());
    }
    out.push(String::new());
    out.push(format!(
        "   {} {} {}",
        style("less", "2", color),
        (0..5).map(|l| paint(l, color)).collect::<String>(),
        style("more", "2", color)
    ));
    out
}

fn row4(a: &str, b: &str, c: &str, d: &str) -> String {
    let a: String = if a.chars().count() > 34 {
        a.chars().take(33).chain(['…']).collect()
    } else {
        a.to_string()
    };
    format!("{a:<34}{b:>10}{c:>12}{d:>12}")
}

pub fn render(s: &Stats, o: &Render) -> String {
    let c = o.color;
    let details = o.models || o.tools || o.cost;
    let mut lines: Vec<String> = Vec::new();
    let heading = format!(
        "{} {}",
        style("codesmith stats", "1;36", c),
        style(&format!("· {} · {}", o.label, o.scope), "2", c)
    );
    let empty = s.sessions == 0 && s.prompts == 0 && s.steps == 0;
    if details {
        lines.push(style(&format!("{} · {}", o.label, o.scope), "2", c));
    } else if empty {
        lines.extend([
            heading,
            String::new(),
            style("no activity in this range", "2", c),
        ]);
    } else {
        let (ok, bad): (u64, u64) = s
            .tools
            .iter()
            .fold((0, 0), |(a, b), t| (a + t.succeeded, b + t.failed));
        let tool_line = if ok + bad == 0 {
            "no tool calls".to_string()
        } else {
            format!(
                "{} tool success",
                style(&percent(ok as f64 * 100.0 / (ok + bad) as f64), "1;36", c)
            )
        };
        lines.push(heading);
        lines.push(String::new());
        lines.extend(calendar(s, c, o.width));
        lines.push(String::new());
        let mut sl = count(s.sessions, "session", c);
        if s.subagents > 0 {
            sl.push_str(&format!(" · {}", count(s.subagents, "subagent", c)));
        }
        lines.push(sl);
        lines.push(format!(
            "{} · {} · {}",
            count(s.prompts, "prompt", c),
            count(s.steps, "step", c),
            count(total(&s.tokens), "token", c)
        ));
        lines.push(format!(
            "{tool_line} · {} · best streak {} day{}",
            count(s.active_days, "active day", c),
            style(&s.streak.to_string(), "1;36", c),
            if s.streak == 1 { "" } else { "s" }
        ));
    }
    let gap = |lines: &mut Vec<String>| {
        if !lines.is_empty() {
            lines.push(String::new());
        }
    };
    if o.cost {
        gap(&mut lines);
        let input = s.tokens.input + s.tokens.cache_read + s.tokens.cache_write;
        let cached = if input == 0 {
            0.0
        } else {
            s.tokens.cache_read as f64 * 100.0 / input as f64
        };
        let r = |l: &str, v: String| format!("  {l:<20}{v}");
        lines.extend([
            "COST & TOKENS".to_string(),
            r("cost", format!("${:.2}", s.cost)),
            r("input", number(s.tokens.input)),
            r("output", number(s.tokens.output)),
            r("cache read", number(s.tokens.cache_read)),
            r("cache write", number(s.tokens.cache_write)),
            r("cached input", percent(cached)),
        ]);
        if s.cost == 0.0 && total(&s.tokens) > 0 {
            lines.push(style(
                "  (set \"cost\" on a model in codesmith.json to price its tokens)",
                "2",
                c,
            ));
        }
    }
    if o.models {
        gap(&mut lines);
        lines.push("MODELS".into());
        if s.models.is_empty() {
            lines.push("  no model usage".into());
        } else {
            lines.push(row4("model", "tokens", "steps", "cost"));
            for m in s.models.iter().take(o.limit) {
                lines.push(row4(
                    &m.model,
                    &number(total(&m.tokens)),
                    &number(m.steps),
                    &format!("${:.2}", m.cost),
                ));
            }
            let more = s.models.len().saturating_sub(o.limit);
            if more > 0 {
                lines.extend([
                    String::new(),
                    format!("+{more} more model{}", if more == 1 { "" } else { "s" }),
                ]);
            }
        }
    }
    if o.tools {
        gap(&mut lines);
        lines.push("TOOL RELIABILITY".into());
        if s.tools.is_empty() {
            lines.push("  no tool calls".into());
        } else {
            lines.push(row4("tool", "calls", "error", "p50"));
            for t in s.tools.iter().take(o.limit) {
                let fin = t.succeeded + t.failed;
                lines.push(row4(
                    &t.name,
                    &number(t.calls),
                    &if fin == 0 {
                        "-".into()
                    } else {
                        percent(t.failed as f64 * 100.0 / fin as f64)
                    },
                    &t.p50_ms.map(duration).unwrap_or_else(|| "-".into()),
                ));
            }
            let fin: u64 = s.tools.iter().map(|t| t.succeeded + t.failed).sum();
            lines.push(String::new());
            lines.push(format!("{} finished calls", number(fin)));
            let more = s.tools.len().saturating_sub(o.limit);
            if more > 0 {
                lines.push(format!(
                    "+{more} more tool{}",
                    if more == 1 { "" } else { "s" }
                ));
            }
        }
    }
    lines.join("\n")
}

/// `--days`, `--year` or `--all` as (from, to, label), in local time.
pub fn range(days: Option<u32>, year: Option<i32>, all: bool) -> (Option<i64>, i64, String) {
    let now = Local::now();
    let to = now.timestamp_millis() + 1;
    let midnight = |d: NaiveDate| {
        Local
            .from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap())
            .earliest()
            .map(|t| t.timestamp_millis())
            .unwrap_or(0)
    };
    if all {
        return (None, to, "all time".into());
    }
    if let Some(n) = days {
        let from = now.date_naive() - Duration::days(n.saturating_sub(1) as i64);
        let label = if n <= 1 {
            "today".to_string()
        } else {
            format!("last {n} days")
        };
        return (Some(midnight(from)), to, label);
    }
    let y = year.unwrap_or(now.year());
    let start = midnight(NaiveDate::from_ymd_opt(y, 1, 1).unwrap_or(now.date_naive()));
    if y == now.year() {
        (Some(start), to, format!("{y} so far"))
    } else {
        (
            Some(start),
            midnight(NaiveDate::from_ymd_opt(y + 1, 1, 1).unwrap_or(now.date_naive())),
            y.to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Record;
    use codesmith_ai::{Message, Part};
    use serde_json::json;

    fn info(id: &str, dir: &str, parent: Option<&str>, created: i64) -> Info {
        Info {
            id: id.into(),
            directory: dir.into(),
            title: None,
            agent: "build".into(),
            model: "p/m".into(),
            parent_id: parent.map(str::to_string),
            created,
            updated: created,
            usage: Usage::default(),
        }
    }

    fn day(d: u32, h: u32) -> i64 {
        Local
            .with_ymd_and_hms(2026, 11, d, h, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis()
    }

    fn store() -> Store {
        let db = Store::memory().unwrap();
        // A session with per-step records over two days.
        db.create(&info("a", "/p", None, day(10, 9))).unwrap();
        db.append_at(
            "a",
            &Record::Message {
                message: Message::user("fix it"),
            },
            day(10, 9),
        )
        .unwrap();
        db.append_at(
            "a",
            &Record::Step {
                model: "p/m".into(),
                usage: Usage {
                    input: 1000,
                    output: 200,
                    cache_read: 800,
                    cache_write: 0,
                },
            },
            day(10, 9),
        )
        .unwrap();
        db.append_at(
            "a",
            &Record::Tool {
                name: "read".into(),
                ms: 40,
                error: false,
            },
            day(10, 9),
        )
        .unwrap();
        db.append_at(
            "a",
            &Record::Tool {
                name: "shell".into(),
                ms: 1500,
                error: true,
            },
            day(10, 9),
        )
        .unwrap();
        db.append_at(
            "a",
            &Record::Message {
                message: Message::user("<shell id=\"1\">done</shell>"),
            },
            day(11, 9),
        )
        .unwrap();
        db.append_at(
            "a",
            &Record::Message {
                message: Message::user("and the docs"),
            },
            day(11, 9),
        )
        .unwrap();
        db.append_at(
            "a",
            &Record::Step {
                model: "q/big".into(),
                usage: Usage {
                    input: 10,
                    output: 5,
                    cache_read: 0,
                    cache_write: 0,
                },
            },
            day(11, 9),
        )
        .unwrap();
        // A subagent of it.
        db.create(&info("a1", "/p", Some("a"), day(11, 9))).unwrap();
        db.append_at(
            "a1",
            &Record::Message {
                message: Message::user("look around"),
            },
            day(11, 9),
        )
        .unwrap();
        db.append_at(
            "a1",
            &Record::Step {
                model: "p/m".into(),
                usage: Usage {
                    input: 5,
                    output: 5,
                    cache_read: 0,
                    cache_write: 0,
                },
            },
            day(11, 9),
        )
        .unwrap();
        // An old-style session (no step records) in another project, three days later.
        let mut old = info("b", "/other", None, day(14, 9));
        old.usage = Usage {
            input: 50,
            output: 50,
            cache_read: 0,
            cache_write: 0,
        };
        db.create(&old).unwrap();
        db.append_at(
            "b",
            &Record::Message {
                message: Message::user("hi"),
            },
            day(14, 9),
        )
        .unwrap();
        db.append_at(
            "b",
            &Record::Message {
                message: Message {
                    role: Role::Assistant,
                    parts: vec![Part::ToolCall {
                        id: "c".into(),
                        name: "glob".into(),
                        input: json!({}),
                    }],
                },
            },
            day(14, 9),
        )
        .unwrap();
        db.append_at(
            "b",
            &Record::Message {
                message: Message {
                    role: Role::User,
                    parts: vec![Part::ToolResult {
                        id: "c".into(),
                        name: "glob".into(),
                        content: "x".into(),
                        is_error: false,
                        media: vec![],
                    }],
                },
            },
            day(14, 9),
        )
        .unwrap();
        db
    }

    #[test]
    fn aggregates() {
        let db = store();
        let price = |m: &str| {
            (m == "p/m").then_some(Cost {
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                cache_write: 0.0,
            })
        };
        let s = compute(
            &db,
            &Query {
                from: None,
                to: day(20, 0),
                directory: None,
            },
            &price,
        )
        .unwrap();
        assert_eq!((s.sessions, s.subagents), (2, 1));
        assert_eq!(
            s.prompts, 3,
            "typed prompts only; subagent tasks and shell notes are not prompts"
        );
        assert_eq!(
            s.steps, 4,
            "three step records plus one assistant message from the old session"
        );
        assert_eq!(
            s.tokens,
            Usage {
                input: 1065,
                output: 260,
                cache_read: 800,
                cache_write: 0
            }
        );
        let pm = s.models.iter().find(|m| m.model == "p/m").unwrap();
        assert_eq!(pm.steps, 3);
        assert!((pm.cost - (1055.0 * 3.0 + 255.0 * 15.0 + 800.0 * 0.3) / 1e6).abs() < 1e-12);
        assert_eq!(s.cost, pm.cost, "unpriced models cost nothing");
        let shell = s.tools.iter().find(|t| t.name == "shell").unwrap();
        assert_eq!(
            (shell.calls, shell.failed, shell.p50_ms),
            (1, 1, Some(1500))
        );
        assert_eq!(
            s.tools.iter().find(|t| t.name == "glob").unwrap().p50_ms,
            None,
            "old sessions have no timings"
        );
        assert_eq!(s.active_days, 3);
        assert_eq!(s.streak, 2, "the 10th and 11th");
        assert_eq!(s.from, day(10, 9));
    }

    #[test]
    fn records_are_read_only_as_far_as_needed() {
        let db = Store::memory().unwrap();
        db.create(&info("x", "/p", None, day(10, 9))).unwrap();
        let big = "x".repeat(100_000);
        for r in [
            Record::Message {
                message: Message {
                    role: Role::User,
                    parts: vec![
                        Part::Text("quoted \"prompt\" \u{1F600}".into()),
                        Part::Media(codesmith_ai::Media {
                            mime: "image/png".into(),
                            source: codesmith_ai::Source::Base64(big.clone()),
                            filename: None,
                        }),
                    ],
                },
            },
            Record::Message {
                message: Message {
                    role: Role::Assistant,
                    parts: vec![
                        Part::Reasoning(big.clone()),
                        Part::ToolCall {
                            id: "1".into(),
                            name: "read".into(),
                            input: json!({"path": big}),
                        },
                    ],
                },
            },
            Record::Message {
                message: Message {
                    role: Role::User,
                    parts: vec![Part::ToolResult {
                        id: "1".into(),
                        name: "read".into(),
                        content: big.clone(),
                        is_error: true,
                        media: vec![],
                    }],
                },
            },
            Record::Checkpoint {
                summary: big.clone(),
                keep_from: 1,
            },
            Record::Snapshot { tree: "t".into() },
        ] {
            db.append_at("x", &r, day(10, 10)).unwrap();
        }
        let q = Query {
            from: None,
            to: day(20, 0),
            directory: None,
        };
        let s = compute(&db, &q, &|_| None).unwrap();
        assert_eq!((s.prompts, s.steps), (1, 1));
        assert_eq!(s.tools.len(), 1);
        assert_eq!((s.tools[0].calls, s.tools[0].failed), (1, 1));
        // A damaged record is reported, as loading the session would.
        db.with_conn(|c| {
            c.execute(
                "INSERT INTO record (session_id, seq, created, body) VALUES ('x', 99, 0, '{\"kind\":\"message\",\"message\":{\"role\":\"user\"}}')",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        let e = compute(&db, &q, &|_| None).unwrap_err();
        assert!(format!("{e:#}").contains("record 99 of x"), "{e:#}");
    }

    #[test]
    fn ranges_and_projects() {
        let db = store();
        let s = compute(
            &db,
            &Query {
                from: Some(day(11, 0)),
                to: day(12, 0),
                directory: None,
            },
            &|_| None,
        )
        .unwrap();
        assert_eq!((s.sessions, s.subagents, s.prompts, s.steps), (1, 1, 1, 2));
        let p = compute(
            &db,
            &Query {
                from: None,
                to: day(20, 0),
                directory: Some("/other".into()),
            },
            &|_| None,
        )
        .unwrap();
        assert_eq!((p.sessions, p.prompts), (1, 1));
        let none = compute(
            &db,
            &Query {
                from: Some(day(1, 0)),
                to: day(2, 0),
                directory: None,
            },
            &|_| None,
        )
        .unwrap();
        assert_eq!(none.sessions, 0);
    }

    #[test]
    fn rendering() {
        let db = store();
        let s = compute(
            &db,
            &Query {
                from: None,
                to: day(20, 0),
                directory: None,
            },
            &|_| None,
        )
        .unwrap();
        let o = Render {
            label: "all time".into(),
            scope: "all projects".into(),
            models: false,
            tools: false,
            cost: false,
            limit: 10,
            color: false,
            width: 80,
        };
        let out = render(&s, &o);
        assert!(
            out.starts_with("codesmith stats · all time · all projects"),
            "{out}"
        );
        assert!(out.contains("2 sessions · 1 subagent"), "{out}");
        assert!(out.contains("3 prompts · 4 steps · 2.1k tokens"), "{out}");
        assert!(
            out.contains("66.7% tool success · 3 active days · best streak 2 days"),
            "{out}"
        );
        assert!(
            out.contains("Mo ") && out.contains("less ·░▒▓█ more"),
            "{out}"
        );
        let full = render(
            &s,
            &Render {
                models: true,
                tools: true,
                cost: true,
                ..o
            },
        );
        assert!(
            full.contains("COST & TOKENS")
                && full.contains("MODELS")
                && full.contains("TOOL RELIABILITY"),
            "{full}"
        );
        assert!(full.contains("shell") && full.contains("1.5s"), "{full}");
        let empty = compute(
            &db,
            &Query {
                from: Some(day(1, 0)),
                to: day(2, 0),
                directory: None,
            },
            &|_| None,
        )
        .unwrap();
        assert!(render(
            &empty,
            &Render {
                label: "x".into(),
                scope: "y".into(),
                models: false,
                tools: false,
                cost: false,
                limit: 10,
                color: false,
                width: 80
            }
        )
        .contains("no activity in this range"));
    }

    #[test]
    fn helpers() {
        assert_eq!(
            (
                number(999),
                number(1500),
                number(2_000_000),
                number(3_400_000_000)
            ),
            ("999".into(), "1.5k".into(), "2m".into(), "3.4b".into())
        );
        assert_eq!((level(0, 9), level(1, 9), level(9, 9)), (0, 1, 4));
        let d = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        assert_eq!(
            streak(
                &[
                    d("2026-01-01"),
                    d("2026-01-02"),
                    d("2026-01-04"),
                    d("2026-01-05"),
                    d("2026-01-06")
                ]
                .into_iter()
                .collect()
            ),
            3
        );
        let (from, to, label) = range(Some(7), None, false);
        assert!(from.unwrap() < to && label == "last 7 days");
        assert_eq!(range(None, None, true).0, None);
        assert_eq!(range(Some(0), None, false).2, "today");
    }
}
