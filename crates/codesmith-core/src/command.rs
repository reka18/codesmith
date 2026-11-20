//! Custom commands: prompt templates run as `/name args`. Defined in config
//! (`commands`) or as Markdown files in `commands/` folders, with an
//! optional `description`/`agent`/`model`/`subagent` header. `$ARGUMENTS`
//! is the whole argument string and `$1`, `$2`… the individual arguments
//! (the last one used takes the rest); with neither, the arguments are
//! appended. Then each `` !`cmd` `` is replaced by what `cmd` prints (run
//! with the configured shell in the session's folder). Ported from OpenCode
//! and pinned to its template rules by the `oc_command_*` tests (from
//! `packages/core/test/config/command.test.ts` and
//! `config/command-subagent.test.ts`).
//!
//! A command with an `agent` switches the session to that agent, and one
//! with a `model` to that model, before it runs. A command with
//! `subagent: true` (or the legacy `subtask: true`), or whose agent is a
//! subagent, runs in a child session instead and leaves the session's own
//! agent and model alone; `subagent: false` keeps it in the session.
//!
//! Command files are `commands/**/*.md` (and `command/`) in each config
//! folder (the global one and each `.codesmith/`), read in config order with
//! the config files, so a later definition, inline or file, replaces an
//! earlier one whole. A nested file `commands/git/sync.md` is `/git/sync`.
//! Claude Code's `~/.claude/commands` and `.claude/commands` are read too,
//! below every config layer.
//!
//! Deliberate difference: a subagent command's child runs to completion
//! before the command returns (OpenCode runs it in the background), and
//! its report joins the session as an exchange.

use crate::skill::frontmatter;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const INITIALIZE: &str = include_str!("prompt/initialize.txt");

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Command {
    pub template: String,
    pub description: Option<String>,
    pub agent: Option<String>,
    /// `provider/model[#variant]` or `{"providerID", "model", "variant"}`.
    #[serde(deserialize_with = "crate::config::model_selection")]
    pub model: Option<String>,
    /// Run in a child session (`true`), or in the session even when the
    /// agent is a subagent (`false`).
    pub subagent: Option<bool>,
    /// The legacy name of `subagent` (`subagent` wins when both are set).
    pub subtask: Option<bool>,
    /// Handled by CodeSmith itself rather than expanded into a prompt
    /// (`/review`, `/calibrate`). A same-named custom command replaces it.
    #[serde(skip)]
    pub native: bool,
    /// An MCP server's prompt, `(server, prompt)`, fetched with `prompts/get`
    /// when run (`/server:prompt args`).
    #[serde(skip)]
    pub mcp: Option<(String, String)>,
    /// A plugin's command run as code (see [`crate::plugin::Command`]),
    /// by name.
    #[serde(skip)]
    pub plugin: Option<String>,
}

impl Command {
    /// Whether the command runs in a child session: `subagent` (else
    /// `subtask`) when set, else whether its agent is a subagent.
    pub fn runs_as_subagent(&self, config: &crate::config::Config) -> bool {
        if self.native || self.mcp.is_some() || self.plugin.is_some() {
            return false;
        }
        self.subagent.or(self.subtask).unwrap_or_else(|| {
            self.agent
                .as_deref()
                .and_then(|a| crate::agent::resolve(config, a))
                .is_some_and(|a| a.mode == "subagent")
        })
    }
}

impl Command {
    /// The agent and model the session should switch to before the
    /// command runs: the command's own, unless it runs as a subagent.
    pub fn switches(&self, config: &crate::config::Config) -> (Option<String>, Option<String>) {
        if self.runs_as_subagent(config) {
            return (None, None);
        }
        (self.agent.clone(), self.model.clone())
    }
}

