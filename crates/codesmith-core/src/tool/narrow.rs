//! Narrowing the tools of one headless run (`codesmith run --tools`,
//! `--no-tools`), for a program that runs CodeSmith on someone else's
//! behalf, such as a tailbus bridge: only the named built-in tools are
//! offered, and every other tool, MCP's, the automation tool's and
//! plugins' (the built-in `codesmith` tool among them) included, is denied
//! by a rule, which also hides it. The rules go in the config's overrides, which come
//! after every agent's and the user's own, so nothing configured can lift
//! them; they only take away. Nothing is stored with the session.

use super::{Tool, ALL};
use crate::permission::{Effect, Rule};

/// Other names a caller may use for a tool (OpenCode's and other agents').
const ALIASES: [(&str, Tool); 4] = [
    ("bash", Tool::Shell),
    ("task", Tool::Subagent),
    ("apply_patch", Tool::Patch),
    ("todo", Tool::Todowrite),
];

/// The tools a narrowed run offers, and the rules that deny the rest.
#[derive(Debug, Clone, PartialEq)]
pub struct Narrowed {
    pub tools: Vec<Tool>,
    pub rules: Vec<Rule>,
}

/// Parse a `--tools` list: names separated by commas or spaces, each a
/// built-in tool's name or alias, case ignored. An empty list is no tools.
pub fn parse(list: &str) -> Result<Vec<Tool>, String> {
    let mut out = Vec::new();
    for raw in list.split(|c: char| c == ',' || c.is_whitespace()) {
        let name = raw.trim().to_ascii_lowercase();
        if name.is_empty() {
            continue;
        }
        let tool = Tool::from_name(&name)
            .or_else(|| ALIASES.iter().find(|(a, _)| *a == name).map(|(_, t)| *t))
            .ok_or_else(|| {
                format!(
                    "unknown tool {name:?}; built-in tools are {}",
                    ALL.iter().map(|t| t.name()).collect::<Vec<_>>().join(", ")
                )
            })?;
        if !out.contains(&tool) {
            out.push(tool);
        }
    }
    Ok(out)
}

/// Offer only `allowed` (none when empty) and deny every other built-in
/// tool's action, each configured MCP server's tools and resource tools,
/// the automation tools, and every enabled plugin's tools (which a deny
/// also hides). `write`, `edit` and `patch` share the `edit`
/// action, so allowing any one of them leaves `edit` allowed; only the
/// tools named are offered.
pub fn narrow(config: &crate::config::Config, allowed: &[Tool]) -> Narrowed {
    let kept: Vec<&str> = allowed.iter().map(|t| t.action()).collect();
    let mut rules = Vec::new();
    for t in ALL {
        let action = t.action();
        if !kept.contains(&action) && !rules.iter().any(|r: &Rule| r.action == action) {
            rules.push(Rule::new(action, "*", Effect::Deny));
        }
    }
    let mcp = config
        .mcp
        .servers
        .keys()
        .map(|s| crate::mcp::tools_pattern(s))
        .chain([
            crate::mcp::LIST_RESOURCES.to_string(),
            crate::mcp::READ_RESOURCE.to_string(),
            crate::automation::TOOL.to_string(),
            crate::automation::WATCH_EVENT.to_string(),
        ])
        .chain(
            crate::plugin::active()
                .tools(config)
                .into_iter()
                .map(|t| t.name().to_string()),
        );
    rules.extend(mcp.map(|a| Rule::new(&a, "*", Effect::Deny)));
    Narrowed {
        tools: allowed.to_vec(),
        rules,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::evaluate;

    fn config_with_mcp(servers: &[&str]) -> crate::config::Config {
        let mut c = crate::config::Config::default();
        for s in servers {
            c.mcp.servers.insert(
                s.to_string(),
                serde_json::from_value(serde_json::json!({"type": "local", "command": ["x"]}))
                    .unwrap(),
            );
        }
        c
    }

    #[test]
    fn names_aliases_case_and_separators_are_accepted() {
        assert_eq!(
            parse("read, Grep  bash,,apply_patch").unwrap(),
            vec![Tool::Read, Tool::Grep, Tool::Shell, Tool::Patch]
        );
        assert_eq!(parse("read,read").unwrap(), vec![Tool::Read]);
        assert_eq!(parse("").unwrap(), vec![]);
        assert_eq!(parse(" , ").unwrap(), vec![]);
    }

    #[test]
    fn an_unknown_tool_is_refused_with_the_list() {
        let e = parse("read,rm").unwrap_err();
        assert!(e.contains("\"rm\"") && e.contains("read, write"), "{e}");
        // Not a path, a pattern or an MCP tool.
        for bad in ["*", "read*", "../read", "github_create_issue", "READ;"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn only_the_named_tools_are_left_allowed() {
        let n = narrow(&config_with_mcp(&["github"]), &[Tool::Read, Tool::Grep]);
        assert_eq!(n.tools, vec![Tool::Read, Tool::Grep]);
        let denied = |a: &str| evaluate(a, "anything", &n.rules) == Effect::Deny;
        assert!(!denied("read") && !denied("grep"));
        for a in [
            "edit",
            "shell",
            "glob",
            "webfetch",
            "websearch",
            "subagent",
            "skill",
            "lsp",
        ] {
            assert!(denied(a), "{a}");
        }
        assert!(denied("github_create_issue"), "MCP tools are denied");
        assert!(denied(crate::mcp::READ_RESOURCE));
        assert!(denied(crate::automation::TOOL));
        // Plugin tools (the `codesmith` tool) are registered by the binary;
        // e2e/run_bridge.rs checks they are hidden.
    }

    #[test]
    fn write_edit_and_patch_share_one_action() {
        let n = narrow(&Default::default(), &[Tool::Write]);
        assert_ne!(evaluate("edit", "x", &n.rules), Effect::Deny);
        assert_eq!(n.tools, vec![Tool::Write], "only write is offered");
        let n = narrow(&Default::default(), &[Tool::Read]);
        assert_eq!(evaluate("edit", "x", &n.rules), Effect::Deny);
    }

    #[test]
    fn no_tools_denies_every_tool() {
        let n = narrow(&config_with_mcp(&["a", "b"]), &[]);
        assert!(n.tools.is_empty());
        for t in ALL {
            assert_eq!(evaluate(t.action(), "x", &n.rules), Effect::Deny, "{t:?}");
        }
        assert_eq!(evaluate("a_x", "x", &n.rules), Effect::Deny);
        assert_eq!(evaluate("b_y", "x", &n.rules), Effect::Deny);
        // One rule per action, no duplicates for edit.
        let edits = n.rules.iter().filter(|r| r.action == "edit").count();
        assert_eq!(edits, 1);
    }

    #[test]
    fn narrowing_never_allows_anything() {
        // Bounded property: for every subset of the built-in tools, the
        // rules only deny.
        for mask in 0u32..(1 << ALL.len()) {
            let allowed: Vec<Tool> = ALL
                .iter()
                .enumerate()
                .filter(|(i, _)| mask & (1 << i) != 0)
                .map(|(_, t)| *t)
                .collect();
            let n = narrow(&config_with_mcp(&["srv"]), &allowed);
            assert!(n.rules.iter().all(|r| r.effect == Effect::Deny));
            for t in ALL {
                let denied = evaluate(t.action(), "r", &n.rules) == Effect::Deny;
                let kept = allowed.iter().any(|a| a.action() == t.action());
                assert_eq!(denied, !kept, "{mask:b} {t:?}");
            }
        }
    }
}
