//! Editing the prompt in an external editor (`prompt.editor`, `<leader>e`,
//! or `/editor`), as OpenCode's `openEditor`: the prompt goes to a private
//! temporary file, `$VISUAL` (or `$EDITOR`, which may carry arguments, as
//! `code --wait`) opens it with the UI's terminal handed over, and what it
//! saved comes back as the prompt. The terminal is taken back whatever the
//! editor did, and at most [`READ_LIMIT`] bytes are read back.

use anyhow::{anyhow, bail, Context, Result};
use std::io::Read;
use std::path::Path;

/// The most of an edited prompt read back.
pub const READ_LIMIT: u64 = 1024 * 1024;

/// The editor command `$VISUAL` or `$EDITOR` names.
pub fn configured(env: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    ["VISUAL", "EDITOR"]
        .iter()
        .filter_map(|k| env(k))
        .find(|v| !v.trim().is_empty())
}

/// Edit `value` with `editor` in `cwd`, calling `suspend` before the editor
/// starts and `resume` once it ended (or failed to start). `None` when it
/// saved nothing.
pub fn open(
    value: &str,
    editor: Option<&str>,
    cwd: &Path,
    suspend: &mut dyn FnMut(),
    resume: &mut dyn FnMut(),
) -> Result<Option<String>> {
    let editor =
        editor.ok_or_else(|| anyhow!("Set $VISUAL or $EDITOR to open an external editor"))?;
    let mut parts = editor.split_whitespace();
    let program = parts.next().ok_or_else(|| anyhow!("$EDITOR is empty"))?;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let file = std::env::temp_dir().join(format!(
        "codesmith-prompt-{}-{}-{n}.md",
        std::process::id(),
        codesmith_core::store::now()
    ));
    codesmith_core::paths::write_private(&file, value.as_bytes())
        .with_context(|| format!("writing {}", file.display()))?;
    suspend();
    let status = std::process::Command::new(program)
        .args(parts)
        .arg(&file)
        .current_dir(if cwd.is_dir() { cwd } else { Path::new(".") })
        .status();
    resume();
    let read = || -> Result<String> {
        let mut text = String::new();
        std::fs::File::open(&file)?
            .take(READ_LIMIT)
            .read_to_string(&mut text)?;
        Ok(text)
    };
    let result = match status {
        Err(e) => Err(anyhow!("could not start the editor {program}: {e}")),
        Ok(s) if !s.success() => match s.code() {
            Some(c) => Err(anyhow!("Editor exited with code {c}")),
            None => Err(anyhow!("Editor exited with a signal")),
        },
        Ok(_) => read(),
    };
    let _ = std::fs::remove_file(&file);
    let text = result?;
    if text.len() as u64 >= READ_LIMIT {
        bail!("the edited prompt is over {} KiB", READ_LIMIT / 1024);
    }
    Ok((!text.is_empty()).then_some(text))
}

/// The editor's text as a prompt: the one trailing newline editors add
/// to a single line is dropped; a longer prompt is kept as it is.
pub fn normalize(content: &str) -> &str {
    for end in ["\r\n", "\n"] {
        if let Some(body) = content.strip_suffix(end) {
            return if body.contains(['\n', '\r']) {
                content
            } else {
                body
            };
        }
    }
    content
}

#[cfg(test)]
mod tests {
    use super::*;

    // editor.test.ts: "rejects when the external editor cannot start"
    #[test]
    fn oc_rejects_when_the_external_editor_cannot_start() {
        let (mut s, mut r) = (|| {}, || {});
        let got = open(
            "original",
            Some("codesmith-editor-that-does-not-exist"),
            Path::new("."),
            &mut s,
            &mut r,
        );
        assert!(got.is_err());
    }

    // editor.test.ts: "returns the edited prompt and resumes the renderer"
    #[cfg(unix)]
    #[test]
    fn oc_returns_the_edited_prompt_and_resumes_the_renderer() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("editor.sh");
        std::fs::write(&script, "#!/bin/sh\nprintf edited >> \"$1\"\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let events = std::cell::RefCell::new(Vec::new());
        let got = open(
            "original ",
            Some(script.to_str().unwrap()),
            dir.path(),
            &mut || events.borrow_mut().push("suspend"),
            &mut || events.borrow_mut().push("resume"),
        )
        .unwrap();
        assert_eq!(got.as_deref(), Some("original edited"));
        assert_eq!(*events.borrow(), ["suspend", "resume"]);
    }

    // editor.test.ts: "says which variable to set when no external editor is configured"
    #[test]
    fn oc_says_which_variable_to_set_when_no_editor_is_configured() {
        assert_eq!(configured(&|_| None), None);
        let mut suspended = false;
        let err = open(
            "original",
            None,
            Path::new("."),
            &mut || suspended = true,
            &mut || {},
        )
        .unwrap_err();
        assert!(err.to_string().contains("$EDITOR"), "{err}");
        assert!(!suspended);
        let env = |k: &str| (k == "EDITOR").then(|| "vi".to_string());
        assert_eq!(configured(&env).as_deref(), Some("vi"));
    }

    // editor.test.ts: "normalizes a single trailing editor newline for one-line prompts"
    #[test]
    fn oc_normalizes_a_single_trailing_editor_newline_for_one_line_prompts() {
        assert_eq!(normalize("hello\n"), "hello");
        assert_eq!(normalize("hello\r\n"), "hello");
    }

    // editor.test.ts: "preserves multiline prompts that end with a newline"
    #[test]
    fn oc_preserves_multiline_prompts_that_end_with_a_newline() {
        assert_eq!(normalize("hello\nworld\n"), "hello\nworld\n");
    }
}
