//! Git repositories that references point at: parsing the forms a config
//! names them in, where each one's checkout lives in the cache, and the
//! cache itself, which clones and refreshes checkouts with `git`. Ported
//! from OpenCode's `repository.ts` and `repository-cache.ts` (MIT), pinned
//! by `tests/oc_references.rs`.
//!
//! The cache keeps one tracking checkout per remote and branch under
//! `<data dir>/repos/<host parts>/<path>[@<branch>]`: a branch-specific
//! reference gets its own directory, so a refresh of the default branch
//! never moves it. A refresh fetches and hard-resets ("newest wins"). The
//! network is only reached through `git`, each command under a timeout and
//! without terminal prompts; refreshes are throttled to one attempt a day
//! per checkout (failed attempts count), recorded under the cache's state
//! folder, and one lock per checkout serializes processes.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

/// A parsed repository reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// Lower-cased; `host:port` for an explicit port; `file` for a local one.
    pub host: String,
    pub path: String,
    pub segments: Vec<String>,
    pub owner: Option<String>,
    pub repo: String,
    /// What `git clone` is given.
    pub remote: String,
    /// How the reference is named to people.
    pub label: String,
    /// The URL scheme, with its colon (`https:`, `ssh:`, `file:`).
    pub protocol: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    InvalidReference { repository: String, message: String },
    UnsupportedLocal { repository: String, message: String },
    InvalidBranch { branch: String, message: String },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::InvalidReference {
                repository,
                message,
            }
            | Error::UnsupportedLocal {
                repository,
                message,
            } => write!(f, "{repository}: {message}"),
            Error::InvalidBranch { branch, message } => write!(f, "{branch}: {message}"),
        }
    }
}

impl std::error::Error for Error {}

fn normalize(input: &str) -> String {
    let mut s = input.trim();
    s = s.strip_prefix("git+").unwrap_or(s);
    let s = s.split('#').next().unwrap_or("");
    s.trim_end_matches('/').to_string()
}

fn trim_git(s: &str) -> &str {
    s.strip_suffix(".git").unwrap_or(s)
}

