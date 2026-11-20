//! Ports of OpenCode's search and file tool tests (`packages/core/test/`
//! `tool-search.test.ts`, `ripgrep.test.ts`, `tool-write.test.ts`,
//! `tool-read-filesystem.test.ts`, MIT), run through `tool::run` the way the
//! session runs a call. Each test names the OpenCode test it ports.
//! See `crates/codesmith-core/PARITY.md`.

use codesmith_core::config::ToolOutputConfig;
use codesmith_core::permission::{Asker, Effect, Gate, Reply, Request, Rule};
use codesmith_core::tool::{self, Ctx};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Records every permission request and answers with `reply` (or rejects
/// the actions in `deny`).
#[derive(Default)]
struct Recorder {
    asked: Mutex<Vec<Request>>,
    deny: Mutex<Vec<String>>,
}

impl Asker for Recorder {
    fn ask(&self, req: &Request) -> Reply {
        self.asked.lock().unwrap().push(req.clone());
        if self.deny.lock().unwrap().contains(&req.action) {
            Reply::Reject(None)
        } else {
            Reply::Once
        }
    }
}

impl Recorder {
    fn actions(&self) -> Vec<String> {
        self.asked
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.action.clone())
            .collect()
    }
    fn resources(&self, i: usize) -> Vec<String> {
        self.asked.lock().unwrap()[i].resources.clone()
    }
}

struct Env {
    dir: tempfile::TempDir,
    gate: Gate,
    rec: Arc<Recorder>,
}

impl Env {
    /// Reads allowed (as by default), everything else goes to the recorder.
    fn new() -> Env {
        Env::with_rules(vec![allow("read")])
    }
    fn with_rules(rules: Vec<Rule>) -> Env {
        let rec = Arc::new(Recorder::default());
        Env {
            dir: tempfile::tempdir().unwrap(),
            gate: Gate::new(rules, rec.clone()),
            rec,
        }
    }
    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }
    fn ctx(&self) -> Ctx<'_> {
        Ctx {
            cwd: self.root(),
            root: self.root(),
            gate: &self.gate,
            shell: "/bin/sh".into(),
            session_id: "test".into(),
            limits: ToolOutputConfig::default(),
            client: reqwest::Client::new(),
            jobs: None,
            websearch: None,
            format: None,
            lsp: None,
            progress: None,
        }
    }
    fn write(&self, rel: &str, content: &str) -> PathBuf {
        write(&self.root().join(rel), content)
    }
    async fn call(&self, name: &str, input: Value) -> (String, bool) {
        tool::execute(&self.ctx(), name, &input).await
    }
}

fn write(p: &Path, content: &str) -> PathBuf {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
    p.to_path_buf()
}

fn sorted_lines(s: &str) -> Vec<String> {
    let mut v: Vec<String> = s.lines().map(String::from).collect();
    v.sort();
    v
}

fn allow(action: &str) -> Rule {
    Rule {
        action: action.into(),
        resource: "*".into(),
        effect: Effect::Allow,
        builtin: false,
    }
}

// --- tool-search.test.ts ---

/// "${name} distinguishes a malformed filename glob from an empty search"
/// (glob and grep). Fixed: the message read "Invalid glob in pattern error
/// parsing glob '[': ...".
#[tokio::test(flavor = "multi_thread")]
async fn oc_search_distinguishes_a_malformed_glob_from_an_empty_search() {
    let env = Env::new();
    env.write("visible.ts", "needle\n");
    let (out, err) = env.call("glob", json!({"pattern": "*.txt"})).await;
    assert!(!err, "{out}");
    assert_eq!(out, "No files found");
    let (out, err) = env
        .call("grep", json!({"pattern": "needle", "include": "*.txt"}))
        .await;
    assert!(!err, "{out}");
    assert_eq!(out, "No matches found");

    let (out, err) = env.call("glob", json!({"pattern": "["})).await;
    assert!(err, "{out}");
    assert_eq!(
        out,
        "Invalid glob in pattern '[': unclosed character class; missing ']'"
    );
    let (out, err) = env
        .call("grep", json!({"pattern": "needle", "include": "["}))
        .await;
    assert!(err, "{out}");
    assert_eq!(
        out,
        "Invalid glob in include '[': unclosed character class; missing ']'"
    );
}

