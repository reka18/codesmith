//! `write`: create or overwrite a file. Ported from OpenCode's write tool.

use super::{str_arg, Ctx, Outcome, Output};
use crate::permission::Request;
use serde_json::{json, Value};

pub fn spec() -> (String, Value) {
    (
        "Writes a file to the local filesystem, overwriting if one exists.\n\nMissing parent directories are created automatically.\n\nUse this tool to create new files or overwrite existing files. For partial changes, use the edit tool instead.".into(),
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file to write to"},
                "content": {"type": "string", "description": "Content to write to the file"}
            },
            "required": ["path", "content"]
        }),
    )
}

pub fn run(ctx: &Ctx, input: &Value) -> Outcome {
    let path = str_arg(input, "path")?;
    let content = str_arg(input, "content")?;
    let target = ctx.resolve(path);
    if target.absolute.is_dir() {
        return Err(format!("Path is a directory, not a file: {path}"));
    }
    ctx.authorize_external(&target, false)?;
    let existed = target.absolute.exists();
    let lines = super::count_lines(content);
    ctx.ask(Request {
        action: "edit".into(),
        resources: target.resources.clone(),
        save: vec!["*".into()],
        title: format!(
            "{} {} ({lines} lines)",
            if existed { "Overwrite" } else { "Create" },
            target.resource
        ),
    })?;
    // Read only now that the change is allowed.
    ctx.changing(&target.absolute);
    if let Some(parent) = target.absolute.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Unable to write {path}: {e}"))?;
    }
    let content = &with_bom(&target.absolute, content);
    std::fs::write(&target.absolute, content)
        .map_err(|e| format!("Unable to write {path}: {e}"))?;
    let note = ctx.format(&target.absolute, content);
    let diagnostics = ctx.diagnostics(&target.absolute, true);
    Ok(Output::text(format!(
        "{} file successfully: {}{note}{diagnostics}",
        if existed { "Wrote" } else { "Created" },
        target.resource
    )))
}

/// Overwriting a file keeps its UTF-8 byte order mark, and the result has
/// at most one, as in OpenCode's `syncTextBom`.
fn with_bom(path: &std::path::Path, content: &str) -> String {
    const BOM: char = '\u{feff}';
    let had = std::fs::read(path).is_ok_and(|b| b.starts_with("\u{feff}".as_bytes()));
    let body = content.trim_start_matches(BOM);
    if had || content.starts_with(BOM) {
        format!("{BOM}{body}")
    } else {
        body.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::Env;
    use super::*;
    use crate::permission::Reply;

    #[tokio::test(flavor = "multi_thread")]
    async fn creates_with_parents_then_overwrites() {
        let env = Env::new();
        let ctx = env.ctx();
        let out = run(&ctx, &json!({"path": "a/b/c.txt", "content": "hi"})).unwrap();
        assert_eq!(out.content, "Created file successfully: a/b/c.txt");
        assert_eq!(
            std::fs::read_to_string(env.root().join("a/b/c.txt")).unwrap(),
            "hi"
        );
        let out = run(&ctx, &json!({"path": "a/b/c.txt", "content": "bye"})).unwrap();
        assert_eq!(out.content, "Wrote file successfully: a/b/c.txt");
        assert_eq!(
            std::fs::read_to_string(env.root().join("a/b/c.txt")).unwrap(),
            "bye"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refuses_directories_and_missing_args() {
        let env = Env::new();
        std::fs::create_dir(env.root().join("d")).unwrap();
        assert!(run(&env.ctx(), &json!({"path": "d", "content": ""}))
            .unwrap_err()
            .contains("is a directory"));
        assert!(run(&env.ctx(), &json!({"path": "x"}))
            .unwrap_err()
            .contains("`content`"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn declined_write_changes_nothing() {
        let env = Env::with_reply(Reply::Reject(Some("not now".into())));
        let e = run(&env.ctx(), &json!({"path": "x.txt", "content": "x"})).unwrap_err();
        assert!(e.contains("not now"));
        assert!(!env.root().join("x.txt").exists());
    }
}