fn parts(s: &str) -> Vec<String> {
    s.split('/')
        .map(|p| trim_git(p.trim()).to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

fn safe_segment(s: &str) -> bool {
    s != "."
        && s != ".."
        && !s.contains(':')
        && !s.contains(char::is_whitespace)
        && !s.contains(['/', '\\'])
}

// The cache path makes each `:`-separated host part a directory.
fn safe_host(s: &str) -> bool {
    !s.is_empty() && !s.starts_with('-') && s.split(':').all(safe_segment)
}

fn host_like(s: &str) -> bool {
    s.contains('.') || s.contains(':') || s == "localhost"
}

/// `$CODESMITH_REPO_CLONE_GITHUB_BASE_URL` replaces `https://github.com/`
/// (OpenCode's `OPENCODE_REPO_CLONE_GITHUB_BASE_URL`).
fn github_remote(path: &str) -> String {
    match std::env::var("CODESMITH_REPO_CLONE_GITHUB_BASE_URL") {
        Ok(base) if !base.is_empty() => {
            let base = if base.ends_with('/') {
                base
            } else {
                format!("{base}/")
            };
            reqwest::Url::parse(&base)
                .and_then(|u| u.join(&format!("{path}.git")))
                .map(|u| u.to_string())
                .unwrap_or_else(|_| format!("{base}{path}.git"))
        }
        _ => format!("https://github.com/{path}.git"),
    }
}

fn build_remote(
    host: &str,
    segments: Vec<String>,
    remote: Option<String>,
    protocol: Option<String>,
) -> Option<Reference> {
    let segments: Vec<String> = segments
        .iter()
        .map(|s| trim_git(s).to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if !safe_host(host) || segments.is_empty() || !segments.iter().all(|s| safe_segment(s)) {
        return None;
    }
    let path = segments.join("/");
    let host = host.to_lowercase();
    let github = host == "github.com";
    Some(Reference {
        owner: (segments.len() == 2).then(|| segments[0].clone()),
        repo: segments.last().unwrap().clone(),
        remote: remote.unwrap_or_else(|| {
            if github {
                github_remote(&path)
            } else {
                format!("https://{host}/{path}.git")
            }
        }),
        label: if github && segments.len() == 2 {
            path.clone()
        } else {
            format!("{host}/{path}")
        },
        host,
        path,
        segments,
        protocol,
    })
}

fn build_file(url: &reqwest::Url, remote: &str) -> Option<Reference> {
    let path = url.to_file_path().ok()?;
    let text = path.display().to_string();
    let segments: Vec<String> = text
        .split(['/', '\\'])
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_end_matches(':').to_string())
        .collect();
    let last = segments.last()?.clone();
    Some(Reference {
        host: "file".into(),
        path: text.clone(),
        owner: None,
        repo: trim_git(&last).to_string(),
        remote: remote.to_string(),
        label: text,
        segments,
        protocol: Some("file:".into()),
    })
}

/// Parse a repository as a config names it: GitHub `owner/repo` (or
/// `github:owner/repo`), `host/path`, scp-style `git@host:path`, or a URL.
pub fn parse(input: &str) -> Option<Reference> {
    let cleaned = normalize(input);
    if cleaned.is_empty() {
        return None;
    }
    if let Some(rest) = cleaned.strip_prefix("github:") {
        let p: Vec<&str> = rest.split('/').collect();
        if p.len() == 2
            && p.iter()
                .all(|s| !s.is_empty() && !s.contains(char::is_whitespace))
        {
            return build_remote("github.com", vec![p[0].into(), p[1].into()], None, None);
        }
    }
    if !cleaned.contains("://") {
        let scp = regex::Regex::new(r"^(?:[^@/\s]+@)?([^:/\s]+):(.+)$").unwrap();
        if let Some(c) = scp.captures(&cleaned) {
            return build_remote(&c[1], parts(&c[2]), Some(cleaned.clone()), None);
        }
        let direct = parts(&cleaned);
        if direct.len() >= 2 && host_like(&direct[0]) {
            return build_remote(&direct[0], direct[1..].to_vec(), None, None);
        }
        if direct.len() == 2 {
            return build_remote("github.com", direct, None, None);
        }
    }
    let url = reqwest::Url::parse(&cleaned).ok()?;
    if url.scheme() == "file" {
        return build_file(&url, &cleaned);
    }
    let host = match (url.host_str(), url.port()) {
        (Some(h), Some(p)) => format!("{h}:{p}"),
        (Some(h), None) => h.to_string(),
        (None, _) => return None,
    };
    let segments = parts(url.path());
    let remote = if host == "github.com" {
        github_remote(&segments.join("/"))
    } else {
        cleaned.clone()
    };
    build_remote(
        &host,
        segments,
        Some(remote),
        Some(format!("{}:", url.scheme())),
    )
}

pub fn is_file(r: &Reference) -> bool {
    r.protocol.as_deref() == Some("file:")
}

pub fn is_remote(r: &Reference) -> bool {
    !is_file(r)
}

/// A remote repository, or a typed error.
pub fn parse_remote(input: &str) -> Result<Reference, Error> {
    let r = parse(input).ok_or_else(|| Error::InvalidReference {
        repository: input.into(),
        message:
            "Repository must be a git URL, host/path reference, or GitHub owner/repo shorthand"
                .into(),
    })?;
    if !is_remote(&r) {
        return Err(Error::UnsupportedLocal {
            repository: input.into(),
            message: "Local file repositories are not supported".into(),
        });
    }
    Ok(r)
}

pub fn validate_branch(branch: &str) -> Result<(), Error> {
    let ok = !branch.is_empty()
        && branch
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/_.-".contains(c))
        && !branch.starts_with('-')
        && !branch.contains("..");
    if ok {
        return Ok(());
    }
    Err(Error::InvalidBranch {
        branch: branch.into(),
        message: "Branch must contain only alphanumeric characters, /, _, ., and -, and cannot start with - or contain .."
            .into(),
    })
}

/// JavaScript's `encodeURIComponent`.
fn encode_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Where a checkout lives under `root`: a directory per host part and path
/// segment, `@<branch>` (percent-encoded) for a branch-specific one.
pub fn cache_path(root: &Path, r: &Reference, branch: Option<&str>) -> PathBuf {
    let mut p = root.to_path_buf();
    for h in r.host.split(':') {
        p.push(h);
    }
    for s in &r.segments {
        p.push(s);
    }
    match branch {
        Some(b) => {
            let mut name = p.into_os_string();
            name.push(format!("@{}", encode_component(b)));
            PathBuf::from(name)
        }
        None => p,
    }
}

/// What makes two spellings the same repository.
pub fn cache_identity(r: &Reference) -> String {
    format!("{}/{}", r.host, r.path)
}

pub fn same(a: &Reference, b: &Reference) -> bool {
    cache_identity(a) == cache_identity(b)
}

// --- The cache ---

/// How long one `git` command may run.
pub const GIT_TIMEOUT: Duration = Duration::from_secs(120);
/// How often a checkout is refreshed at most, failed attempts included.
pub const REFRESH_INTERVAL_MS: i64 = 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refresh {
    /// Never refresh an existing checkout.
    No,
    /// Refresh once per [`REFRESH_INTERVAL_MS`].
    Daily,
    /// Refresh now.
    Force,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Status {
    Cached,
    Cloned,
    Refreshed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Checkout {
    pub repository: String,
    pub host: String,
    pub remote: String,
    pub local_path: PathBuf,
    pub status: Status,
    pub head: Option<String>,
    pub branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheError {
    InvalidBranch {
        branch: String,
        message: String,
    },
    CloneFailed {
        repository: String,
        message: String,
    },
    FetchFailed {
        repository: String,
        message: String,
    },
    CheckoutFailed {
        repository: String,
        branch: String,
        message: String,
    },
    ResetFailed {
        repository: String,
        message: String,
    },
    LockFailed {
        local_path: String,
        message: String,
    },
    Operation {
        operation: String,
        path: String,
        message: String,
    },
}

impl std::fmt::Display for CacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CacheError::InvalidBranch { branch, message } => write!(f, "{branch}: {message}"),
            CacheError::CloneFailed {
                repository,
                message,
            } => {
                write!(f, "cloning {repository} failed: {message}")
            }
            CacheError::FetchFailed {
                repository,
                message,
            } => {
                write!(f, "fetching {repository} failed: {message}")
            }
            CacheError::CheckoutFailed {
                repository,
                branch,
                message,
            } => write!(f, "checking out {branch} of {repository} failed: {message}"),
            CacheError::ResetFailed {
                repository,
                message,
            } => {
                write!(f, "resetting {repository} failed: {message}")
            }
            CacheError::LockFailed {
                local_path,
                message,
            } => write!(f, "locking {local_path} failed: {message}"),
            CacheError::Operation {
                operation,
                path,
                message,
            } => write!(f, "{operation} {path}: {message}"),
        }
    }
}

impl std::error::Error for CacheError {}

/// The record of a checkout's last refresh attempt. Unknown fields (older
/// records) are ignored.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attempt {
    pub attempted_at: i64,
}

/// The checkout cache: checkouts under `root`, attempt records and locks
/// under `state`.
#[derive(Debug, Clone)]
pub struct Cache {
    pub root: PathBuf,
    pub state: PathBuf,
}

impl Default for Cache {
    fn default() -> Self {
        let data = crate::paths::data_dir();
        Cache {
            root: data.join("repos"),
            state: data.join("repos-state"),
        }
    }
}

/// The output of a `git` command that succeeded, or its error text.
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    run_git(dir, args, false)
}