/// Built-in commands: `/init`, plus the native `/review` and `/calibrate`.
pub fn builtins(project: &Path) -> BTreeMap<String, Command> {
    let mut m = BTreeMap::new();
    m.insert(
        "init".into(),
        Command {
            template: INITIALIZE.replace("${path}", &project.display().to_string()),
            description: Some("guided AGENTS.md setup".into()),
            ..Default::default()
        },
    );
    m.insert(
        "review".into(),
        Command {
            description: Some("review changes [commit|range|branch|#pr] [--comment] with parallel reviewers and verification; defaults to uncommitted".into()),
            native: true,
            ..Default::default()
        },
    );
    m.insert(
        "calibrate".into(),
        Command {
            description: Some(
                "survey the codebase and choose the review dimensions /review uses".into(),
            ),
            native: true,
            ..Default::default()
        },
    );
    m
}

/// The commands of the installed plugins `config` enables, which sit above
/// the built-ins and below command files and config.
pub fn plugin_commands(config: &crate::config::Config) -> BTreeMap<String, Command> {
    crate::plugin::active()
        .commands(config)
        .into_iter()
        .map(|c| {
            let name = c.name().to_string();
            let cmd = Command {
                description: c.description().map(str::to_string),
                plugin: Some(name.clone()),
                ..Default::default()
            };
            (name, cmd)
        })
        .collect()
}

/// Claude Code's command folders, lowest precedence first: `<name>.md`
/// files there sit below every config layer. (CodeSmith's own `commands/`
/// folders are config layers: see [`documents`].)
pub fn roots(cwd: &Path, root: &Path, _config_dir: &Path, home: &Path) -> Vec<PathBuf> {
    if std::env::var_os("CODESMITH_DISABLE_CLAUDE_CODE").is_some() {
        return Vec::new();
    }
    let mut out = vec![home.join(".claude/commands")];
    let mut dirs: Vec<&Path> = cwd
        .ancestors()
        .take_while(|d| d.starts_with(root))
        .collect();
    dirs.reverse();
    for d in dirs {
        out.push(d.join(".claude/commands"));
    }
    out
}

/// A command file: its header and its body as the template. `None` when
/// a field has the wrong type (`subagent: maybe`).
pub fn parse_markdown(text: &str) -> Option<Command> {
    let (meta, body) = frontmatter(text);
    let flag = |k: &str| match meta.get(k).map(|v| v.trim()) {
        None => Ok(None),
        Some("true") => Ok(Some(true)),
        Some("false") => Ok(Some(false)),
        Some(_) => Err(()),
    };
    Some(Command {
        template: body.trim().to_string(),
        description: meta.get("description").cloned(),
        agent: meta.get("agent").cloned(),
        model: meta.get("model").cloned(),
        subagent: flag("subagent").ok()?,
        subtask: flag("subtask").ok()?,
        native: false,
        mcp: None,
        plugin: None,
    })
}

pub fn from_markdown(text: &str) -> Command {
    parse_markdown(text).unwrap_or_default()
}

/// The commands defined by files in config folder `dir`, as a config layer
/// `{"commands": {...}}`: `command[s]/**/*.md`, named by their path below
/// that folder without `.md` (`commands/git/sync.md` is `git/sync`), in
/// path order. Files with a malformed header are skipped.
pub fn documents(dir: &Path) -> Value {
    let mut out = serde_json::Map::new();
    for sub in ["command", "commands"] {
        let base = dir.join(sub);
        let mut files = Vec::new();
        markdown_files(&base, &mut files);
        files.sort();
        for p in files {
            let Some(name) = p
                .strip_prefix(&base)
                .ok()
                .and_then(|r| r.with_extension("").to_str().map(|s| s.replace('\\', "/")))
            else {
                continue;
            };
            let Some(c) = std::fs::read_to_string(&p)
                .ok()
                .and_then(|t| parse_markdown(&t))
            else {
                continue;
            };
            let mut v = json!({ "template": c.template });
            for (k, f) in [
                ("description", &c.description),
                ("agent", &c.agent),
                ("model", &c.model),
            ] {
                if let Some(f) = f {
                    v[k] = json!(f);
                }
            }
            for (k, f) in [("subagent", c.subagent), ("subtask", c.subtask)] {
                if let Some(f) = f {
                    v[k] = json!(f);
                }
            }
            out.insert(name, v);
        }
    }
    if out.is_empty() {
        json!({})
    } else {
        json!({ "commands": out })
    }
}

