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

// --- tool-read-filesystem.test.ts ---

/// "ReadTool text serialization": every case of the table, checked against
/// the text the model sees.
#[tokio::test(flavor = "multi_thread")]
async fn oc_read_text_serialization() {
    // (name, content, offset, limit, what the model sees)
    type Case = (
        &'static str,
        &'static str,
        Option<u64>,
        Option<u64>,
        &'static str,
    );
    let cases: &[Case] = &[
        (
            "reads a partial GIF signature as text",
            "GIF8 is text",
            None,
            None,
            "Read file lines.txt, lines 1-1\n1: GIF8 is text",
        ),
        (
            "preserves a selected trailing blank line before continuation",
            "alpha\n\nomega\n",
            Some(1),
            Some(2),
            "Read file lines.txt, lines 1-2\n1: alpha\n2: \n[Output truncated. Continue reading with offset: 3]",
        ),
        (
            "preserves multiple selected trailing blank lines at a noninitial offset",
            "before\nalpha\n\n\nomega\n",
            Some(2),
            Some(3),
            "Read file lines.txt, lines 2-4\n2: alpha\n3: \n4: \n[Output truncated. Continue reading with offset: 5]",
        ),
        (
            "preserves a selected trailing blank line at EOF",
            "alpha\n\n",
            None,
            Some(2),
            "Read file lines.txt, lines 1-2\n1: alpha\n2: ",
        ),
        (
            "preserves internal blank lines in a page",
            "alpha\n\nomega\n",
            None,
            Some(3),
            "Read file lines.txt, lines 1-3\n1: alpha\n2: \n3: omega",
        ),
        (
            "preserves continuation for a nonblank page",
            "alpha\n\nomega\n",
            None,
            Some(1),
            "Read file lines.txt, lines 1-1\n1: alpha\n[Output truncated. Continue reading with offset: 2]",
        ),
        (
            "strips only the terminal file newline in a whole-file read",
            "alpha\n\n",
            None,
            None,
            "Read file lines.txt, lines 1-2\n1: alpha\n2: ",
        ),
        (
            "does not add a line for a whole-file terminal newline",
            "alpha\n",
            None,
            None,
            "Read file lines.txt, lines 1-1\n1: alpha",
        ),
        (
            "preserves a whole-file read without a terminal newline",
            "alpha",
            None,
            None,
            "Read file lines.txt, lines 1-1\n1: alpha",
        ),
        (
            "preserves empty whole-file output",
            "",
            None,
            None,
            "Read file lines.txt, 0 lines",
        ),
        (
            "preserves empty-file page output",
            "",
            None,
            Some(2),
            "Read file lines.txt, 0 lines",
        ),
        (
            "numbers the only blank line in a newline-only file",
            "\n",
            None,
            Some(1),
            "Read file lines.txt, lines 1-1\n1: ",
        ),
        (
            "keeps the newline of a whole newline-only file",
            "\n",
            None,
            None,
            "Read file lines.txt, lines 1-1\n1: ",
        ),
        (
            "numbers a single blank line before continuation",
            "alpha\n\nomega\n",
            Some(2),
            Some(1),
            "Read file lines.txt, lines 2-2\n2: \n[Output truncated. Continue reading with offset: 3]",
        ),
        (
            "counts the last blank line when an offset is past EOF",
            "alpha\n\n",
            Some(9),
            Some(1),
            "Read file lines.txt, lines 2-2 (offset 9 is past the end; the file has 2 lines)\n2: ",
        ),
    ];
    let mut failures = vec![];
    for (name, content, offset, limit, model) in cases {
        let env = Env::new();
        env.write("lines.txt", content);
        let mut input = json!({"path": "lines.txt"});
        if let Some(o) = offset {
            input["offset"] = json!(o);
        }
        if let Some(l) = limit {
            input["limit"] = json!(l);
        }
        let (out, err) = env.call("read", input).await;
        if err || out != *model {
            failures.push(format!("{name}:\n  got  {out:?}\n  want {model:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Reads `name` (written with `content`) and splits what the model sees
/// into lines; `Err` when the read failed.
async fn read_lines(
    env: &Env,
    name: &str,
    content: &[u8],
    offset: Option<i64>,
    limit: Option<u64>,
) -> Result<Vec<String>, String> {
    std::fs::write(env.root().join(name), content).unwrap();
    let mut input = json!({"path": name});
    if let Some(o) = offset {
        input["offset"] = json!(o);
    }
    if let Some(l) = limit {
        input["limit"] = json!(l);
    }
    let (out, err) = env.call("read", input).await;
    if err {
        Err(out)
    } else {
        Ok(out.split('\n').map(String::from).collect())
    }
}

fn numbered(lines: &[String], from: usize, to: usize) -> Vec<String> {
    (from..=to)
        .map(|n| format!("{n}: {}", lines[n - 1]))
        .collect()
}

fn padded(count: usize, width: usize, fill: char) -> Vec<String> {
    (1..=count)
        .map(|i| {
            let mut s = i.to_string();
            while s.len() < width {
                s.push(fill);
            }
            s
        })
        .collect()
}

/// "returns the last lines when the offset is past the end"
#[tokio::test(flavor = "multi_thread")]
async fn oc_read_returns_the_last_lines_when_the_offset_is_past_the_end() {
    let env = Env::new();
    assert_eq!(
        read_lines(
            &env,
            "short.txt",
            b"one\ntwo\nthree\nfour\nfive\n",
            Some(9),
            Some(2)
        )
        .await
        .unwrap(),
        [
            "Read file short.txt, lines 4-5 (offset 9 is past the end; the file has 5 lines)",
            "4: four",
            "5: five"
        ]
    );
    assert_eq!(
        read_lines(&env, "empty.txt", b"", Some(5), None)
            .await
            .unwrap(),
        ["Read file empty.txt, 0 lines (offset 5 is past the end; the file has 0 lines)"]
    );
    let long = padded(1_800, 1_000, 'x');
    let tail = read_lines(
        &env,
        "long.txt",
        long.join("\n").as_bytes(),
        Some(2_000),
        Some(2_000),
    )
    .await
    .unwrap();
    assert_eq!(
        [tail[0].as_str(), tail.last().unwrap().as_str()],
        [
            "Read file long.txt, lines 1750-1800 (offset 2000 is past the end; the file has 1800 lines)",
            format!("1800: {}", long[1_799]).as_str()
        ]
    );
}

/// "returns the last lines of a file larger than the first chunk when the
/// offset is past the end"
#[tokio::test(flavor = "multi_thread")]
async fn oc_read_returns_the_last_lines_of_a_large_file_past_the_end() {
    let env = Env::new();
    let lines = padded(3_000, 100, 'x');
    let head = |name: &str, from: usize, to: usize| {
        format!(
            "Read file {name}, lines {from}-{to} (offset {{}} is past the end; the file has 3000 lines)"
        )
    };
    let lf = read_lines(
        &env,
        "lf.txt",
        format!("{}\n", lines.join("\n")).as_bytes(),
        Some(5_000),
        None,
    )
    .await
    .unwrap();
    assert_eq!(lf[0], head("lf.txt", 2_901, 3_000).replace("{}", "5000"));
    assert_eq!(lf[1..], numbered(&lines, 2_901, 3_000)[..]);
    let un = read_lines(
        &env,
        "unterminated.txt",
        lines.join("\n").as_bytes(),
        Some(3_001),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        un[0],
        head("unterminated.txt", 2_901, 3_000).replace("{}", "3001")
    );
    assert_eq!(un[1..], numbered(&lines, 2_901, 3_000)[..]);
    let crlf = read_lines(
        &env,
        "crlf.txt",
        format!("{}\r\n", lines.join("\r\n")).as_bytes(),
        Some(3_001),
        Some(5),
    )
    .await
    .unwrap();
    assert_eq!(
        crlf[0],
        head("crlf.txt", 2_996, 3_000).replace("{}", "3001")
    );
    assert_eq!(crlf[1..], numbered(&lines, 2_996, 3_000)[..]);
    let one = read_lines(
        &env,
        "crlf-unterminated.txt",
        lines.join("\r\n").as_bytes(),
        Some(4_000),
        Some(1),
    )
    .await
    .unwrap();
    assert_eq!(
        one,
        [
            head("crlf-unterminated.txt", 3_000, 3_000).replace("{}", "4000"),
            format!("3000: {}", lines[2_999])
        ]
    );
    // A mid-file byte order mark is line content.
    let mut marked = lines.clone();
    marked[2_900] = format!("\u{feff}{}", lines[2_900]);
    let bom = read_lines(
        &env,
        "bom.txt",
        marked.join("\n").as_bytes(),
        Some(3_001),
        None,
    )
    .await
    .unwrap();
    assert_eq!(bom[1], format!("2901: \u{feff}{}", lines[2_900]));
    let huge = read_lines(
        &env,
        "huge-last.txt",
        format!("{}\n{}", lines[..10].join("\n"), "z".repeat(300 * 1024)).as_bytes(),
        Some(12),
        None,
    )
    .await
    .unwrap();
    let mut want = vec![
        "Read file huge-last.txt, lines 1-11 (offset 12 is past the end; the file has 11 lines)"
            .to_string(),
    ];
    want.extend(numbered(&lines, 1, 10));
    want.push(format!(
        "11: {}... (line truncated to 2000 chars)",
        "z".repeat(2_000)
    ));
    assert_eq!(huge, want);
    let wide = padded(200, 3_000, 'w');
    let capped = read_lines(
        &env,
        "wide.txt",
        format!("{}\n", wide.join("\n")).as_bytes(),
        Some(201),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        capped[0],
        "Read file wide.txt, lines 176-200 (offset 201 is past the end; the file has 200 lines)"
    );
    let want: Vec<String> = (176..=200)
        .map(|n| {
            format!(
                "{n}: {}... (line truncated to 2000 chars)",
                &wide[n - 1][..2_000]
            )
        })
        .collect();
    assert_eq!(capped[1..], want[..]);
    let mut early = lines.join("\n").into_bytes();
    early[10] = 0;
    let err = read_lines(&env, "early-nul.txt", &early, Some(3_001), None)
        .await
        .unwrap_err();
    assert_eq!(err, "Cannot read binary file: early-nul.txt");
}

/// "starts a negative offset that many lines before the end"
#[tokio::test(flavor = "multi_thread")]
async fn oc_read_starts_a_negative_offset_that_many_lines_before_the_end() {
    let env = Env::new();
    let small = b"one\ntwo\nthree\nfour\nfive\n";
    assert_eq!(
        read_lines(&env, "small.txt", small, Some(-3), None)
            .await
            .unwrap(),
        [
            "Read file small.txt, lines 3-5",
            "3: three",
            "4: four",
            "5: five"
        ]
    );
    assert_eq!(
        read_lines(&env, "all.txt", small, Some(-50), None)
            .await
            .unwrap(),
        [
            "Read file all.txt, lines 1-5",
            "1: one",
            "2: two",
            "3: three",
            "4: four",
            "5: five"
        ]
    );
    assert_eq!(
        read_lines(&env, "limited.txt", small, Some(-4), Some(2))
            .await
            .unwrap(),
        [
            "Read file limited.txt, lines 2-3",
            "2: two",
            "3: three",
            "[Output truncated. Continue reading with offset: 4]"
        ]
    );
    assert_eq!(
        read_lines(&env, "crlf.txt", b"one\r\ntwo\r\nthree\r\n", Some(-2), None)
            .await
            .unwrap(),
        ["Read file crlf.txt, lines 2-3", "2: two", "3: three"]
    );
    assert_eq!(
        read_lines(&env, "unterminated.txt", b"one\ntwo\nthree", Some(-1), None)
            .await
            .unwrap(),
        ["Read file unterminated.txt, lines 3-3", "3: three"]
    );
    assert_eq!(
        read_lines(&env, "empty.txt", b"", Some(-5), None)
            .await
            .unwrap(),
        ["Read file empty.txt, 0 lines"]
    );
}

/// "starts a negative offset from the end of a file larger than the first chunk"
#[tokio::test(flavor = "multi_thread")]
async fn oc_read_starts_a_negative_offset_from_the_end_of_a_large_file() {
    let env = Env::new();
    let lines = padded(3_000, 100, 'x');
    let content = format!("{}\n", lines.join("\n"));
    let lf = read_lines(&env, "lf.txt", content.as_bytes(), Some(-50), None)
        .await
        .unwrap();
    assert_eq!(lf[0], "Read file lf.txt, lines 2951-3000");
    assert_eq!(lf[1..], numbered(&lines, 2_951, 3_000)[..]);
    let window = read_lines(&env, "window.txt", content.as_bytes(), Some(-50), Some(10))
        .await
        .unwrap();
    let mut want = vec!["Read file window.txt, lines 2951-2960".to_string()];
    want.extend(numbered(&lines, 2_951, 2_960));
    want.push("[Output truncated. Continue reading with offset: 2961]".into());
    assert_eq!(window, want);
    let crlf = read_lines(
        &env,
        "crlf.txt",
        lines.join("\r\n").as_bytes(),
        Some(-3),
        None,
    )
    .await
    .unwrap();
    assert_eq!(crlf[0], "Read file crlf.txt, lines 2998-3000");
    assert_eq!(crlf[1..], numbered(&lines, 2_998, 3_000)[..]);
    let whole = read_lines(&env, "whole.txt", content.as_bytes(), Some(-5_000), None)
        .await
        .unwrap();
    let n = whole.len();
    assert_eq!(
        [&whole[0], &whole[n - 2], &whole[n - 1]],
        [
            "Read file whole.txt, lines 1-506",
            &format!("506: {}", lines[505]),
            "[Output truncated. Continue reading with offset: 507]"
        ]
    );
    let wide = padded(200, 3_000, 'w');
    let clipped = read_lines(
        &env,
        "wide.txt",
        format!("{}\n", wide.join("\n")).as_bytes(),
        Some(-100),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        [clipped[0].as_str(), clipped.last().unwrap().as_str()],
        [
            "Read file wide.txt, lines 101-125",
            "[Output truncated. Continue reading with offset: 126]"
        ]
    );
}

/// "reads the end of a file many chunks long"
#[tokio::test(flavor = "multi_thread")]
async fn oc_read_reads_the_end_of_a_file_many_chunks_long() {
    let env = Env::new();
    let lines = padded(60_000, 50, 'x');
    let content = format!("{}\n", lines.join("\n"));
    let mut want = vec!["Read file negative.txt, lines 59996-60000".to_string()];
    want.extend(numbered(&lines, 59_996, 60_000));
    assert_eq!(
        read_lines(&env, "negative.txt", content.as_bytes(), Some(-5), None)
            .await
            .unwrap(),
        want
    );
    let mut want = vec![
        "Read file past.txt, lines 59998-60000 (offset 70000 is past the end; the file has 60000 lines)"
            .to_string(),
    ];
    want.extend(numbered(&lines, 59_998, 60_000));
    assert_eq!(
        read_lines(&env, "past.txt", content.as_bytes(), Some(70_000), Some(3))
            .await
            .unwrap(),
        want
    );
    let mut want = vec!["Read file deep.txt, lines 45000-45001".to_string()];
    want.extend(numbered(&lines, 45_000, 45_001));
    want.push("[Output truncated. Continue reading with offset: 45002]".into());
    assert_eq!(
        read_lines(&env, "deep.txt", content.as_bytes(), Some(45_000), Some(2))
            .await
            .unwrap(),
        want
    );
    let mut want = vec!["Read file far.txt, lines 50001-50003".to_string()];
    want.extend(numbered(&lines, 50_001, 50_003));
    want.push("[Output truncated. Continue reading with offset: 50004]".into());
    assert_eq!(
        read_lines(&env, "far.txt", content.as_bytes(), Some(-10_000), Some(3))
            .await
            .unwrap(),
        want
    );
    let mut early = content.into_bytes();
    early[10] = 0;
    for (name, offset) in [("early-nul.txt", -5), ("early-nul-past.txt", 70_000)] {
        let err = read_lines(&env, name, &early, Some(offset), None)
            .await
            .unwrap_err();
        assert_eq!(err, format!("Cannot read binary file: {name}"));
    }
}

/// "reads malformed UTF-8 lossily and still rejects null-byte binary content"
#[tokio::test(flavor = "multi_thread")]
async fn oc_read_reads_malformed_utf8_lossily_and_rejects_binary() {
    let env = Env::new();
    let err = read_lines(&env, "archive.dat", &[0, 1, 2, 3], None, None)
        .await
        .unwrap_err();
    assert_eq!(err, "Cannot read binary file: archive.dat");
    assert_eq!(
        read_lines(&env, "malformed.txt", &[0x68, 0x69, 0x80], None, None)
            .await
            .unwrap(),
        ["Read file malformed.txt, lines 1-1", "1: hi\u{FFFD}"]
    );
}

/// "reads text despite a binary-associated extension"
#[tokio::test(flavor = "multi_thread")]
async fn oc_read_reads_text_despite_a_binary_extension() {
    let env = Env::new();
    assert_eq!(
        read_lines(&env, "notes.docx", b"plain text", None, None)
            .await
            .unwrap(),
        ["Read file notes.docx, lines 1-1", "1: plain text"]
    );
}

/// "pages text with one-based offsets"
#[tokio::test(flavor = "multi_thread")]
async fn oc_read_pages_text_with_one_based_offsets() {
    let env = Env::new();
    assert_eq!(
        read_lines(&env, "lines.txt", b"one\r\ntwo\nthree", Some(2), Some(1))
            .await
            .unwrap(),
        [
            "Read file lines.txt, lines 2-2",
            "2: two",
            "[Output truncated. Continue reading with offset: 3]"
        ]
    );
}

/// "truncates long lines"
#[tokio::test(flavor = "multi_thread")]
async fn oc_read_truncates_long_lines() {
    let env = Env::new();
    let out = read_lines(
        &env,
        "long.txt",
        format!("{}😀tail", "a".repeat(1_999)).as_bytes(),
        None,
        Some(1),
    )
    .await
    .unwrap();
    assert_eq!(
        out[1],
        format!("1: {}... (line truncated to 2000 chars)", "a".repeat(1_999))
    );
}

// --- shell.test.ts ---

/// "builds command args per shell family" (the POSIX shells) and "passes an
/// unmatched zsh glob through to the command". Fixed: Zsh ran without
/// `+o nomatch`, so an unmatched glob failed the whole line.
#[tokio::test(flavor = "multi_thread")]
async fn oc_shell_passes_an_unmatched_zsh_glob_through_to_the_command() {
    let env = Env::with_rules(vec![allow("*")]);
    for shell in ["/bin/zsh", "/bin/bash", "/bin/sh"] {
        if !Path::new(shell).exists() {
            continue;
        }
        let mut ctx = env.ctx();
        ctx.shell = shell.into();
        ctx.jobs = Some(tool::shell::Jobs::default());
        let (out, err) = tool::execute(
            &ctx,
            "shell",
            &json!({"command": "echo opencode-missing-*.nope; echo after", "description": "glob"}),
        )
        .await;
        assert!(!err, "{shell}: {out}");
        assert!(
            out.starts_with("opencode-missing-*.nope\nafter"),
            "{shell}: {out}"
        );
    }
}