/// "glob honors hidden=${hidden} before limit=${limit}" (all five cases).
#[tokio::test(flavor = "multi_thread")]
async fn oc_search_glob_honors_hidden_before_limit() {
    for (hidden, limit) in [
        (None, 1),
        (None, 10),
        (Some(false), 1),
        (Some(false), 10),
        (Some(true), 10),
    ] {
        let env = Env::new();
        for f in [
            "src/visible.ts",
            ".hidden.ts",
            "src/.hidden.ts",
            ".hidden/nested.ts",
            ".git/config.ts",
        ] {
            env.write(f, "needle\n");
        }
        let mut input = json!({"pattern": "**/*.ts", "limit": limit});
        if let Some(h) = hidden {
            input["hidden"] = json!(h);
        }
        let (out, err) = env.call("glob", input).await;
        assert!(!err, "{out}");
        let expected: Vec<&str> = if hidden == Some(true) {
            vec![
                ".hidden.ts",
                ".hidden/nested.ts",
                "src/.hidden.ts",
                "src/visible.ts",
            ]
        } else {
            vec!["src/visible.ts"]
        };
        let mut want: Vec<String> = expected
            .iter()
            .map(|f| env.root().join(f).display().to_string())
            .collect();
        want.sort();
        assert_eq!(sorted_lines(&out), want, "hidden={hidden:?} limit={limit}");
    }
}

/// "bounds omitted glob and grep limits"
#[tokio::test(flavor = "multi_thread")]
async fn oc_search_bounds_omitted_glob_and_grep_limits() {
    let env = Env::new();
    let limit = 100;
    for i in 0..=limit {
        env.write(&format!("{i}.txt"), "needle\n");
    }
    let truncated = format!(
        "(Results are truncated: showing first {limit} results. Consider using a more specific path or pattern.)"
    );
    let (glob, err) = env.call("glob", json!({"pattern": "*"})).await;
    assert!(!err, "{glob}");
    assert_eq!(glob.split('\n').count(), limit + 2, "{glob}");
    assert!(glob.ends_with(&truncated), "{glob}");
    let (grep, err) = env.call("grep", json!({"pattern": "needle"})).await;
    assert!(!err, "{grep}");
    assert!(
        grep.starts_with(&format!("Found {limit} matches\n")),
        "{grep}"
    );
    assert!(grep.ends_with(&truncated), "{grep}");
}

/// "rejects an empty grep pattern" (adapted: CodeSmith reports the rule
/// without OpenCode's argument-validation envelope).
#[tokio::test(flavor = "multi_thread")]
async fn oc_search_rejects_an_empty_grep_pattern() {
    let env = Env::new();
    let (out, err) = env.call("grep", json!({"pattern": ""})).await;
    assert!(err, "{out}");
    assert_eq!(out, "Pattern must not be empty");
}

/// "handles explicit grep file and directory paths"
#[tokio::test(flavor = "multi_thread")]
async fn oc_search_handles_explicit_grep_file_and_directory_paths() {
    let env = Env::new();
    let target = env.write("target.txt", "needle\n");
    let other = env.write("other.txt", "needle\n");
    let (file, err) = env
        .call("grep", json!({"path": "target.txt", "pattern": "needle"}))
        .await;
    assert!(!err, "{file}");
    assert!(file.starts_with("Found 1 matches"), "{file}");
    assert!(file.contains(&target.display().to_string()), "{file}");
    assert!(!file.contains(&other.display().to_string()), "{file}");
    let (dir, err) = env
        .call("grep", json!({"path": ".", "pattern": "needle"}))
        .await;
    assert!(!err, "{dir}");
    assert!(dir.starts_with("Found 2 matches"), "{dir}");
    assert!(dir.contains(&target.display().to_string()), "{dir}");
    assert!(dir.contains(&other.display().to_string()), "{dir}");
}

