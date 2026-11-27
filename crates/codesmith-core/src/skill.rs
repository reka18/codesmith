//! Skills: folders with a `SKILL.md` (a `name`/`description` header plus
//! instructions) that the model loads on demand with the `skill` tool.
//! Ported from OpenCode, with skills downloaded from catalog URLs (see
//! [`crate::skill_remote`]).
//!
//! The `oc_skill_*` tests port OpenCode's frontmatter cases
//! (`packages/core/test/config/markdown.test.ts`, `config/skill.test.ts`).
//! Deliberate differences: frontmatter is read as flat `key: value` lines
//! (so an unquoted colon or a leading `[tag]` in a value just works, and
//! lists stay text), a skill without a `description` is skipped since the
//! model picks skills by it, skills are only `<id>/SKILL.md` folders (no
//! single-file `<id>.md`), and `disable-model-invocation` is not supported.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub content: String,
}

/// The description of the built-in `codesmith` skill.
pub const CODESMITH_DESCRIPTION: &str = "Use this skill for any question about CodeSmith itself: how it works, installing, using or configuring it, troubleshooting it, and its agents, commands, skills, tools, permissions, MCP servers, providers, models, sessions, automations, the CLI, the terminal UI, the local server and editors over ACP.";
const REPORT_DESCRIPTION: &str = "Use when the user wants to report a CodeSmith issue or bug. Collect standard diagnostics, add user-specific reproduction context, and publish the issue with GitHub CLI.";

/// The skills shipped with CodeSmith (OpenCode's built-in `opencode` and
/// `report` skills, as `codesmith` and `report`), offered only when the
/// config's `builtin_skills` is on so every other session's prompt stays
/// as it was. Their paths are `skill://<id>/SKILL.md`: they have no folder.
/// The report skill ends with the diagnostics of this run.
pub fn builtin(version: &str) -> Vec<Skill> {
    let term = ["TERM_PROGRAM", "TERM"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|t| !t.is_empty()))
        .unwrap_or_else(|| "Unavailable: TERM is not set".into());
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "Unavailable: SHELL is not set".into());
    let report = [
        include_str!("skill_builtin/report.md").trim_end(),
        "",
        "## Runtime Diagnostics Snapshot",
        "",
        "These values were captured when the built-in report skill was loaded. Verify them before publishing.",
        "",
        &format!("- CodeSmith version: {version}"),
        &format!(
            "- OS: {} ({})",
            std::env::consts::OS,
            std::env::consts::ARCH
        ),
        &format!("- Terminal: {term}"),
        &format!("- Shell: {shell}"),
    ]
    .join("\n");
    vec![
        Skill {
            id: "codesmith".into(),
            name: "CodeSmith".into(),
            description: CODESMITH_DESCRIPTION.into(),
            path: PathBuf::from("skill://codesmith/SKILL.md"),
            content: include_str!("skill_builtin/codesmith.md").into(),
        },
        Skill {
            id: "report".into(),
            name: "Report".into(),
            description: REPORT_DESCRIPTION.into(),
            path: PathBuf::from("skill://report/SKILL.md"),
            content: report,
        },
    ]
}

/// Add the built-in skills `skills` does not already have (a skill of
/// the user's with the same id wins), keeping the list in id order.
pub fn with_builtin(mut skills: Vec<Skill>, version: &str) -> Vec<Skill> {
    for b in builtin(version) {
        if !skills.iter().any(|s| s.id == b.id) {
            skills.push(b);
        }
    }
    skills.sort_by(|a, b| a.id.cmp(&b.id));
    skills
}

/// Split `---\nkey: value\n---\nbody` into the header map and the body.
pub fn frontmatter(text: &str) -> (BTreeMap<String, String>, String) {
    let mut map = BTreeMap::new();
    let t = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = t
        .strip_prefix("---\n")
        .or_else(|| t.strip_prefix("---\r\n"))
    else {
        return (map, t.to_string());
    };
    let Some(end) = rest.find("\n---") else {
        return (map, t.to_string());
    };
    let header = &rest[..end];
    let body = rest[end + 4..]
        .trim_start_matches(['-'])
        .trim_start_matches(['\r', '\n']);
    let mut last: Option<String> = None;
    for line in header.lines() {
        if let Some((k, v)) = line
            .split_once(':')
            .filter(|(k, _)| !k.starts_with(' ') && !k.trim().is_empty())
        {
            let v = v.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
            map.insert(k.trim().to_string(), v);
            last = Some(k.trim().to_string());
        } else if let (Some(k), true) = (&last, line.starts_with(' ')) {
            // folded continuation lines
            let e = map.entry(k.clone()).or_default();
            if !e.is_empty() && !matches!(e.as_str(), "|" | ">") {
                e.push(' ');
            } else {
                e.clear();
            }
            e.push_str(line.trim());
        }
    }
    (map, body.to_string())
}