// Every git command here is [`crate::git::remote_command`]: a checkout's
// contents come from a remote CodeSmith does not control, and the agent's
// tools can reach the cache, so nothing in a checkout's config may run a
// program (no hooks, no fsmonitor, no `ext::` transport, and no `file://`
// transport unless the reference itself is a local repository).

fn run_git(dir: &Path, args: &[&str], allow_file: bool) -> Result<String, String> {
    let mut cmd = crate::git::remote_command(dir, allow_file);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Without a controlling terminal, ssh cannot ask for a password or a
    // host key over the user's screen; it fails instead.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe and touches no Rust state.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    let mut child = cmd.spawn().map_err(|e| format!("git: {e}"))?;
    let deadline = Instant::now() + GIT_TIMEOUT;
    // Drain the pipes on threads so a chatty command cannot block on them.
    let out = child.stdout.take();
    let err = child.stderr.take();
    let read = |p: Option<std::process::ChildStdout>| {
        std::thread::spawn(move || {
            let mut s = String::new();
            if let Some(mut p) = p {
                let _ = std::io::Read::read_to_string(&mut p, &mut s);
            }
            s
        })
    };
    let out = read(out);
    let err = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(mut p) = err {
            let _ = std::io::Read::read_to_string(&mut p, &mut s);
        }
        s
    });
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "git {} timed out after {}s",
                    args.first().unwrap_or(&""),
                    GIT_TIMEOUT.as_secs()
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(format!("git: {e}")),
        }
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    if status.success() {
        Ok(stdout)
    } else {
        let msg = stderr.trim();
        Err(if msg.is_empty() {
            format!("git {} exited with {status}", args.join(" "))
        } else {
            msg.to_string()
        })
    }
}

