//! An isolated world for one test: a temp folder holding a git project, a
//! home, the config and data folders and the session database, and a way
//! to run the real `codesmith` binary inside it.
//!
//! The binary runs with a cleared environment (so no API keys or user
//! config leak in), `HOME`, `XDG_*`, `CODESMITH_CONFIG_DIR` and
//! `CODESMITH_DB` pointing into the temp folder, and in its own session
//! (`setsid`), so it has no controlling terminal and can never open
//! `/dev/tty` to ask the person running the tests.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

pub const BIN: &str = env!("CARGO_BIN_EXE_codesmith");

/// How long one binary run may take before the test fails.
pub const TIMEOUT: Duration = Duration::from_secs(60);

pub fn have(program: &str, arg: &str) -> bool {
    std::process::Command::new(program)
        .arg(arg)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

pub fn python() -> Option<&'static str> {
    ["python3", "python"]
        .into_iter()
        .find(|p| have(p, "--version"))
}

/// Return from the test (passing) when git is missing.
#[macro_export]
macro_rules! need_git {
    () => {
        if !$crate::harness::have("git", "--version") {
            eprintln!("skipped: git is not installed");
            return;
        }
    };
}

pub struct Out {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Out {
    pub fn ok(&self) -> &Self {
        assert_eq!(self.code, Some(0), "failed:\n{}", self.both());
        self
    }
    pub fn both(&self) -> String {
        format!("--- stdout\n{}\n--- stderr\n{}", self.stdout, self.stderr)
    }
    /// `--format json` lines.
    pub fn events(&self) -> Vec<Value> {
        self.stdout
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }
    /// The streamed text of a `--format json` run.
    pub fn text(&self) -> String {
        self.events()
            .iter()
            .filter(|e| e["type"] == "text")
            .filter_map(|e| e["text"].as_str())
            .collect()
    }
    /// The final `done` event of a `--format json` run.
    pub fn done(&self) -> Value {
        self.events()
            .into_iter()
            .rfind(|e| e["type"] == "done")
            .unwrap_or_else(|| panic!("no done event:\n{}", self.both()))
    }
}

pub struct Env {
    _tmp: tempfile::TempDir,
    pub root: PathBuf,
    pub home: PathBuf,
    pub project: PathBuf,
    pub config_dir: PathBuf,
    pub data: PathBuf,
    pub db: PathBuf,
}

impl Env {
    /// A world with an empty project folder (a git repo with one commit
    /// when `git` is installed).
    pub fn new() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let e = Env {
            home: root.join("home"),
            project: root.join("project"),
            config_dir: root.join("config"),
            data: root.join("data"),
            db: root.join("data/sessions.db"),
            root,
            _tmp: tmp,
        };
        for d in [&e.home, &e.project, &e.config_dir, &e.data] {
            std::fs::create_dir_all(d).unwrap();
        }
        if have("git", "--version") {
            e.git(&["init", "-q", "-b", "main"]);
            e.write("README.md", "# demo\n");
            e.commit("initial");
        }
        e
    }

    /// Environment variables every process in this world gets.
    pub fn vars(&self) -> Vec<(String, String)> {
        let p = |p: &Path| p.display().to_string();
        vec![
            ("PATH".into(), std::env::var("PATH").unwrap_or_default()),
            ("HOME".into(), p(&self.home)),
            ("USER".into(), "tester".into()),
            ("SHELL".into(), "/bin/sh".into()),
            ("TERM".into(), "dumb".into()),
            ("NO_COLOR".into(), "1".into()),
            ("TMPDIR".into(), p(&self.root)),
            ("XDG_CONFIG_HOME".into(), p(&self.home.join(".config"))),
            ("XDG_DATA_HOME".into(), p(&self.data)),
            ("XDG_CACHE_HOME".into(), p(&self.home.join(".cache"))),
            ("XDG_STATE_HOME".into(), p(&self.home.join(".state"))),
            ("CODESMITH_CONFIG_DIR".into(), p(&self.config_dir)),
            ("CODESMITH_DB".into(), p(&self.db)),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ("GIT_AUTHOR_NAME".into(), "Tester".into()),
            ("GIT_AUTHOR_EMAIL".into(), "tester@example.com".into()),
            ("GIT_COMMITTER_NAME".into(), "Tester".into()),
            ("GIT_COMMITTER_EMAIL".into(), "tester@example.com".into()),
        ]
    }

    pub fn git(&self, args: &[&str]) -> String {
        self.git_in(&self.project, args)
    }