/// "reports no grep matches"
#[tokio::test(flavor = "multi_thread")]
async fn oc_search_reports_no_grep_matches() {
    let env = Env::new();
    env.write("file.txt", "haystack\n");
    let (out, err) = env.call("grep", json!({"pattern": "needle"})).await;
    assert!(!err, "{out}");
    assert_eq!(out, "No matches found");
}

/// "reports invalid grep regex details"
#[tokio::test(flavor = "multi_thread")]
async fn oc_search_reports_invalid_grep_regex_details() {
    let env = Env::new();
    let (out, err) = env.call("grep", json!({"pattern": "["})).await;
    assert!(err, "{out}");
    assert!(out.starts_with("Invalid regex pattern:"), "{out}");
    assert!(out.contains("unclosed character class"), "{out}");
}

/// "requires external_directory approval for external grep files and directories"
#[tokio::test(flavor = "multi_thread")]
async fn oc_search_external_grep_needs_external_directory_approval() {
    let env = Env::new();
    let outside = tempfile::tempdir().unwrap();
    let outside = outside.path().canonicalize().unwrap();
    write(&outside.join("outside.txt"), "needle\n");
    let (dir, err) = env
        .call(
            "grep",
            json!({"path": outside.display().to_string(), "pattern": "needle"}),
        )
        .await;
    assert!(!err, "{dir}");
    let (file, err) = env
        .call(
            "grep",
            json!({"path": outside.join("outside.txt").display().to_string(), "pattern": "needle"}),
        )
        .await;
    assert!(!err, "{file}");
    assert_eq!(
        env.rec.actions(),
        ["external_directory", "grep", "external_directory", "grep"]
    );
    // Adapted: for a file, CodeSmith names the file and saves its folder
    // (OpenCode names the folder), as the read tool does.
    let glob = format!("{}/*", outside.display());
    assert_eq!(env.rec.resources(0), std::slice::from_ref(&glob));
    let file = env.rec.asked.lock().unwrap()[2].clone();
    assert_eq!(
        file.resources,
        [outside.join("outside.txt").display().to_string()]
    );
    assert_eq!(file.save, [glob]);
}

/// "${name} reports a missing search path" (glob and grep). Fixed: glob
/// said the path was not a directory, grep "Unable to grep for ...".
#[tokio::test(flavor = "multi_thread")]
async fn oc_search_reports_a_missing_search_path() {
    let env = Env::new();
    for (name, pattern) in [("glob", "*"), ("grep", "needle")] {
        let (out, err) = env
            .call(name, json!({"path": "missing", "pattern": pattern}))
            .await;
        assert!(err, "{name}: {out}");
        assert_eq!(out, "Search path does not exist: missing", "{name}");
    }
}

/// "reports a file used as the glob search path"
#[tokio::test(flavor = "multi_thread")]
async fn oc_search_reports_a_file_used_as_the_glob_search_path() {
    let env = Env::new();
    env.write("file.txt", "content\n");
    let (out, err) = env
        .call("glob", json!({"path": "file.txt", "pattern": "*"}))
        .await;
    assert!(err, "{out}");
    assert_eq!(out, "Search path is not a directory: file.txt");
}

/// "requires external_directory approval for an explicit external glob path"
#[tokio::test(flavor = "multi_thread")]
async fn oc_search_external_glob_path_needs_external_directory_approval() {
    let env = Env::new();
    let outside = tempfile::tempdir().unwrap();
    let outside = outside.path().canonicalize().unwrap();
    write(&outside.join("outside.txt"), "outside\n");
    let (out, err) = env
        .call(
            "glob",
            json!({"path": outside.display().to_string(), "pattern": "*.txt"}),
        )
        .await;
    assert!(!err, "{out}");
    assert_eq!(env.rec.actions(), ["external_directory", "glob"]);
    assert_eq!(env.rec.resources(0), [format!("{}/*", outside.display())]);
}

