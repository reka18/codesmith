//! Skills: folders with a `SKILL.md` (a `name`/`description` header plus
//! instructions) that the model loads on demand with the `skill` tool.
//! Ported from OpenCode, local skills only (no remote skill indexes).

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

/// Folders that may contain `<id>/SKILL.md`, lowest precedence first.
pub fn roots(
    cwd: &Path,
    root: &Path,
    config_dir: &Path,
    home: &Path,
    extra: &[String],
) -> Vec<PathBuf> {
    let mut out = vec![config_dir.join("skills"), home.join(".agents/skills")];
    if claude_compat() {
        out.push(home.join(".claude/skills"));
    }
    let mut dirs: Vec<&Path> = cwd
        .ancestors()
        .take_while(|d| d.starts_with(root))
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
        let p = PathBuf::from(e);
        out.push(if p.is_absolute() { p } else { root.join(p) });
    }
    out
}

/// Load every skill under `roots`; later roots override earlier ones by id.
pub fn discover(roots: &[PathBuf]) -> Vec<Skill> {
    let mut by_id: BTreeMap<String, Skill> = BTreeMap::new();
    for r in roots {
        let Ok(entries) = std::fs::read_dir(r) else {
            continue;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let id = e.file_name().to_string_lossy().into_owned();
            let file = e.path().join("SKILL.md");
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
    let mut lines = vec![
        "Skills provide specialized instructions and workflows for specific tasks.".to_string(),
        "Use the skill tool to load a skill when a task matches its description.".into(),
    ];
    if skills.is_empty() {
        lines.push("No skills are currently available.".into());
    } else {
        lines.push("<available_skills>".into());
        for s in skills {
            lines.push(format!("  <skill>\n    <id>{}</id>\n    <name>{}</name>\n    <description>{}</description>\n  </skill>", s.id, s.name, s.description));
        }
        lines.push("</available_skills>".into());
    }
    lines.join("\n")
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
}
