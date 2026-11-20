//! Code review: `/calibrate` derives review dimensions from the whole
//! codebase, `/review` runs one reviewer per relevant dimension in
//! parallel, then has each finding checked by an independent verifier and
//! keeps only the confirmed ones. The method (independent parallel
//! reviewers, high-signal rules, a false-positive list and per-finding
//! validation) builds on Anthropic's code-review plugin; see
//! `prompt/review/NOTICE.md`.

pub mod dims;
pub mod facts;
pub mod target;

use crate::config::Loaded;
use crate::permission::{Fixed, Reply};
use crate::session::{Options, Session, Sink};
use crate::store::Store;
use anyhow::{anyhow, Result};
use codesmith_ai::{Part, Role, Usage};
use dims::{Calibration, Dimension};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use target::Target;

const REVIEWER: &str = include_str!("../prompt/review/reviewer.txt");
const VERIFIER: &str = include_str!("../prompt/review/verifier.txt");
const SURVEY: &str = include_str!("../prompt/review/survey.txt");
const SYNTHESIZE: &str = include_str!("../prompt/review/synthesize.txt");

/// A prompt file without its attribution header.
fn body(p: &str) -> &str {
    p.split_once("---8<---\n").map(|(_, b)| b).unwrap_or(p)
}

fn fill(template: &str, vars: &[(&str, &str)]) -> String {
    let mut s = body(template).to_string();
    for (k, v) in vars {
        s = s.replace(&format!("${{{k}}}"), v);
    }
    s
}

/// `review` in codesmith.json.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct ReviewConfig {
    /// Model for reviewers and verifiers (default: the session's model).
    pub model: Option<String>,
    /// Child sessions run at once.
    pub concurrency: usize,
    /// Minimum verifier confidence (0-100) for a finding to be reported.
    pub threshold: u8,
    /// Diff characters given to each reviewer inline.
    #[serde(alias = "maxDiffChars")]
    pub max_diff_chars: usize,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        ReviewConfig {
            model: None,
            concurrency: 3,
            threshold: 80,
            max_diff_chars: 60_000,
        }
    }
}

/// Where and how child sessions run.
pub struct Env {
    pub loaded: Loaded,
    pub cwd: PathBuf,
    pub root: PathBuf,
    pub model: Option<String>,
    pub store: Option<PathBuf>,
    pub parent: Option<String>,
    pub mcp: Option<Arc<crate::mcp::Mcp>>,
}

impl Env {
    pub fn new(
        loaded: &Loaded,
        cwd: PathBuf,
        model: Option<String>,
        store: Option<PathBuf>,
        parent: Option<String>,
        mcp: Option<Arc<crate::mcp::Mcp>>,
    ) -> Env {
        let mut loaded = loaded.clone();
        // Read-only children need no undo snapshots.
        loaded.config.snapshots = Some(false);
        let model = loaded.config.review.model.clone().or(model);
        Env {
            root: loaded.root.clone(),
            loaded,
            cwd,
            model,
            store,
            parent,
            mcp,
        }
    }

    fn cfg(&self) -> &ReviewConfig {
        &self.loaded.config.review
    }

    /// Run one read-only child session and return its final answer.
    async fn ask(&self, title: String, prompt: String) -> Result<(String, Usage)> {
        let store = match &self.store {
            Some(p) => Some(Store::open(p)?),
            None => None,
        };
        let opts = Options {
            agent: Some("review".into()),
            model: self.model.clone(),
            resume: None,
            title: Some(title),
            parent: self.parent.clone(),
            questioner: None,
            mcp: self.mcp.clone(),
        };
        let mut s = Session::new(
            &self.loaded,
            self.cwd.clone(),
            opts,
            Arc::new(Fixed(Reply::Reject(Some(
                "Reviewers are read-only.".into(),
            )))),
            store,
        )?;
        let r = s
            .prompt_from(crate::session::Origin::Engine, &prompt, vec![], &mut Quiet)
            .await;
        let answer = s
            .messages
            .iter()
            .rev()
            .find(|m| {
                m.role == Role::Assistant
                    && m.parts
                        .iter()
                        .any(|p| matches!(p, Part::Text(t) if !t.trim().is_empty()))
            })
            .map(|m| {
                m.parts
                    .iter()
                    .filter_map(|p| {
                        if let Part::Text(t) = p {
                            Some(t.as_str())
                        } else {
                            None
                        }
                    })
                    .collect::<String>()
            })
            .unwrap_or_default();
        r?;
        Ok((answer, s.usage))
    }