pub fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !id.starts_with('.')
}

fn claude_compat() -> bool {
    std::env::var_os("CODESMITH_DISABLE_CLAUDE_CODE_SKILLS").is_none()
        && std::env::var_os("CODESMITH_DISABLE_CLAUDE_CODE").is_none()
}

/// The permission rules that come with the built-in skills: filing or
/// commenting on a GitHub issue with `gh` always asks, so the `report`
/// skill's "after the user confirms" does not rest on the model. They are
/// built-in rules ([`Rule::into_builtin`](crate::permission::Rule::into_builtin)):
/// they only restrict, so a configured allow cannot lift them, while a
/// configured deny still wins.
pub fn builtin_rules() -> Vec<crate::permission::Rule> {
    use crate::permission::{Effect, Rule};
    ["issue create", "issue comment"]
        .iter()
        .flat_map(|cmd| [format!("gh {cmd}*"), format!("gh * {cmd}*")])
        .map(|pattern| Rule::new("shell", &pattern, Effect::Ask).into_builtin())
        .collect()
}

/// The skills a session in `cwd` has: those under [`roots`] (from the
/// folder, its project, the config folder and home), and the built-in ones
/// when `builtin_skills` is on.
pub fn for_folder(cwd: &Path, loaded: &crate::config::Loaded) -> Vec<Skill> {
    let config = &loaded.config;
    let home = dirs::home_dir().unwrap_or_default();
    let skills = discover(&roots(
        cwd,
        &loaded.root,
        &crate::paths::config_dir(),
        &home,
        &config.skills,
    ));
    if config.builtin_skills {
        return with_builtin(skills, env!("CARGO_PKG_VERSION"));
    }
    skills
}

/// Folders that may contain `<id>/SKILL.md`, lowest precedence first.
pub fn roots(
    cwd: &Path,
    root: &Path,
    config_dir: &Path,
    home: &Path,
    extra: &[String],
) -> Vec<PathBuf> {
    let global = !crate::config::global_disabled();
    let mut out = Vec::new();
    if global {
        out.extend([config_dir.join("skills"), home.join(".agents/skills")]);
        if claude_compat() {
            out.push(home.join(".claude/skills"));
        }
    }
    // The home folder's `.agents` and `.claude` are global however the
    // walk reaches them. `$CODESMITH_DISABLE_PROJECT_CONFIG` turns the
    // repository's own skills off.
    let mut dirs: Vec<&Path> = cwd
        .ancestors()
        .take_while(|d| d.starts_with(root))
        .filter(|d| global || !crate::config::global_source(d, config_dir, home))
        .filter(|_| !crate::config::project_disabled())
        .collect();
    dirs.reverse();
    for d in dirs {
        out.push(d.join(".agents/skills"));
        if claude_compat() {
            out.push(d.join(".claude/skills"));
        }
        out.push(d.join(".codesmith/skills"));
    }
    for e in extra {
        if crate::skill_remote::is_url(e) {
            // A catalog URL: its downloaded skill folders.
            out.extend(crate::skill_remote::pulled(e));
            continue;
        }
        // `~/x` is in the home folder, as in OpenCode; other relative paths
        // are the project's.
        let p = match e.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None if e == "~" => home.to_path_buf(),
            None => PathBuf::from(e),
        };
        out.push(if p.is_absolute() { p } else { root.join(p) });
    }
    out
}

/// The skills a session working in `cwd` finds.
pub fn find(cwd: &Path, root: &Path, extra: &[String]) -> Vec<Skill> {
    let home = dirs::home_dir().unwrap_or_default();
    discover(&roots(cwd, root, &crate::paths::config_dir(), &home, extra))
}

/// Load every skill under `roots`; later roots override earlier ones by id.
pub fn discover(roots: &[PathBuf]) -> Vec<Skill> {
    let mut by_id: BTreeMap<String, Skill> = BTreeMap::new();
    for r in roots {
        // A root with its own `SKILL.md` is one skill (a downloaded one);
        // otherwise each `<id>/SKILL.md` below it is.
        let dirs: Vec<PathBuf> = if r.join("SKILL.md").is_file() {
            vec![r.clone()]
        } else {
            let Ok(entries) = std::fs::read_dir(r) else {
                continue;
            };
            let mut entries: Vec<_> = entries.flatten().collect();
            entries.sort_by_key(|e| e.file_name());
            entries.into_iter().map(|e| e.path()).collect()
        };
        for dir in dirs {
            let Some(id) = dir.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                continue;
            };
            let file = dir.join("SKILL.md");
            if !is_safe_id(&id) || !file.is_file() {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let (meta, body) = frontmatter(&text);
            let description = meta.get("description").cloned().unwrap_or_default();
            if description.is_empty() {
                continue;
            }
            by_id.insert(
                id.clone(),
                Skill {
                    name: meta
                        .get("name")
                        .cloned()
                        .filter(|n| !n.is_empty())
                        .unwrap_or(id.clone()),
                    id,
                    description,
                    path: file,
                    content: body,
                },
            );
        }
    }
    by_id.into_values().collect()
}

