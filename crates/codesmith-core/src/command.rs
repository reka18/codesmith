//! Custom commands: prompt templates run as `/name args`. Defined in config
//! (`commands`) or as markdown files in `commands/` folders, with an
//! optional `description`/`agent`/`model` header. `$ARGUMENTS` is the whole
//! argument string and `$1`, `$2`… the individual arguments (the last one
//! used takes the rest); with neither, the arguments are appended. Ported
//! from OpenCode and pinned to its template rules by the `oc_command_*`
//! tests (from `packages/core/test/config/command.test.ts`).
//!
//! Deliberate differences: no `` !`cmd` `` shell interpolation, command files
//! are read from the top of each `commands/` folder only (no `nested/name`
//! commands, since `/a/b` reads as a path), commands defined in config
//! override command files wherever they sit, and there is no `subtask` mode.

use crate::skill::frontmatter;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const INITIALIZE: &str = include_str!("prompt/initialize.txt");

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Command {
    pub template: String,
    pub description: Option<String>,
    pub agent: Option<String>,
    pub model: Option<String>,
    /// Handled by CodeSmith itself rather than expanded into a prompt
    /// (`/review`, `/calibrate`). A same-named custom command replaces it.
    #[serde(skip)]
    pub native: bool,
    /// An MCP server's prompt, `(server, prompt)`, fetched with `prompts/get`
    /// when run (`/server:prompt args`).
    #[serde(skip)]
    pub mcp: Option<(String, String)>,
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

/// Folders that may contain `<name>.md` commands, lowest precedence first.
pub fn roots(cwd: &Path, root: &Path, config_dir: &Path, home: &Path) -> Vec<PathBuf> {
    let claude = std::env::var_os("CODESMITH_DISABLE_CLAUDE_CODE").is_none();
    let mut out = vec![config_dir.join("commands")];
    if claude {
        out.push(home.join(".claude/commands"));
    }
    let mut dirs: Vec<&Path> = cwd
        .ancestors()
        .take_while(|d| d.starts_with(root))
        .collect();
    dirs.reverse();
    for d in dirs {
        if claude {
            out.push(d.join(".claude/commands"));
        }
        out.push(d.join(".codesmith/commands"));
    }
    out
}

pub fn from_markdown(text: &str) -> Command {
    let (meta, body) = frontmatter(text);
    Command {
        template: body.trim().to_string(),
        description: meta.get("description").cloned(),
        agent: meta.get("agent").cloned(),
        model: meta.get("model").cloned(),
        native: false,
        mcp: None,
    }
}

/// Every command: built-ins, then markdown folders, then config (highest).
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
            if let Ok(text) = std::fs::read_to_string(&p) {
                all.insert(name, from_markdown(&text));
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
                native: false,
                mcp: None,
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