    /// Ask, and if the answer has no parseable JSON, ask once more for just the JSON.
    async fn ask_json(&self, title: String, prompt: String) -> Result<(Value, Usage)> {
        let (a, mut u) = self.ask(title.clone(), prompt.clone()).await?;
        if let Some(v) = dims::extract_json(&a) {
            return Ok((v, u));
        }
        let retry = format!("{prompt}\n\nYour previous answer did not end with the required JSON object. Here it is; restate it as only the JSON object in a ```json block:\n\n{a}");
        let (a, u2) = self.ask(title, retry).await?;
        add(&mut u, &u2);
        dims::extract_json(&a)
            .map(|v| (v, u))
            .ok_or_else(|| anyhow!("the model did not return the required JSON"))
    }
}

struct Quiet;
impl Sink for Quiet {
    fn text(&mut self, _: &str) {}
}

fn add(a: &mut Usage, b: &Usage) {
    a.input += b.input;
    a.output += b.output;
    a.cache_read += b.cache_read;
    a.cache_write += b.cache_write;
}

/// Progress lines for the user.
pub type Progress<'a> = &'a (dyn Fn(String) + Send + Sync);

// ----- calibrate -------------------------------------------------------------

pub struct Calibrated {
    pub calibration: Calibration,
    pub path: PathBuf,
    pub usage: Usage,
}

pub async fn calibrate(env: &Env, progress: Progress<'_>) -> Result<Calibrated> {
    let facts = tokio::task::block_in_place(|| facts::gather(&env.root));
    if facts.files.is_empty() {
        return Err(anyhow!(
            "{} has no tracked files to calibrate on",
            env.root.display()
        ));
    }
    progress(format!(
        "calibrating on {} files in {} areas",
        facts.files.len(),
        facts.areas.len()
    ));
    let mut usage = Usage::default();
    let jobs: Vec<_> = facts
        .areas
        .iter()
        .map(|a| {
            let prompt = fill(
                SURVEY,
                &[
                    ("area", &a.name),
                    ("paths", &a.paths.join(", ")),
                    ("facts", &facts.for_area(a)),
                ],
            );
            let (name, title) = (a.name.clone(), format!("Survey {}", a.name));
            async move { (name, env.ask(title, prompt).await) }
        })
        .collect();
    let mut surveys = Vec::new();
    let mut results =
        futures_util::stream::iter(jobs).buffer_unordered(env.cfg().concurrency.max(1));
    while let Some((name, r)) = results.next().await {
        match r {
            Ok((text, u)) => {
                add(&mut usage, &u);
                progress(format!("surveyed {name}"));
                surveys.push((name, text));
            }
            Err(e) => progress(format!("survey of {name} failed: {e:#}")),
        }
    }
    drop(results);
    if surveys.is_empty() {
        return Err(anyhow!("every area survey failed"));
    }
    surveys.sort();
    let survey_text = surveys
        .iter()
        .map(|(n, t)| format!("### {n}\n{}\n", t.trim()))
        .collect::<String>();
    progress("choosing review dimensions".into());
    let prompt = fill(
        SYNTHESIZE,
        &[("facts", &facts.render()), ("surveys", &survey_text)],
    );
    let mut last_err = String::new();
    for attempt in 0..2 {
        let p = if attempt == 0 {
            prompt.clone()
        } else {
            format!("{prompt}\n\nThe previous answer was rejected: {last_err}. Fix that and answer again.")
        };
        let (v, u) = env
            .ask_json("Calibrate review dimensions".into(), p)
            .await?;
        add(&mut usage, &u);
        let parsed: Result<Vec<Dimension>> = serde_json::from_value(v["dimensions"].clone())
            .map_err(|e| anyhow!("dimensions are not in the required shape: {e}"));
        let dims = parsed
            .map(dims::normalize)
            .and_then(|d| dims::validate(&d).map(|_| d));
        match dims {
            Ok(d) if d.len() >= 2 => {
                let calibration = Calibration {
                    version: dims::VERSION,
                    generated: chrono::Utc::now()
                        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                    commit: target::git(&env.root, &["rev-parse", "--short", "HEAD"])
                        .ok()
                        .map(|s| s.trim().to_string()),
                    summary: v["summary"].as_str().unwrap_or("").trim().to_string(),
                    dimensions: d,
                };
                let path = dims::save(&env.root, &calibration)?;
                return Ok(Calibrated {
                    calibration,
                    path,
                    usage,
                });
            }
            Ok(d) => last_err = format!("only {} usable dimension(s); give at least 4", d.len()),
            Err(e) => last_err = format!("{e:#}"),
        }
    }
    Err(anyhow!("could not get valid review dimensions: {last_err}"))
}