/// "globs through an in-location external symlink without external approval"
/// Deliberate difference: OpenCode resolves the search path lexically;
/// CodeSmith follows the link and asks `external_directory` for its target
/// (as `oc_edit_external_symlink_target_needs_external_approval` pins for
/// edit), then lists the files under the link's name.
#[tokio::test(flavor = "multi_thread")]
async fn oc_search_globs_through_an_in_location_symlink_after_external_approval() {
    let env = Env::new();
    let outside = tempfile::tempdir().unwrap();
    let outside = outside.path().canonicalize().unwrap();
    write(&outside.join("outside.txt"), "outside\n");
    std::os::unix::fs::symlink(&outside, env.root().join("linked")).unwrap();
    let (out, err) = env
        .call("glob", json!({"path": "linked", "pattern": "*.txt"}))
        .await;
    assert!(!err, "{out}");
    assert_eq!(env.rec.actions(), ["external_directory", "glob"]);
    assert!(out.ends_with("outside.txt"), "{out}");
}

// --- ripgrep.test.ts (through the glob and grep tools) ---

/// "keeps matching files when an ignore glob is malformed and a symlink is broken"
#[tokio::test(flavor = "multi_thread")]
async fn oc_ripgrep_keeps_matches_despite_a_malformed_ignore_glob_and_a_broken_symlink() {
    let env = Env::new();
    let visible = env.write("visible.ts", "needle\n");
    env.write(".ignore", "[z-a]\n");
    std::os::unix::fs::symlink("missing", env.root().join("broken")).unwrap();
    let (out, err) = env.call("glob", json!({"pattern": "*.ts"})).await;
    assert!(!err, "{out}");
    assert_eq!(out, visible.display().to_string());
}

/// "globs files as an array"
#[tokio::test(flavor = "multi_thread")]
async fn oc_ripgrep_globs_files() {
    let env = Env::new();
    let m = env.write("src/match.ts", "needle\n");
    let (out, err) = env.call("glob", json!({"pattern": "**/*.ts"})).await;
    assert!(!err, "{out}");
    assert_eq!(out, m.display().to_string());
}

/// "greps files with include filtering"
#[tokio::test(flavor = "multi_thread")]
async fn oc_ripgrep_greps_files_with_include_filtering() {
    let env = Env::new();
    let m = env.write("src/match.ts", "needle\n");
    let skip = env.write("src/skip.txt", "needle\n");
    let (out, err) = env
        .call("grep", json!({"pattern": "needle", "include": "*.ts"}))
        .await;
    assert!(!err, "{out}");
    assert!(out.starts_with("Found 1 matches"), "{out}");
    assert!(out.contains(&m.display().to_string()), "{out}");
    assert!(!out.contains(&skip.display().to_string()), "{out}");
}

/// "greps lines that are not valid UTF-8"
#[tokio::test(flavor = "multi_thread")]
async fn oc_ripgrep_greps_lines_that_are_not_valid_utf8() {
    let env = Env::new();
    let utf8 = env.write("utf8.txt", "needle\n");
    let latin1 = env.root().join("latin1.txt");
    std::fs::write(&latin1, b"needle caf\xe9\n").unwrap();
    let (out, err) = env.call("grep", json!({"pattern": "needle"})).await;
    assert!(!err, "{out}");
    assert!(out.starts_with("Found 2 matches"), "{out}");
    assert!(out.contains(&utf8.display().to_string()), "{out}");
    assert!(out.contains(&latin1.display().to_string()), "{out}");
    assert!(out.contains("needle caf\u{FFFD}"), "{out}");
}