fn now_ms() -> i64 {
    crate::store::now()
}

fn state_name(local_path: &Path) -> String {
    crate::instruction_state::hash(&serde_json::Value::String(local_path.display().to_string()))
}

impl Cache {
    pub fn new(root: impl Into<PathBuf>, state: impl Into<PathBuf>) -> Cache {
        Cache {
            root: root.into(),
            state: state.into(),
        }
    }

    /// Where a reference's checkout lives.
    pub fn path(&self, r: &Reference, branch: Option<&str>) -> PathBuf {
        cache_path(&self.root, r, branch)
    }

    fn attempt_file(&self, local_path: &Path) -> PathBuf {
        self.state.join(format!("{}.json", state_name(local_path)))
    }

    /// The last refresh attempt recorded for a checkout.
    pub fn attempt(&self, local_path: &Path) -> Option<Attempt> {
        let text = std::fs::read_to_string(self.attempt_file(local_path)).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Record (or with `None`, forget) a checkout's last refresh attempt.
    pub fn set_attempt(&self, local_path: &Path, attempt: Option<&serde_json::Value>) {
        let file = self.attempt_file(local_path);
        match attempt {
            Some(v) => {
                let _ = std::fs::create_dir_all(&self.state);
                let _ = std::fs::write(file, v.to_string());
            }
            None => {
                let _ = std::fs::remove_file(file);
            }
        }
    }

    /// Make sure the checkout of `r` (at `branch`, else the remote's default
    /// branch) exists, cloning it if needed and refreshing it as `refresh`
    /// says.
    pub fn ensure(
        &self,
        r: &Reference,
        branch: Option<&str>,
        refresh: Refresh,
    ) -> Result<Checkout, CacheError> {
        if let Some(b) = branch {
            validate_branch(b).map_err(|e| match e {
                Error::InvalidBranch { branch, message } => {
                    CacheError::InvalidBranch { branch, message }
                }
                e => CacheError::InvalidBranch {
                    branch: b.into(),
                    message: e.to_string(),
                },
            })?;
        }
        let local = self.path(r, branch);
        let op = |operation: &str, e: std::io::Error| CacheError::Operation {
            operation: operation.into(),
            path: local.display().to_string(),
            message: e.to_string(),
        };
        std::fs::create_dir_all(&self.state).map_err(|e| op("ensure state directory", e))?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.state.join(format!("{}.lock", state_name(&local))))
            .map_err(|e| CacheError::LockFailed {
                local_path: local.display().to_string(),
                message: e.to_string(),
            })?;
        lock.lock().map_err(|e| CacheError::LockFailed {
            local_path: local.display().to_string(),
            message: e.to_string(),
        })?;
        let result = self.ensure_locked(r, branch, refresh, &local);
        let _ = lock.unlock();
        result
    }