fn markdown_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        let Ok(meta) = std::fs::metadata(&p) else {
            continue;
        };
        if meta.is_dir() {
            if !matches!(
                p.file_name().and_then(|n| n.to_str()),
                Some("node_modules" | ".git")
            ) {
                markdown_files(&p, out);
            }
        } else if p.extension().and_then(|x| x.to_str()) == Some("md") {
            out.push(p);
        }
    }
}

/// Every command: built-ins, then Claude Code's command files, then config
/// (its inline commands and command files in config order; highest).
pub fn discover(
    builtins: BTreeMap<String, Command>,
    roots: &[PathBuf],
    configured: &BTreeMap<String, Command>,
) -> BTreeMap<String, Command> {
    let mut all = builtins;
    for r in roots {
        let Ok(entries) = std::fs::read_dir(r) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("md") {
                continue;
            }
            let Some(name) = p.file_stem().and_then(|s| s.to_str()).map(str::to_string) else {
                continue;
            };
            if let Some(c) = std::fs::read_to_string(&p)
                .ok()
                .and_then(|t| parse_markdown(&t))
            {
                all.insert(name, c);
            }
        }
    }
    for (k, v) in configured {
        all.insert(k.clone(), v.clone());
    }
    all
}

/// Split arguments like a shell would for quoting purposes (OpenCode's
/// `parseArguments`): quoted strings stay whole, quotes are removed.
pub fn parse_args(input: &str) -> Vec<String> {
    let re = regex::Regex::new(r#""[^"]*"|'[^']*'|[^\s"']+"#).unwrap();
    re.find_iter(input)
        .map(|m| {
            let s = m.as_str();
            s.strip_prefix(['"', '\''])
                .and_then(|x| x.strip_suffix(['"', '\'']))
                .unwrap_or(s)
                .to_string()
        })
        .collect()
}

/// Fill a template with arguments in one pass, so text that arrives in the
/// arguments (`$1`, `$ARGUMENTS`, `$&`) is never expanded again. The highest
/// `$N` in the template takes all remaining arguments.
pub fn expand(template: &str, args: &str) -> String {
    let value = args.trim();
    let placeholder = regex::Regex::new(r"\$(?:ARGUMENTS|([1-9]\d*))").unwrap();
    let positions: Vec<usize> = placeholder
        .captures_iter(template)
        .filter_map(|c| c.get(1)?.as_str().parse().ok())
        .collect();
    if positions.is_empty() && !template.contains("$ARGUMENTS") {
        return [template.trim(), value]
            .iter()
            .filter(|s| !s.is_empty())
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n");
    }
    let last = positions.iter().copied().max().unwrap_or(0);
    let parts = parse_args(value);
    placeholder
        .replace_all(template, |c: &regex::Captures| {
            let Some(n) = c.get(1) else {
                return value.to_string();
            };
            let position: usize = n.as_str().parse().unwrap_or(usize::MAX);
            match parts.get(position - 1..) {
                Some(rest) if !rest.is_empty() && position == last => rest.join(" "),
                Some(rest) if !rest.is_empty() => rest[0].clone(),
                _ => String::new(),
            }
        })
        .trim()
        .to_string()
}

/// Replace each `` !`cmd` `` in `text` with what `cmd` prints (stdout and
/// stderr together), run with `shell` in `cwd` with no input, two at a
/// time, as OpenCode's command templates do. Runs after argument
/// expansion, so arguments can supply commands, as in OpenCode: a command
/// is the user's own, and runs without a permission prompt. A command that
/// cannot start fails the whole template.
pub async fn interpolate_shell(text: &str, shell: &str, cwd: &Path) -> anyhow::Result<String> {
    let re = regex::Regex::new(r"!`([^`]+)`").unwrap();
    let sources: Vec<String> = re.captures_iter(text).map(|c| c[1].to_string()).collect();
    if sources.is_empty() {
        return Ok(text.to_string());
    }
    let run = |source: String| async move {
        let out = tokio::process::Command::new(shell)
            .args(crate::tool::shell::shell_args(shell))
            .arg(&source)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .env_remove(crate::SERVER_TOKEN_VAR)
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("Shell interpolation failed for {source:?}: {e}"))?;
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        anyhow::Ok(text)
    };
    let mut outputs = Vec::with_capacity(sources.len());
    let mut pending = sources.into_iter();
    loop {
        let batch: Vec<String> = pending.by_ref().take(2).collect();
        if batch.is_empty() {
            break;
        }
        for r in futures_util::future::join_all(batch.into_iter().map(run)).await {
            outputs.push(r?);
        }
    }
    let mut outputs = outputs.into_iter();
    Ok(re
        .replace_all(text, |_: &regex::Captures| {
            outputs.next().unwrap_or_default()
        })
        .into_owned())
}

