//! Port of OpenCode's `packages/core/test/util/which.test.ts` (MIT):
//! `format::which`, the PATH lookup formatters use. One test, as it sets
//! the process's PATH (this file is a test binary of its own).
#![cfg(unix)]

use codesmith_core::format::which;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn cmd(dir: &Path, name: &str, executable: bool) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let p = dir.join(name);
    std::fs::write(&p, "#!/bin/sh\n").unwrap();
    let mode = if executable { 0o755 } else { 0o644 };
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
    p
}

/// "returns null when command is missing", "finds a command from PATH
/// override", "uses first PATH match" and "returns null for
/// non-executable file on unix".
#[test]
fn oc_which_looks_commands_up_on_path() {
    let saved = std::env::var_os("PATH");
    let tmp = tempfile::tempdir().unwrap();
    let (bin, a, b) = (
        tmp.path().join("bin"),
        tmp.path().join("a"),
        tmp.path().join("b"),
    );
    let tool = cmd(&bin, "tool", true);
    let first = cmd(&a, "dupe", true);
    cmd(&b, "dupe", true);
    cmd(&bin, "noexec", false);
    let result = std::panic::catch_unwind(|| {
        assert_eq!(which("codesmith-missing-command-for-test"), None);
        std::env::set_var("PATH", &bin);
        assert_eq!(which("tool"), Some(tool.clone()));
        assert_eq!(which("noexec"), None);
        std::env::set_var("PATH", std::env::join_paths([&a, &b]).unwrap());
        assert_eq!(which("dupe"), Some(first.clone()));
    });
    match saved {
        Some(p) => std::env::set_var("PATH", p),
        None => std::env::remove_var("PATH"),
    }
    result.unwrap();
}