/// The system-prompt section listing available skills.
pub fn instructions(skills: &[Skill]) -> String {
    render_summaries(&summaries(skills))
}

/// What the skill list says about one skill.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Summary {
    pub id: String,
    pub name: String,
    pub description: String,
}

fn summaries(skills: &[Skill]) -> Vec<Summary> {
    let mut out: Vec<Summary> = skills
        .iter()
        .map(|s| Summary {
            id: s.id.clone(),
            name: s.name.clone(),
            description: s.description.clone(),
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

fn entries(skills: &[Summary]) -> Vec<String> {
    skills
        .iter()
        .flat_map(|s| {
            [
                "  <skill>".to_string(),
                format!("    <id>{}</id>", s.id),
                format!("    <name>{}</name>", s.name),
                format!("    <description>{}</description>", s.description),
                "  </skill>".to_string(),
            ]
        })
        .collect()
}

fn render_summaries(skills: &[Summary]) -> String {
    let mut lines = vec![
        "Skills provide specialized instructions and workflows for specific tasks.".to_string(),
        "Use the skill tool to load a skill when a task matches its description.".into(),
    ];
    if skills.is_empty() {
        lines.push("No skills are currently available.".into());
    } else {
        lines.push("<available_skills>".into());
        lines.extend(entries(skills));
        lines.push("</available_skills>".into());
    }
    lines.join("\n")
}

const UPDATE: crate::instruction_state::ListUpdate<Summary> = crate::instruction_state::ListUpdate {
    key: |s| s.id.clone(),
    changed: |a, b| a.name != b.name || a.description != b.description,
    entries,
    render: render_summaries,
    replaced: "The available skills have changed. This list supersedes the previous available skills list.",
    added: "New skills are available in addition to those previously listed:",
    changed_text: "The following skills have changed. Each entry replaces the previously listed skill with the same ID:",
    removed: |ids| format!("The following skill IDs are no longer available and must not be used: {ids}."),
};

pub const KEY: &str = "core/skill-guidance";

/// The skill list as an instruction source: none is a removal, and an
/// update names only the skills that came, went or changed.
pub fn source(skills: &[Skill]) -> crate::instruction_state::List {
    use crate::instruction_state::{make, Definition, Observed};
    let list = summaries(skills);
    make::<Vec<Summary>>(Definition {
        key: KEY.into(),
        read: Box::new(move || {
            if list.is_empty() {
                Observed::Removed
            } else {
                Observed::Value(list.clone())
            }
        }),
        initial: Box::new(|s| render_summaries(s)),
        changed: Box::new(|a, b| Some(UPDATE.render(a, b))),
        removed: Some(Box::new(|_| {
            "Skill guidance is no longer available. Do not use any previously listed skill.".into()
        })),
    })
}

/// Up to `max` files in the skill's folder besides SKILL.md, relative to it.
pub fn files(skill: &Skill, max: usize) -> Vec<String> {
    let Some(dir) = skill.path.parent() else {
        return vec![];
    };
    let mut out: Vec<String> = ignore::WalkBuilder::new(dir)
        .max_depth(Some(4))
        .build()
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter_map(|e| {
            e.path()
                .strip_prefix(dir)
                .ok()
                .map(|p| p.display().to_string())
        })
        .filter(|p| p != "SKILL.md")
        .collect();
    out.sort();
    out.truncate(max);
    out
}

/// What the `skill` tool returns (OpenCode's `toModelOutput`).
pub fn content(skill: &Skill) -> String {
    let dir = skill
        .path
        .parent()
        .map(|d| d.display().to_string())
        .unwrap_or_default();
    let mut lines = vec![
        format!("<skill_content name=\"{}\">", skill.name),
        format!("# Skill: {}", skill.name),
        String::new(),
        skill.content.trim().to_string(),
        String::new(),
        format!("Base directory for this skill: {dir}"),
        "Relative paths in this skill (e.g., scripts/, reference/) are relative to this base directory.".into(),
        "Note: file list is sampled.".into(),
        String::new(),
        "<skill_files>".into(),
    ];
    lines.extend(
        files(skill, 50)
            .into_iter()
            .map(|f| format!("<file>{f}</file>")),
    );
    lines.push("</skill_files>".into());
    lines.push("</skill_content>".into());
    lines.join("\n")
}

pub fn tool_spec(skills: &[Skill]) -> (String, serde_json::Value) {
    let ids: Vec<&str> = skills.iter().map(|s| s.id.as_str()).collect();
    (
        "Load a specialized skill's instructions and resources into the current conversation when the task at hand matches its description.\n\nThe skill ID must match an available skill or a skill explicitly referenced by the user.".into(),
        serde_json::json!({
            "type": "object",
            "properties": {"id": {"type": "string", "enum": ids, "description": "The ID of an available skill or a skill explicitly referenced by the user"}},
            "required": ["id"]
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_parsing() {
        let (m, b) = frontmatter(
            "---\nname: PDF tools\ndescription: \"Work with PDFs\"\n---\n# Body\ntext\n",
        );
        assert_eq!(m["name"], "PDF tools");
        assert_eq!(m["description"], "Work with PDFs");
        assert_eq!(b, "# Body\ntext\n");
        let (m, b) = frontmatter("no header");
        assert!(m.is_empty() && b == "no header");
        let (m, _) = frontmatter("---\ndescription: >\n  folded long\n  text\n---\nx");
        assert_eq!(m["description"], "folded long text");
    }

    #[test]
    fn ids() {
        assert!(is_safe_id("pdf-tools_2"));
        assert!(
            !is_safe_id("../x") && !is_safe_id(".hidden") && !is_safe_id("a b") && !is_safe_id("")
        );
    }

    fn write(dir: &Path, id: &str, text: &str) {
        std::fs::create_dir_all(dir.join(id)).unwrap();
        std::fs::write(dir.join(id).join("SKILL.md"), text).unwrap();
    }

    #[test]
    fn discovery_and_override() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("a"), d.path().join("b"));
        write(&a, "pdf", "---\ndescription: old\n---\nold body");
        write(
            &a,
            "docs",
            "---\nname: Docs\ndescription: Write docs\n---\nbody",
        );
        write(&a, "nodesc", "---\nname: x\n---\nbody");
        write(&b, "pdf", "---\ndescription: new\n---\nnew body");
        std::fs::write(b.join("pdf/helper.py"), "").unwrap();
        let skills = discover(&[a, b, d.path().join("missing")]);
        let ids: Vec<&str> = skills.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            ["docs", "pdf"],
            "skills without a description are skipped"
        );
        let pdf = skills.iter().find(|s| s.id == "pdf").unwrap();
        assert_eq!(pdf.description, "new", "later roots win");
        let c = content(pdf);
        assert!(c.starts_with("<skill_content name=\"pdf\">\n# Skill: pdf\n\nnew body"));
        assert!(c.contains("<file>helper.py</file>") && !c.contains("<file>SKILL.md</file>"));
        let i = instructions(&skills);
        assert!(i.contains(
            "<id>docs</id>\n    <name>Docs</name>\n    <description>Write docs</description>"
        ));
        assert!(instructions(&[]).ends_with("No skills are currently available."));
    }

    #[test]
    fn roots_order() {
        let r = roots(
            Path::new("/r/a"),
            Path::new("/r"),
            Path::new("/cfg"),
            Path::new("/home"),
            &["extra".into()],
        );
        assert_eq!(r.first().unwrap(), Path::new("/cfg/skills"));
        assert_eq!(r.last().unwrap(), Path::new("/r/extra"));
        assert!(
            r.iter()
                .position(|p| p == Path::new("/r/.codesmith/skills"))
                < r.iter()
                    .position(|p| p == Path::new("/r/a/.codesmith/skills"))
        );
    }

    #[test]
    fn oc_skill_frontmatter_values_with_colons_and_brackets() {
        let (m, b) = frontmatter(
            "---\ndescription: Use when the user needs to crawl pages. Keywords: crawl, scrape\n---\nbody",
        );
        assert_eq!(
            m["description"],
            "Use when the user needs to crawl pages. Keywords: crawl, scrape"
        );
        assert_eq!(b.trim(), "body");
        let (m, _) =
            frontmatter("---\ndescription: [Team] Does a thing\ntools: [read, edit]\n---\nbody");
        assert_eq!(m["description"], "[Team] Does a thing");
        let (m, b) = frontmatter("just body");
        assert!(m.is_empty());
        assert_eq!(b, "just body");
    }

    #[test]
    fn oc_skill_ids_come_from_the_folder_and_name_defaults_to_it() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "manual",
            "---\nname: Manual\ndescription: Manual only\n---\n# manual",
        );
        write(d.path(), "plain", "---\ndescription: d\n---\n# plain");
        let skills = discover(&[d.path().to_path_buf()]);
        assert_eq!(
            skills
                .iter()
                .map(|s| (s.id.as_str(), s.name.as_str(), s.content.as_str()))
                .collect::<Vec<_>>(),
            [
                ("manual", "Manual", "# manual"),
                ("plain", "plain", "# plain")
            ]
        );
    }
}