    fn ensure_locked(
        &self,
        r: &Reference,
        branch: Option<&str>,
        refresh: Refresh,
        local: &Path,
    ) -> Result<Checkout, CacheError> {
        let repository = r.label.clone();
        let op = |operation: &str, e: std::io::Error| CacheError::Operation {
            operation: operation.into(),
            path: local.display().to_string(),
            message: e.to_string(),
        };
        let parent = local.parent().unwrap_or(local);
        std::fs::create_dir_all(parent).map_err(|e| op("ensure cache directory", e))?;
        let target = parse(&r.remote).unwrap_or_else(|| r.clone());
        // Only a reference that is itself a local repository may use the
        // file transport (a remote one never needs it).
        let file = r.remote.starts_with("file:");
        let git = |dir: &Path, args: &[&str]| run_git(dir, args, file);
        // `git` walks upward, so an enclosing repository with a matching
        // origin could pass for the checkout: it must live exactly here.
        let reuse = local.is_dir()
            && git(local, &["rev-parse", "--show-toplevel"])
                .ok()
                .map(|t| PathBuf::from(t.trim()))
                .and_then(|t| t.canonicalize().ok())
                == local.canonicalize().ok()
            && git(local, &["config", "--get", "remote.origin.url"])
                .ok()
                .and_then(|o| parse(o.trim()))
                .is_some_and(|o| same(&o, &target));
        if !reuse && local.exists() {
            std::fs::remove_dir_all(local).map_err(|e| op("remove stale cache", e))?;
        }
        let now = now_ms();
        let refresh = match refresh {
            Refresh::Daily => self
                .attempt(local)
                .is_none_or(|a| now - a.attempted_at >= REFRESH_INTERVAL_MS),
            Refresh::Force => true,
            Refresh::No => false,
        };
        let status = if !reuse {
            Status::Cloned
        } else if refresh {
            Status::Refreshed
        } else {
            Status::Cached
        };
        if status != Status::Cached {
            // Recorded before the network work, so failures are throttled too.
            self.set_attempt(local, Some(&serde_json::json!({ "attemptedAt": now })));
        }
        if status == Status::Cloned {
            let dir = local.display().to_string();
            let mut args = vec!["clone", "--depth", "100"];
            if let Some(b) = branch {
                args.extend(["--branch", b]);
            }
            args.extend(["--", r.remote.as_str(), dir.as_str()]);
            git(parent, &args).map_err(|message| CacheError::CloneFailed {
                repository: repository.clone(),
                message,
            })?;
        }
        if status == Status::Refreshed {
            let fetch = |message| CacheError::FetchFailed {
                repository: repository.clone(),
                message,
            };
            git(local, &["fetch", "--all", "--prune"]).map_err(fetch)?;
            if let Some(b) = branch {
                let spec = format!("+refs/heads/{b}:refs/remotes/origin/{b}");
                git(local, &["fetch", "origin", &spec]).map_err(fetch)?;
            }
            // Checking out the tracked branch before resetting heals a
            // checkout left on another branch.
            let tracked = branch.map(str::to_string).or_else(|| {
                git(local, &["symbolic-ref", "refs/remotes/origin/HEAD"])
                    .ok()
                    .map(|t| {
                        t.trim()
                            .trim_start_matches("refs/remotes/origin/")
                            .to_string()
                    })
                    .filter(|t| !t.is_empty())
            });
            if let Some(b) = &tracked {
                git(local, &["checkout", "-B", b, &format!("origin/{b}")]).map_err(|message| {
                    CacheError::CheckoutFailed {
                        repository: repository.clone(),
                        branch: b.clone(),
                        message,
                    }
                })?;
            }
            let target = tracked
                .or_else(|| current_branch(local))
                .map(|b| format!("origin/{b}"))
                .unwrap_or_else(|| "HEAD".into());
            git(local, &["reset", "--hard", &target]).map_err(|message| {
                CacheError::ResetFailed {
                    repository: repository.clone(),
                    message,
                }
            })?;
        }
        Ok(Checkout {
            repository,
            host: r.host.clone(),
            remote: r.remote.clone(),
            local_path: local.to_path_buf(),
            status,
            head: git(local, &["rev-parse", "HEAD"])
                .ok()
                .map(|h| h.trim().to_string())
                .filter(|h| !h.is_empty()),
            branch: current_branch(local),
        })
    }
}

fn current_branch(dir: &Path) -> Option<String> {
    git(dir, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok()
        .map(|b| b.trim().to_string())
        .filter(|b| !b.is_empty())
}