    pub fn git_in(&self, dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env_clear()
            .envs(self.vars())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    pub fn commit(&self, message: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--no-gpg-sign", "-m", message]);
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.project.join(rel)
    }

    pub fn write(&self, rel: &str, text: &str) {
        let p = self.path(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    pub fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.path(rel)).unwrap_or_else(|e| panic!("reading {rel}: {e}"))
    }

    /// The global config: the scripted model as `fake/m` (and `fake/other`),
    /// plus `extra` (its `permissions` append; other keys replace).
    pub fn config(&self, model: &str, extra: Value) {
        let mut cfg = json!({
            "model": "fake/m",
            "providers": {"fake": {
                "protocol": "openai-chat",
                "settings": {"baseURL": model, "apiKey": "test-key"},
                "models": {
                    "m": {"limit": {"context": 100000}, "cost": {"input": 1.0, "output": 2.0}},
                    "other": {"name": "Other", "limit": {"context": 100000}}
                }
            }},
            "permissions": []
        });
        for (k, v) in extra.as_object().unwrap() {
            if k == "permissions" {
                cfg[k]
                    .as_array_mut()
                    .unwrap()
                    .extend(v.as_array().unwrap().clone());
            } else {
                cfg[k] = v.clone();
            }
        }
        std::fs::write(
            self.config_dir.join("codesmith.json"),
            serde_json::to_string_pretty(&cfg).unwrap(),
        )
        .unwrap();
    }

    /// The binary, ready to run in `dir` with this world's environment.
    pub fn command_in(&self, dir: &Path, args: &[&str]) -> tokio::process::Command {
        let mut c = tokio::process::Command::new(BIN);
        c.args(args)
            .current_dir(dir)
            .env_clear()
            .envs(self.vars())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Detach from the terminal running the tests.
        unsafe {
            c.pre_exec(|| {
                setsid();
                Ok(())
            });
        }
        c
    }

    pub fn command(&self, args: &[&str]) -> tokio::process::Command {
        self.command_in(&self.project, args)
    }

    /// Run the binary in the project and wait for it.
    pub async fn run(&self, args: &[&str]) -> Out {
        self.run_in(&self.project, args).await
    }

    pub async fn run_in(&self, dir: &Path, args: &[&str]) -> Out {
        let child = self.command_in(dir, args).spawn().unwrap();
        finish(child).await
    }

    /// Run the binary with `input` on stdin.
    pub async fn run_with_stdin(&self, args: &[&str], input: &str) -> Out {
        use tokio::io::AsyncWriteExt;
        let mut c = self.command(args);
        c.stdin(Stdio::piped());
        let mut child = c.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(input.as_bytes()).await.unwrap();
        drop(stdin);
        finish(child).await
    }

    pub fn store(&self) -> codesmith::core::store::Store {
        codesmith::core::store::Store::open(&self.db).unwrap()
    }

    /// A session's stored history as JSON messages.
    pub fn history(&self, id: &str) -> Vec<Value> {
        self.store()
            .history(id)
            .unwrap()
            .iter()
            .map(|m| serde_json::to_value(m).unwrap())
            .collect()
    }

    /// The record kinds in a session's log, in order.
    pub fn record_kinds(&self, id: &str) -> Vec<String> {
        self.store()
            .entries(id)
            .unwrap()
            .iter()
            .map(|e| {
                serde_json::to_value(&e.record).unwrap()["kind"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }
}

extern "C" {
    fn setsid() -> i32;
}

async fn finish(child: tokio::process::Child) -> Out {
    let out = tokio::time::timeout(TIMEOUT, child.wait_with_output())
        .await
        .expect("codesmith timed out")
        .unwrap();
    Out {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// Texts of a stored message's parts of one type (`text`, `tool_call`, ...).
pub fn parts<'a>(m: &'a Value, kind: &str) -> Vec<&'a Value> {
    m["parts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| p["type"] == kind)
        .map(|p| &p["data"])
        .collect()
}

/// The user-typed texts in a stored history (tool results are user-role too).
pub fn user_texts(history: &[Value]) -> Vec<String> {
    history
        .iter()
        .filter(|m| m["role"] == "user")
        .flat_map(|m| parts(m, "text"))
        .filter_map(|t| t.as_str().map(str::to_string))
        .collect()
}

/// Every tool result in a stored history.
pub fn tool_results(history: &[Value]) -> Vec<Value> {
    history
        .iter()
        .flat_map(|m| parts(m, "tool_result"))
        .cloned()
        .collect()
}