pub fn render_calibration(c: &Calibrated) -> String {
    let mut s = format!("## Review calibration\n\n{}\n\n", c.calibration.summary);
    for d in &c.calibration.dimensions {
        let scope = if d.paths.is_empty() {
            "everywhere".to_string()
        } else {
            d.paths.join(", ")
        };
        s.push_str(&format!(
            "- **{}** (`{}`, {scope}): {}\n",
            d.name, d.id, d.focus
        ));
    }
    s.push_str(&format!("\nAlso always checked: correctness and project instructions.\nSaved to `{}`; edit it to adjust, and commit it to share with the team. Run `/review` to use it.\n", c.path.display()));
    s
}

// ----- review ----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Finding {
    #[serde(default)]
    pub file: String,
    #[serde(default)]
    pub line: Option<u64>,
    #[serde(default = "medium")]
    pub severity: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub failure_scenario: String,
    #[serde(skip)]
    pub dimension: String,
}

fn medium() -> String {
    "medium".into()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Verified {
    pub finding: Finding,
    pub confidence: u8,
    pub reason: String,
}

pub struct Report {
    pub target: Target,
    pub dimensions: Vec<Dimension>,
    pub calibrated: bool,
    pub raised: usize,
    pub confirmed: Vec<Verified>,
    pub failed: Vec<String>,
    pub usage: Usage,
}

fn rank(sev: &str) -> u8 {
    match sev {
        "high" => 0,
        "medium" => 1,
        _ => 2,
    }
}

fn words(s: &str) -> std::collections::HashSet<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 2)
        .map(str::to_string)
        .collect()
}

/// Merge findings that describe the same problem: same file, nearby lines,
/// overlapping wording. The most severe copy is kept.
pub fn dedupe(mut v: Vec<Finding>) -> Vec<Finding> {
    v.sort_by_key(|f| rank(&f.severity));
    let mut out: Vec<Finding> = Vec::new();
    for f in v {
        let dup = out.iter().any(|g| {
            let near = match (f.line, g.line) {
                (Some(a), Some(b)) => a.abs_diff(b) <= 3,
                _ => true,
            };
            let (a, b) = (words(&f.title), words(&g.title));
            let overlap = a.intersection(&b).count() as f32 / a.len().min(b.len()).max(1) as f32;
            g.file == f.file && near && overlap >= 0.5
        });
        if !dup {
            out.push(f);
        }
    }
    out
}

fn instruction_files(env: &Env, files: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for name in ["AGENTS.md", "CLAUDE.md"] {
        if env.root.join(name).is_file() {
            out.push(name.to_string());
        }
        for f in files {
            let mut p = std::path::Path::new(f).parent();
            while let Some(dir) = p.filter(|d| !d.as_os_str().is_empty()) {
                let cand = dir.join(name);
                if env.root.join(&cand).is_file() && !out.contains(&cand.display().to_string()) {
                    out.push(cand.display().to_string());
                }
                p = dir.parent();
            }
        }
    }
    out
}