/// A command invocation in `message` naming one of `known`: as
/// [`parse_invocation`], and also a nested command (`/git/sync args`),
/// which only counts when it exists, so a path stays a message.
pub fn find_invocation<T>(message: &str, known: &BTreeMap<String, T>) -> Option<(String, String)> {
    if let Some(found) = parse_invocation(message) {
        return Some(found);
    }
    let m = message.trim_start().strip_prefix('/')?;
    let (name, rest) = match m.find(char::is_whitespace) {
        Some(i) => (&m[..i], m[i..].trim()),
        None => (m, ""),
    };
    known
        .contains_key(name)
        .then(|| (name.to_string(), rest.to_string()))
}

/// `/name rest` → (name, rest), if the message is a command invocation.
pub fn parse_invocation(message: &str) -> Option<(String, String)> {
    let m = message.trim_start().strip_prefix('/')?;
    let (name, rest) = match m.find(char::is_whitespace) {
        Some(i) => (&m[..i], m[i..].trim()),
        None => (m, ""),
    };
    (!name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.')))
    .then(|| (name.to_string(), rest.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expansion() {
        assert_eq!(
            expand("Fix issue $ARGUMENTS now", "#42 quickly"),
            "Fix issue #42 quickly now"
        );
        assert_eq!(
            expand("Compare $1 with $2", "\"file a.rs\" b.rs"),
            "Compare file a.rs with b.rs"
        );
        assert_eq!(
            expand("Missing $3", "a"),
            "Missing",
            "trimmed, as in OpenCode"
        );
        assert_eq!(
            expand("Review the code", "focus on errors"),
            "Review the code\n\nfocus on errors"
        );
        assert_eq!(expand("Review the code\n", ""), "Review the code");
    }

    /// OpenCode's interpolation cases, verbatim.
    #[test]
    fn oc_command_interpolates_literal_input() {
        let mut cases: Vec<(&str, String, String)> = Vec::new();
        for input in ["$&", "$$", "$`", "$'"] {
            cases.push((
                "Explain $ARGUMENTS.",
                input.into(),
                format!("Explain {input}."),
            ));
            cases.push((
                "Explain $1.",
                format!("\"{input}\""),
                format!("Explain {input}."),
            ));
            cases.push(("Explain.", input.into(), format!("Explain.\n\n{input}")));
        }
        for input in ["abc", "", "alpha beta", "\"alpha beta\"", "$1", "$<name>"] {
            cases.push((
                "Explain $ARGUMENTS.",
                input.into(),
                format!("Explain {input}."),
            ));
        }
        for (t, i, e) in [
            (
                "First $1. Rest $2.",
                "\"alpha beta\" gamma delta",
                "First alpha beta. Rest gamma delta.",
            ),
            (
                "Run awk '{print $0}' on $1.",
                "app.log",
                "Run awk '{print $0}' on app.log.",
            ),
            (
                "Run awk '{print $0}'.",
                "app.log",
                "Run awk '{print $0}'.\n\napp.log",
            ),
            (
                "First $1. Rest $2.",
                "\"$ARGUMENTS\" tail",
                "First $ARGUMENTS. Rest tail.",
            ),
            ("$ARGUMENTS / $ARGUMENTS", "$& $$", "$& $$ / $& $$"),
        ] {
            cases.push((t, i.into(), e.into()));
        }
        for (template, input, expected) in cases {
            assert_eq!(
                expand(template, &input),
                expected,
                "{template:?} with {input:?}"
            );
        }
    }

    #[test]
    fn oc_command_appends_input_to_a_file_command_without_placeholders() {
        let c = from_markdown("---\ndescription: File review\nagent: reviewer\n---\nReview files");
        assert_eq!(c.description.as_deref(), Some("File review"));
        assert_eq!(expand(&c.template, "latest"), "Review files\n\nlatest");
        assert_eq!(expand("Write docs", "details"), "Write docs\n\ndetails");
        assert_eq!(
            from_markdown("").template,
            "",
            "an empty file is an empty command"
        );
        assert_eq!(
            expand("Use $10 and $1", "a b c d e f g h i j k"),
            "Use j k and a"
        );
    }

    #[test]
    fn arguments() {
        assert_eq!(
            parse_args("a \"b c\" 'd e' f"),
            vec!["a", "b c", "d e", "f"]
        );
        assert!(parse_args("   ").is_empty());
    }

    #[test]
    fn invocations() {
        assert_eq!(
            parse_invocation("/review HEAD~1"),
            Some(("review".into(), "HEAD~1".into()))
        );
        assert_eq!(
            parse_invocation("  /init"),
            Some(("init".into(), "".into()))
        );
        assert_eq!(
            parse_invocation("/usr/bin is a path"),
            None,
            "a path is not a command name"
        );
        assert_eq!(parse_invocation("hello /init"), None);
        assert_eq!(parse_invocation("/"), None);
    }

    #[test]
    fn markdown_commands_and_precedence() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("commands");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("test.md"),
            "---\ndescription: Run tests\nagent: build\n---\nRun the tests for $ARGUMENTS",
        )
        .unwrap();
        std::fs::write(dir.join("review.md"), "My own review").unwrap();
        std::fs::write(dir.join("notes.txt"), "ignored").unwrap();
        let mut configured = BTreeMap::new();
        configured.insert(
            "test".to_string(),
            Command {
                template: "configured wins".into(),
                ..Default::default()
            },
        );
        let all = discover(builtins(Path::new("/p")), &[dir], &configured);
        assert_eq!(all["test"].template, "configured wins");
        assert_eq!(
            all["review"].template, "My own review",
            "files override built-ins"
        );
        assert!(
            !all["review"].native,
            "a custom review command replaces the native one"
        );
        assert!(all["calibrate"].native);
        assert!(all["init"].template.contains("/p"));
        assert!(!all.contains_key("notes"));
        let c = from_markdown("---\ndescription: Run tests\nagent: build\n---\nRun $1");
        assert_eq!(
            c,
            Command {
                template: "Run $1".into(),
                description: Some("Run tests".into()),
                agent: Some("build".into()),
                model: None,
                subagent: None,
                subtask: None,
                native: false,
                mcp: None,
                plugin: None,
            }
        );
    }

    #[test]
    fn builtin_prompts_are_rebranded() {
        for (_, c) in builtins(Path::new("/p")) {
            assert!(!c.template.to_lowercase().contains("opencode"));
        }
        let b = builtins(Path::new("/p"));
        assert!(b["review"].native && b["calibrate"].native && !b["init"].native);
    }
}