/// "keeps ignored files out of catch-all find results"
#[tokio::test(flavor = "multi_thread")]
async fn oc_ripgrep_keeps_ignored_files_out_of_catch_all_results() {
    let env = Env::new();
    std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(env.root())
        .status()
        .unwrap();
    env.write(".gitignore", "node_modules/\n");
    let ignored = env.write("node_modules/pkg/index.js", "ignored\n");
    let included = env.write("src/index.js", "included\n");
    let (out, err) = env.call("glob", json!({"pattern": "**/*"})).await;
    assert!(!err, "{out}");
    assert!(out.contains(&included.display().to_string()), "{out}");
    assert!(!out.contains(&ignored.display().to_string()), "{out}");
}

/// "never includes git metadata"
#[tokio::test(flavor = "multi_thread")]
async fn oc_ripgrep_never_includes_git_metadata() {
    let env = Env::new();
    let kept = env.write(".opencode/config", "needle\n");
    let git = env.write(".git/config", "needle\n");
    let (out, err) = env
        .call("glob", json!({"pattern": "**/*", "hidden": true}))
        .await;
    assert!(!err, "{out}");
    assert!(out.contains(&kept.display().to_string()), "{out}");
    assert!(!out.contains(&git.display().to_string()), "{out}");
    let (out, err) = env
        .call(
            "grep",
            json!({"pattern": "needle", "include": "config", "hidden": true}),
        )
        .await;
    assert!(!err, "{out}");
    assert!(!out.contains(&git.display().to_string()), "{out}");
}

/// "returns a bounded preview for matches on oversized lines" and "ends
/// oversized line previews on a whole character". Adapted: CodeSmith counts
/// 2,000 characters (OpenCode counts UTF-16 units) and marks the cut with
/// "…" (OpenCode "...").
#[tokio::test(flavor = "multi_thread")]
async fn oc_ripgrep_bounds_previews_of_oversized_lines_on_whole_characters() {
    let env = Env::new();
    env.write(
        "generated.ts",
        &format!("Cloudflare{}\n", "x".repeat(70 * 1024)),
    );
    let (out, err) = env.call("grep", json!({"pattern": "Cloudflare"})).await;
    assert!(!err, "{out}");
    let line = out
        .lines()
        .find(|l| l.contains("Cloudflare"))
        .unwrap()
        .to_string();
    let preview = &line[line.find("Cloudflare").unwrap()..];
    assert_eq!(preview.chars().count(), 2_001, "{preview}");
    assert!(preview.ends_with('…'));

    let env = Env::new();
    env.write("emoji.txt", &format!("{}😀tail\n", "x".repeat(1_999)));
    let (out, err) = env.call("grep", json!({"pattern": "tail"})).await;
    assert!(!err, "{out}");
    assert!(out.contains(&format!("{}😀…", "x".repeat(1_999))), "{out}");
}

// --- tool-write.test.ts ---

/// "registers and creates a relative file through FileMutation once"
#[tokio::test(flavor = "multi_thread")]
async fn oc_write_creates_a_relative_file_and_its_parents() {
    let env = Env::new();
    let (out, err) = env
        .call(
            "write",
            json!({"path": "src/new.txt", "content": "created"}),
        )
        .await;
    assert!(!err, "{out}");
    assert_eq!(
        std::fs::read_to_string(env.root().join("src/new.txt")).unwrap(),
        "created"
    );
    assert_eq!(env.rec.actions(), ["edit"]);
}

/// "overwrites a relative existing file and reports that it wrote the file"
#[tokio::test(flavor = "multi_thread")]
async fn oc_write_overwrites_an_existing_file() {
    let env = Env::new();
    env.write("existing.txt", "before");
    let (out, err) = env
        .call("write", json!({"path": "existing.txt", "content": "after"}))
        .await;
    assert!(!err, "{out}");
    assert!(out.contains("Wrote file successfully"), "{out}");
    assert_eq!(
        std::fs::read_to_string(env.root().join("existing.txt")).unwrap(),
        "after"
    );
}