pub async fn review(env: &Env, args: &target::Args, progress: Progress<'_>) -> Result<Report> {
    let t = tokio::task::block_in_place(|| target::resolve(&env.root, args.target.as_deref()))?;
    let cal = dims::load(&env.root)?;
    let calibrated = cal.is_some();
    let instructions = instruction_files(env, &t.files);
    let mut dims = dims::always(!instructions.is_empty());
    if let Some(c) = &cal {
        dims.extend(
            c.dimensions
                .iter()
                .filter(|d| dims::applies(d, &t.files))
                .cloned(),
        );
    }
    progress(format!(
        "reviewing {} ({} file(s)) across {} dimension(s): {}{}",
        t.label,
        t.files.len(),
        dims.len(),
        dims.iter()
            .map(|d| d.id.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        if calibrated {
            ""
        } else {
            " — run /calibrate for dimensions tuned to this codebase"
        }
    ));
    let max = env.cfg().max_diff_chars;
    let (diff, note) = if t.diff.len() > max {
        let cut = (0..=max)
            .rev()
            .find(|i| t.diff.is_char_boundary(*i))
            .unwrap_or(0);
        (&t.diff[..cut], format!("(The diff is {} characters; only the first {max} are shown. Run `{}` to see the rest.)", t.diff.len(), t.command))
    } else {
        (t.diff.as_str(), String::new())
    };
    let files = t
        .files
        .iter()
        .map(|f| format!("- {f}"))
        .collect::<Vec<_>>()
        .join("\n");
    let context = if t.context.trim().is_empty() {
        String::new()
    } else {
        format!("Author's description:\n{}\n", t.context.trim())
    };
    let inst = if instructions.is_empty() {
        String::new()
    } else {
        format!("Project instruction files that may apply (read the ones relevant to your dimension): {}\n", instructions.join(", "))
    };

    let mut usage = Usage::default();
    let mut failed = Vec::new();
    let jobs: Vec<_> = dims
        .iter()
        .map(|d| {
            let checks = if d.checks.is_empty() {
                String::new()
            } else {
                format!(
                    "Check in particular:\n{}",
                    d.checks
                        .iter()
                        .map(|c| format!("- {c}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            };
            let scope = if d.paths.is_empty() {
                String::new()
            } else {
                format!("\nThis dimension covers: {}", d.paths.join(", "))
            };
            let prompt = fill(
                REVIEWER,
                &[
                    ("name", &d.name),
                    ("id", &d.id),
                    ("focus", &format!("{}{scope}", d.focus)),
                    ("checks", &checks),
                    ("target", &t.label),
                    ("context", &context),
                    ("files", &files),
                    ("instructions", &inst),
                    ("diff", diff),
                    ("diff_note", &note),
                ],
            );
            let (id, title) = (d.id.clone(), format!("Review: {}", d.name));
            async move { (id, env.ask_json(title, prompt).await) }
        })
        .collect();
    let mut raised = Vec::new();
    let mut stream =
        futures_util::stream::iter(jobs).buffer_unordered(env.cfg().concurrency.max(1));
    while let Some((id, r)) = stream.next().await {
        match r {
            Ok((v, u)) => {
                add(&mut usage, &u);
                let found: Vec<Finding> = v["findings"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|f| serde_json::from_value::<Finding>(f.clone()).ok())
                    .filter(|f| !f.title.trim().is_empty())
                    .take(8)
                    .map(|mut f| {
                        f.dimension = id.clone();
                        f.severity = f.severity.to_lowercase();
                        f
                    })
                    .collect();
                progress(format!("{id}: {} finding(s)", found.len()));
                raised.extend(found);
            }
            Err(e) => {
                progress(format!("{id}: reviewer failed: {e:#}"));
                failed.push(id);
            }
        }
    }
    drop(stream);
    let total = raised.len();
    let candidates = dedupe(raised);
    if !candidates.is_empty() {
        progress(format!("verifying {} finding(s)", candidates.len()));
    }
    let threshold = env.cfg().threshold;
    let jobs: Vec<_> = candidates
        .into_iter()
        .map(|f| {
            let location = match f.line {
                Some(l) => format!("{}:{l}", f.file),
                None => f.file.clone(),
            };
            let dim_name = dims
                .iter()
                .find(|d| d.id == f.dimension)
                .map(|d| format!("{} — {}", d.name, d.focus))
                .unwrap_or_else(|| f.dimension.clone());
            let prompt = fill(
                VERIFIER,
                &[
                    ("target", &t.label),
                    ("context", &context),
                    ("files", &files),
                    ("diff_command", &t.command),
                    ("dimension", &dim_name),
                    ("location", &location),
                    ("title", &f.title),
                    ("detail", &f.detail),
                    ("failure_scenario", &f.failure_scenario),
                ],
            );
            let title = format!("Verify: {}", f.title.chars().take(60).collect::<String>());
            async move {
                let r = env.ask_json(title, prompt).await;
                (f, r)
            }
        })
        .collect();
    let mut confirmed = Vec::new();
    let mut stream =
        futures_util::stream::iter(jobs).buffer_unordered(env.cfg().concurrency.max(1));
    while let Some((f, r)) = stream.next().await {
        match r {
            Ok((v, u)) => {
                add(&mut usage, &u);
                let verdict = v["verdict"].as_str().unwrap_or("uncertain");
                let conf = v["confidence"].as_f64().unwrap_or(0.0).clamp(0.0, 100.0) as u8;
                if verdict == "confirmed" && conf >= threshold {
                    confirmed.push(Verified {
                        finding: f,
                        confidence: conf,
                        reason: v["reason"].as_str().unwrap_or("").to_string(),
                    });
                }
            }
            Err(e) => progress(format!("verifying \"{}\" failed: {e:#}", f.title)),
        }
    }
    drop(stream);
    confirmed.sort_by(|a, b| {
        rank(&a.finding.severity)
            .cmp(&rank(&b.finding.severity))
            .then(b.confidence.cmp(&a.confidence))
    });
    Ok(Report {
        target: t,
        dimensions: dims,
        calibrated,
        raised: total,
        confirmed,
        failed,
        usage,
    })
}

impl Report {
    pub fn render(&self) -> String {
        let names = self
            .dimensions
            .iter()
            .map(|d| d.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let mut s = format!("## Code review: {}\n\n", self.target.label);
        if self.confirmed.is_empty() {
            s.push_str(&format!("No issues found. Checked: {names}.\n"));
        } else {
            for (i, v) in self.confirmed.iter().enumerate() {
                let f = &v.finding;
                let loc = match f.line {
                    Some(l) => format!("{}:{l}", f.file),
                    None => f.file.clone(),
                };
                s.push_str(&format!(
                    "{}. **[{}] {}** — `{loc}` ({})\n",
                    i + 1,
                    f.severity,
                    f.title.trim(),
                    f.dimension
                ));
                if !f.detail.trim().is_empty() {
                    s.push_str(&format!("   {}\n", f.detail.trim().replace('\n', "\n   ")));
                }
                if !f.failure_scenario.trim().is_empty() {
                    s.push_str(&format!("   Failure: {}\n", f.failure_scenario.trim()));
                }
                s.push_str(&format!(
                    "   Verified ({}% confidence): {}\n\n",
                    v.confidence,
                    v.reason.trim()
                ));
            }
            s.push_str(&format!("Checked: {names}.\n"));
        }
        s.push_str(&format!(
            "\n{} finding(s) raised, {} confirmed by independent verification.",
            self.raised,
            self.confirmed.len()
        ));
        if !self.failed.is_empty() {
            s.push_str(&format!(
                " Reviewers that failed: {}.",
                self.failed.join(", ")
            ));
        }
        if !self.calibrated {
            s.push_str(" Run `/calibrate` to add review dimensions tuned to this codebase.");
        }
        s.push('\n');
        s
    }
}

/// Post the report to the pull request it reviewed.
pub fn comment(root: &std::path::Path, r: &Report) -> Result<()> {
    let n = r
        .target
        .pr
        .ok_or_else(|| anyhow!("--comment only applies to a pull request"))?;
    let out = std::process::Command::new("gh")
        .env_remove(crate::SERVER_TOKEN_VAR)
        .args(["pr", "comment", &n.to_string(), "--body", &r.render()])
        .current_dir(root)
        .output()?;
    if !out.status.success() {
        return Err(anyhow!(
            "gh pr comment: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// Run a native command (`review` or `calibrate`) and return its report.
pub async fn run_command(
    name: &str,
    args: &str,
    env: &Env,
    progress: Progress<'_>,
) -> Result<(String, Usage)> {
    match name {
        "calibrate" => {
            let c = calibrate(env, progress).await?;
            Ok((render_calibration(&c), c.usage))
        }
        "review" => {
            let a = target::parse_args(args);
            let r = review(env, &a, progress).await?;
            let mut text = r.render();
            if a.comment {
                comment(&env.root, &r)?;
                text.push_str("\nPosted this review to the pull request.\n");
            }
            Ok((text, r.usage))
        }
        other => Err(anyhow!("/{other} is not a native command")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(file: &str, line: u64, sev: &str, title: &str) -> Finding {
        Finding {
            file: file.into(),
            line: Some(line),
            severity: sev.into(),
            title: title.into(),
            detail: String::new(),
            failure_scenario: String::new(),
            dimension: "d".into(),
        }
    }

    #[test]
    fn prompts_lose_their_headers_and_fill() {
        for p in [REVIEWER, VERIFIER] {
            assert!(p.starts_with("Adapted for CodeSmith from Anthropic"));
            assert!(!body(p).contains("Apache"));
        }
        assert!(!fill(REVIEWER, &[]).is_empty());
        let s = fill(
            VERIFIER,
            &[("title", "X is null"), ("target", "commit abc")],
        );
        assert!(s.contains("Claim: X is null") && s.contains("Target: commit abc"));
        for t in [REVIEWER, VERIFIER, SURVEY, SYNTHESIZE] {
            assert!(t.contains("${"), "templates have placeholders");
        }
    }

    #[test]
    fn duplicates_merge_keeping_the_most_severe() {
        let v = dedupe(vec![
            f("a.rs", 10, "medium", "Null pointer dereference in parse"),
            f("a.rs", 11, "high", "parse dereferences a null pointer"),
            f("a.rs", 40, "low", "Unrelated leak of file handle"),
            f("b.rs", 10, "medium", "Null pointer dereference in parse"),
        ]);
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].severity, "high");
    }

    #[test]
    fn findings_parse_leniently() {
        let x: Finding =
            serde_json::from_value(serde_json::json!({"file": "a.rs", "title": "t"})).unwrap();
        assert_eq!((x.severity.as_str(), x.line), ("medium", None));
    }

    #[test]
    fn report_text() {
        let t = Target {
            label: "commit abc".into(),
            diff: "d".into(),
            files: vec!["a.rs".into()],
            context: String::new(),
            command: "git show abc".into(),
            pr: None,
        };
        let mut r = Report {
            target: t,
            dimensions: dims::always(false),
            calibrated: false,
            raised: 2,
            confirmed: vec![],
            failed: vec![],
            usage: Usage::default(),
        };
        let s = r.render();
        assert!(s.contains("No issues found. Checked: Correctness."), "{s}");
        assert!(s.contains("/calibrate"));
        r.confirmed.push(Verified {
            finding: f("a.rs", 3, "high", "Off by one"),
            confidence: 91,
            reason: "loop runs past end".into(),
        });
        r.calibrated = true;
        let s = r.render();
        assert!(
            s.contains("1. **[high] Off by one** — `a.rs:3` (d)") && s.contains("91% confidence"),
            "{s}"
        );
        assert!(!s.contains("/calibrate"));
    }
}
