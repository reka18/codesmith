//! System prompt assembly. The base prompts are ported from OpenCode.

use std::path::Path;

const SYSTEM: &str = include_str!("prompt/system.txt");
const GPT: &str = include_str!("prompt/gpt.txt");
const ANTHROPIC: &str = include_str!("prompt/anthropic.txt");

/// Fill `${TOOL_GUIDANCE}` with advice for the tools actually available.
pub fn render(template: &str, tools: &[&str]) -> String {
    let mut lines: Vec<&str> = Vec::new();
    if tools.contains(&"shell") {
        lines.push("- Prefer dedicated tools over shell commands; fall back to the shell when a tool cannot do what you need.");
        lines.push("- Do not chain shell commands with separators like `echo \"====\";` or `printf '---'`; the output becomes noisy in a way that makes the user's side of the conversation worse.");
    }
    if tools.contains(&"write") {
        lines.push("- Use the write tool to create files or completely replace their content. Prefer using the edit tool for targeted changes.");
    }
    if tools.contains(&"edit") {
        lines.push("- Use the edit tool for targeted changes to existing text files. It replaces the exact text in `oldString` with `newString`, and the values must differ. By default, `oldString` must occur exactly once. If it occurs multiple times, include more surrounding context to make it unique or set `replaceAll` to true to replace every occurrence.");
    }
    template.replace("${TOOL_GUIDANCE}", &lines.join("\n"))
}

/// The base prompt for a model: GPT models get their own; Claude models get
/// the default plus a short appendix; everything else gets the default.
pub fn base(model_id: &str, tools: &[&str]) -> String {
    let id = model_id.to_lowercase();
    if id.contains("gpt") {
        return render(GPT, tools);
    }
    let base = render(SYSTEM, tools);
    if id.contains("claude") {
        format!("{base}\n\n{}", ANTHROPIC.trim_end())
    } else {
        base
    }
}

pub struct Environment<'a> {
    pub cwd: &'a Path,
    pub root: &'a Path,
    pub is_git: bool,
    pub tmp: &'a Path,
    pub date: String,
}

/// The `<env>` block and date (OpenCode's built-in instructions).
pub fn environment(e: &Environment) -> String {
    [
        format!("Today's date: {}", e.date),
        String::new(),
        "Here is some useful information about the environment you are running in:".into(),
        "<env>".into(),
        format!("  Working directory: {}", e.cwd.display()),
        format!("  Workspace root folder: {}", e.root.display()),
        format!("  Is directory a git repo: {}", if e.is_git { "yes" } else { "no" }),
        format!("  Platform: {}", std::env::consts::OS),
        format!("  Prefer {} over generic system temporary directories such as /tmp; it is pre-created and approved for external access.", e.tmp.display()),
        "</env>".into(),
    ]
    .join("\n")
}

/// Today's date in OpenCode's `Date.toDateString()` form, e.g. "Thu Nov 19 2026".
pub fn today() -> String {
    chrono::Local::now().format("%a %b %d %Y").to_string()
}

/// Full system prompt: an agent's own prompt replaces the base one.
pub fn assemble(
    agent_system: Option<&str>,
    model_id: &str,
    tools: &[&str],
    env: &str,
    instructions: &str,
) -> String {
    let mut parts = vec![match agent_system {
        Some(s) => s.to_string(),
        None => base(model_id, tools),
    }];
    parts.push(env.to_string());
    if !instructions.is_empty() {
        parts.push(instructions.to_string());
    }
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guidance_depends_on_tools() {
        let all = render(SYSTEM, &["shell", "write", "edit"]);
        assert!(
            all.contains("Prefer dedicated tools")
                && all.contains("write tool")
                && all.contains("edit tool")
        );
        assert!(!all.contains("${TOOL_GUIDANCE}"));
        let none = render(SYSTEM, &["read"]);
        assert!(!none.contains("edit tool") && !none.contains("${TOOL_GUIDANCE}"));
    }

    #[test]
    fn model_specific_bases() {
        assert!(base("gpt-5", &[]).contains("## Intermediate Commentary"));
        assert!(base("claude-sonnet", &[]).contains("# Code comments"));
        let q = base("Qwen3.5-122B", &[]);
        assert!(q.starts_with("You are an AI agent running in CodeSmith"));
        assert!(!q.contains("# Code comments") && !q.contains("Intermediate Commentary"));
    }

    #[test]
    fn no_upstream_branding() {
        for p in [SYSTEM, GPT, ANTHROPIC] {
            assert!(!p.to_lowercase().contains("opencode"));
        }
    }

    #[test]
    fn environment_block() {
        let e = environment(&Environment {
            cwd: Path::new("/w/p/src"),
            root: Path::new("/w/p"),
            is_git: true,
            tmp: Path::new("/t"),
            date: "Thu Nov 19 2026".into(),
        });
        assert!(e.starts_with("Today's date: Thu Nov 19 2026\n"));
        assert!(e.contains("<env>\n  Working directory: /w/p/src\n  Workspace root folder: /w/p\n  Is directory a git repo: yes"));
        assert!(e.ends_with("</env>"));
    }

    #[test]
    fn agent_prompt_replaces_base() {
        let s = assemble(Some("You only search."), "gpt-5", &["read"], "ENV", "");
        assert_eq!(s, "You only search.\n\nENV");
        let s = assemble(None, "m", &[], "ENV", "INSTR");
        assert!(s.ends_with("ENV\n\nINSTR"));
    }

    #[test]
    fn date_format() {
        let d = today();
        assert_eq!(d.split(' ').count(), 4, "{d}");
    }
}
