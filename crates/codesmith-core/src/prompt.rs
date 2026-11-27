//! System prompt assembly. The base prompts are ported from OpenCode.

use std::path::Path;

const SYSTEM: &str = include_str!("prompt/system.txt");
const GPT: &str = include_str!("prompt/gpt.txt");
const ANTHROPIC: &str = include_str!("prompt/anthropic.txt");
const ASTRA: &str = include_str!("prompt/gpt-astra.txt");
const KIMI: &str = include_str!("prompt/kimi.txt");
const TRINITY: &str = include_str!("prompt/trinity.txt");
const META: &str = include_str!("prompt/meta.txt");
const WORKTREES: &str = include_str!("prompt/worktrees.txt");

/// Guidance on separate checkouts (nested git worktrees), for every base
/// prompt whose agent has the shell.
pub fn checkouts(tools: &[&str]) -> Option<&'static str> {
    tools.contains(&"shell").then(|| WORKTREES.trim_end())
}

/// `base` followed by the guidance for `tools` that every base prompt gets.
fn with_guidance(base: String, tools: &[&str]) -> String {
    match checkouts(tools) {
        Some(g) => format!("{}\n\n{g}", base.trim_end()),
        None => base,
    }
}

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

/// The base prompt for a model by its id (see [`base_named`]).
pub fn base(model_id: &str, tools: &[&str]) -> String {
    base_named(model_id, model_id, tools)
}

/// The base prompt for model `id` (the id in config or the catalog, not
/// the API model name it may stand for), with the tool guidance for
/// `tools`: OpenCode's model-lab prompts. GPT models get the GPT prompt
/// (`astra` ones their own), Kimi, Arcee Trinity and Meta Muse models get
/// theirs (Muse's names the model by `name`, its catalog name), Claude
/// models get the default plus a short appendix, and everything else gets
/// the default. An agent's own prompt replaces all of these.
pub fn base_named(id: &str, name: &str, tools: &[&str]) -> String {
    with_guidance(lab_prompt(id, name, tools), tools)
}

fn lab_prompt(id: &str, name: &str, tools: &[&str]) -> String {
    let id = id.to_lowercase();
    if id.contains("gpt") {
        return render(if id.contains("astra") { ASTRA } else { GPT }, tools);
    }
    if id.contains("kimi") {
        return render(KIMI, tools);
    }
    if id.contains("trinity") {
        return render(TRINITY, tools);
    }
    if id.contains("muse") {
        return render(&META.replace("{{MODEL_NAME}}", name), tools);
    }
    let base = render(SYSTEM, tools);
    if id.contains("claude") {
        format!("{base}\n\n{}", render(ANTHROPIC, tools).trim_end())
    } else {
        base
    }
}

/// The display name of `provider/model`: its configured `name`, else the
/// catalog's, else its id.
pub fn model_name(config: &crate::config::Config, spec: &str) -> String {
    let base = crate::config::split_variant(spec).0;
    let Some((p, m)) = base.split_once('/') else {
        return base.to_string();
    };
    let configured = config.provider(p).and_then(|c| c.models.get(m).cloned());
    if let Some(name) = configured.as_ref().and_then(|c| c.name.clone()) {
        return name;
    }
    let catalog = config.models_dev.as_ref().map(|c| c.catalog());
    catalog
        .as_ref()
        .and_then(|c| {
            configured
                .as_ref()
                .and_then(|c| c.id.as_deref())
                .and_then(|id| c.model(p, id))
                .or_else(|| c.model(p, m))
                .map(|cm| cm.name.clone())
        })
        .unwrap_or_else(|| m.to_string())
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

/// The `<env>` block alone.
pub fn env_block(e: &Environment) -> String {
    [
        "<env>".to_string(),
        format!("  Working directory: {}", e.cwd.display()),
        format!("  Workspace root folder: {}", e.root.display()),
        format!("  Is directory a git repo: {}", if e.is_git { "yes" } else { "no" }),
        format!("  Platform: {}", std::env::consts::OS),
        format!("  Prefer {} over generic system temporary directories such as /tmp; it is pre-created and approved for external access.", e.tmp.display()),
        "</env>".into(),
    ]
    .join("\n")
}

/// The key of the date's instruction source. On a local server its
/// baseline text is not part of the system prompt: it is sent as a system
/// update before the history (see `Session::date_note`), so the system
/// prompt and the tool definitions after it in many chat templates (Qwen's
/// among them) stay byte-identical from one day to the next.
pub const DATE_KEY: &str = "core/date";

/// The date and environment as instruction sources (OpenCode's built-in
/// instructions): `date` is read at each observation, so a session that
/// runs past midnight is told the new date, once a day.
pub fn builtin_sources(
    date: impl Fn() -> String + Send + Sync + 'static,
    env: String,
) -> crate::instruction_state::List {
    use crate::instruction_state::{make, Definition, Observed};
    let mut list = make::<String>(Definition {
        key: DATE_KEY.into(),
        read: Box::new(move || Observed::Value(date())),
        initial: Box::new(|d| format!("Today's date: {d}")),
        changed: Box::new(|_, d| Some(format!("Today's date is now: {d}"))),
        removed: None,
    });
    list.extend(make::<String>(Definition {
        key: "core/environment".into(),
        read: Box::new(move || Observed::Value(env.clone())),
        initial: Box::new(|e| {
            format!(
                "Here is some useful information about the environment you are running in:\n{e}"
            )
        }),
        changed: Box::new(|_, e| Some(format!("The environment you are running in is now:\n{e}"))),
        removed: None,
    }));
    list
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
        for p in [SYSTEM, GPT, ANTHROPIC, WORKTREES] {
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
    fn every_base_prompt_with_the_shell_says_where_worktrees_go() {
        let ids = [
            "gpt-5",
            "gpt-5-astra",
            "kimi-k2",
            "trinity",
            "muse-spark",
            "claude-sonnet-4",
            "llama-3.3",
        ];
        for id in ids {
            let p = base_named(id, id, &["shell", "edit"]);
            assert!(p.contains("# Separate checkouts"), "{id}");
            assert!(
                p.contains("git worktree add <repo>/.worktrees/<name> <ref>"),
                "{id}"
            );
            assert!(
                p.contains(".git/info/exclude") && p.contains("not `.gitignore`"),
                "{id}"
            );
            assert!(
                p.contains("Never `cd` or `git -C` into folders outside the project"),
                "{id}"
            );
            assert!(
                p.contains("git worktree remove <path>") && p.contains("git worktree prune"),
                "{id}"
            );
            assert!(p.contains("worktree_create"), "{id}");
            assert_eq!(p.matches("# Separate checkouts").count(), 1, "{id}");
            // Without the shell there is nothing to run it with.
            assert!(
                !base_named(id, id, &["read"]).contains("Separate checkouts"),
                "{id}"
            );
        }
        assert!(checkouts(&["shell"]).is_some() && checkouts(&[]).is_none());
        // An agent's own prompt still replaces the base, guidance and all.
        let s = assemble(Some("You only search."), "gpt-5", &["shell"], "ENV", "");
        assert!(!s.contains("Separate checkouts"));
        assert!(assemble(None, "m", &["shell"], "ENV", "").contains("Separate checkouts"));
    }

    #[test]
    fn date_format() {
        let d = today();
        assert_eq!(d.split(' ').count(), 4, "{d}");
    }
}