/// "preserves exactly one BOM when overwriting existing files"
#[tokio::test(flavor = "multi_thread")]
async fn oc_write_preserves_exactly_one_bom() {
    let env = Env::new();
    env.write("preserved.txt", "\u{feff}before");
    env.write("deduplicated.txt", "\u{feff}before");
    let (out, err) = env
        .call(
            "write",
            json!({"path": "preserved.txt", "content": "after"}),
        )
        .await;
    assert!(!err, "{out}");
    let (out, err) = env
        .call(
            "write",
            json!({"path": "deduplicated.txt", "content": "\u{feff}after"}),
        )
        .await;
    assert!(!err, "{out}");
    for f in ["preserved.txt", "deduplicated.txt"] {
        assert_eq!(
            std::fs::read_to_string(env.root().join(f)).unwrap(),
            "\u{feff}after",
            "{f}"
        );
    }
}

/// "accepts an absolute file path inside the active Location"
#[tokio::test(flavor = "multi_thread")]
async fn oc_write_accepts_an_absolute_path_inside_the_project() {
    let env = Env::new();
    let target = env.root().join("absolute.txt");
    let (out, err) = env
        .call(
            "write",
            json!({"path": target.display().to_string(), "content": "inside"}),
        )
        .await;
    assert!(!err, "{out}");
    assert_eq!(env.rec.actions(), ["edit"]);
    assert_eq!(std::fs::read_to_string(target).unwrap(), "inside");
}

/// "approves an explicit external absolute path before edit". Adapted: the
/// request names the file and saves its folder.
#[tokio::test(flavor = "multi_thread")]
async fn oc_write_approves_an_external_path_before_edit() {
    let env = Env::new();
    let outside = tempfile::tempdir().unwrap();
    let outside = outside.path().canonicalize().unwrap();
    let target = outside.join("external.txt");
    let (out, err) = env
        .call(
            "write",
            json!({"path": target.display().to_string(), "content": "external"}),
        )
        .await;
    assert!(!err, "{out}");
    assert_eq!(env.rec.actions(), ["external_directory", "edit"]);
    let first = env.rec.asked.lock().unwrap()[0].clone();
    assert_eq!(first.save, [format!("{}/*", outside.display())]);
    assert_eq!(std::fs::read_to_string(target).unwrap(), "external");
}

/// "does not write when external_directory or edit approval is denied"
#[tokio::test(flavor = "multi_thread")]
async fn oc_write_does_not_write_when_denied() {
    let env = Env::new();
    env.rec
        .deny
        .lock()
        .unwrap()
        .push("external_directory".into());
    let outside = tempfile::tempdir().unwrap();
    let external = outside.path().join("denied.txt");
    let (out, err) = env
        .call(
            "write",
            json!({"path": external.display().to_string(), "content": "blocked"}),
        )
        .await;
    assert!(err, "{out}");
    assert_eq!(env.rec.actions(), ["external_directory"]);
    assert!(!external.exists());

    let env = Env::new();
    env.rec.deny.lock().unwrap().push("edit".into());
    let (out, err) = env
        .call("write", json!({"path": "denied.txt", "content": "blocked"}))
        .await;
    assert!(err, "{out}");
    assert_eq!(env.rec.actions(), ["edit"]);
    assert!(!env.root().join("denied.txt").exists());
}

/// "writes an external symlink target with only its in-location permission".
/// Deliberate difference: CodeSmith follows the link and asks
/// `external_directory` for its target before `edit`.
#[tokio::test(flavor = "multi_thread")]
async fn oc_write_external_symlink_target_needs_external_approval() {
    let env = Env::new();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("external.txt");
    std::fs::write(&target, "before").unwrap();
    std::os::unix::fs::symlink(&target, env.root().join("link.txt")).unwrap();
    let (out, err) = env
        .call("write", json!({"path": "link.txt", "content": "after"}))
        .await;
    assert!(!err, "{out}");
    assert_eq!(env.rec.actions(), ["external_directory", "edit"]);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "after");
}
