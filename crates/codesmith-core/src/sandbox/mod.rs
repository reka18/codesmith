//! The operating-system sandbox under the agent's shell commands (see
//! `docs/design/sandbox.md` and `docs/sandbox.md`).
//!
//! The command analysis ([`crate::tool::shell_scan`]) decides what to ask
//! about; the sandbox is the floor under it. Every command the agent runs
//! through `shell`, background jobs and everything they start included,
//! runs with a policy the kernel enforces:
//!
//! - **writes** only to the project, the session's own temp folder
//!   (`TMPDIR`), CodeSmith's scratch and tool-output folders, the package
//!   caches, `sandbox.write`, and the folders approved for that command;
//! - **reads** everywhere but the protected paths: the secret-file guards'
//!   folders and files ([`crate::permission::secret_guards`]), CodeSmith's
//!   own key and runtime files, and what a `read` deny rule names;
//! - **network** `all` (the default) or `none` (Unix sockets only).
//!
//! macOS runs each command under `sandbox-exec` with a profile made for it
//! ([`macos`]); Linux applies Landlock, a seccomp filter and
//! `no_new_privs` in the child before it runs the shell ([`linux`]). A
//! machine where neither works refuses agent commands unless
//! `sandbox.unsupported` is `"run"`.

use crate::permission::{Effect, Rule};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
pub mod linux;
pub mod macos;

/// What commands may connect to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    /// Anything, as without a sandbox.
    #[default]
    All,
    /// Listed hosts through a proxy: not built yet, so commands are
    /// refused with a message saying so.
    Allowlist,
    /// Local Unix sockets only.
    None,
}

impl Network {
    pub fn as_str(self) -> &'static str {
        match self {
            Network::All => "all",
            Network::Allowlist => "allowlist",
            Network::None => "none",
        }
    }
}

/// What to do on a machine that can't sandbox.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Unsupported {
    /// Refuse agent commands (the default).
    #[default]
    Refuse,
    /// Run them without a sandbox, with a notice once per session.
    Run,
}

/// The `sandbox` settings. Read from your own config only (the global
/// file, the per-repo files in the global folder, `$CODESMITH_CONFIG` and
/// `$CODESMITH_CONFIG_CONTENT`): CodeSmith reads no config from a
/// repository, so a repository can never set them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// `false` turns the sandbox off. On when unset.
    pub enabled: Option<bool>,
    /// `all` (the default), `none`, or `allowlist` (not available yet).
    pub network: Option<Network>,
    /// Extra hosts for `allowlist`; accepted, unused until the proxy is
    /// built.
    pub allow: Vec<String>,
    /// Extra folders commands may always write (`~/` for the home folder).
    pub write: Vec<String>,
    /// `false` removes the package caches from the folders commands may
    /// write (see [`cache_dirs`]). On when unset.
    pub caches: Option<bool>,
    /// `refuse` (the default) or `run`.
    pub unsupported: Option<Unsupported>,
}

impl Settings {
    pub fn enabled(&self) -> bool {
        self.enabled != Some(false)
    }

    pub fn network(&self) -> Network {
        self.network.unwrap_or_default()
    }

    pub fn unsupported(&self) -> Unsupported {
        self.unsupported.unwrap_or_default()
    }
}

static CURRENT: std::sync::RwLock<Option<Settings>> = std::sync::RwLock::new(None);

/// Make `settings` (a loaded config's) the settings in force for commands
/// whose session sets none of its own.
pub fn install(settings: &Settings) {
    *CURRENT.write().unwrap_or_else(|e| e.into_inner()) = Some(settings.clone());
}

/// The settings in force: the last config loaded's, else the defaults
/// (sandboxed).
pub fn current() -> Settings {
    CURRENT
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_default()
}

/// Whether this machine can sandbox commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Support {
    Yes,
    /// It can't; why, for messages and `codesmith debug sandbox`.
    No(String),
}

/// Whether this machine can sandbox commands, worked out once.
pub fn support() -> Support {
    static SUPPORT: std::sync::OnceLock<Support> = std::sync::OnceLock::new();
    SUPPORT.get_or_init(probe).clone()
}

#[cfg(target_os = "macos")]
fn probe() -> Support {
    macos::probe()
}

#[cfg(target_os = "linux")]
fn probe() -> Support {
    linux::probe()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn probe() -> Support {
    Support::No(format!(
        "CodeSmith has no sandbox for {}",
        std::env::consts::OS
    ))
}

/// What happens to a session's commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// They run in the sandbox.
    Enforce,
    /// They run without it, because the settings say so; the notice says
    /// why, once per session.
    Unsandboxed { notice: String },
    /// They are refused; the message says why and what to change.
    Refuse { message: String },
}

/// The setting that runs commands on a machine that can't sandbox.
pub const RUN_SETTING: &str = r#""sandbox": {"unsupported": "run"}"#;

/// What `settings` and `support` make of a session's commands.
pub fn decide(settings: &Settings, support: &Support) -> Decision {
    if !settings.enabled() {
        return Decision::Unsandboxed {
            notice: "The sandbox is off (\"sandbox\": {\"enabled\": false} in your config): \
                     shell commands can write anywhere you can."
                .into(),
        };
    }
    if settings.network() == Network::Allowlist {
        return Decision::Refuse {
            message: "Not run: \"sandbox\": {\"network\": \"allowlist\"} isn't available yet \
                      (it needs a proxy CodeSmith doesn't have yet). Set \"network\" to \"all\" \
                      or \"none\" in your config."
                .into(),
        };
    }
    match support {
        Support::Yes => Decision::Enforce,
        Support::No(why) => match settings.unsupported() {
            Unsupported::Run => Decision::Unsandboxed {
                notice: format!(
                    "This machine can't sandbox shell commands ({why}), and your config says \
                     to run them anyway (\"unsupported\": \"run\"): they can write anywhere \
                     you can."
                ),
            },
            Unsupported::Refuse => Decision::Refuse {
                message: format!(
                    "Not run: this machine can't sandbox shell commands ({why}), and CodeSmith \
                     doesn't run the agent's commands without one. To run them anyway, add \
                     {RUN_SETTING} to your config. `codesmith debug sandbox` says more."
                ),
            },
        },
    }
}

/// The policy one command runs with. Every path is absolute and canonical
/// (symlinks resolved, `/private/var` rather than `/var` on macOS); a
/// pattern is a permission pattern ([`crate::wildcard`]: `*` matches any
/// run of characters, `/` included) over such paths.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    pub network: Network,
    /// Folders (and files) commands may write, everything under them
    /// included.
    pub write: Vec<PathBuf>,
    /// Never read or written. A pattern without a wildcard covers that
    /// path and everything under it.
    pub protected: Vec<String>,
    /// Inside the protected paths, what stays readable (never writable):
    /// `~/.ssh/known_hosts`, `~/.ssh/config`, public keys.
    pub readable: Vec<String>,
    /// Agent sockets commands may connect to (`SSH_AUTH_SOCK`, GPG's).
    pub sockets: Vec<PathBuf>,
    /// The session's own temp folder, `TMPDIR` for the command.
    pub tmpdir: PathBuf,
}

impl Policy {
    /// Whether the policy lets a command write `path` (absolute; resolved
    /// as far as it exists).
    pub fn writes(&self, path: &Path) -> bool {
        let p = canonical(path);
        !self.protects(&p) && self.write.iter().any(|w| p.starts_with(w))
    }

    /// Whether `path` (canonical) is protected and not carved out as
    /// readable.
    pub fn protects(&self, path: &Path) -> bool {
        let s = path.display().to_string();
        self.protected.iter().any(|p| covers(p, &s))
    }

    /// Whether reading `path` (canonical) is refused.
    pub fn refuses_read(&self, path: &Path) -> bool {
        let s = path.display().to_string();
        self.protects(path) && !self.readable.iter().any(|p| covers(p, &s))
    }
}

/// Whether pattern `p` covers `path`: a wildcard pattern matching it, or a
/// plain path that is it or a folder above it.
pub(crate) fn covers(p: &str, path: &str) -> bool {
    if has_wildcard(p) {
        crate::wildcard::matches(path, p, false)
    } else {
        let p = p.trim_end_matches('/');
        path == p || path.strip_prefix(p).is_some_and(|r| r.starts_with('/'))
    }
}

pub(crate) fn has_wildcard(p: &str) -> bool {
    p.contains(['*', '?'])
}

/// The part of a pattern before its first wildcard.
pub(crate) fn fixed_prefix(p: &str) -> &str {
    match p.find(['*', '?']) {
        Some(i) => &p[..i],
        None => p,
    }
}

/// `path` with symlinks resolved as far as it exists, the rest appended
/// as written (after `..` and `.` are applied).
pub fn canonical(path: &Path) -> PathBuf {
    let path = crate::tool::normalize(path);
    let mut existing = path.as_path();
    let mut rest: Vec<&std::ffi::OsStr> = Vec::new();
    loop {
        if let Ok(real) = existing.canonicalize() {
            let mut out = real;
            for r in rest.iter().rev() {
                out.push(r);
            }
            return out;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name);
                existing = parent;
            }
            _ => return path.clone(),
        }
    }
}

/// A pattern with its fixed part canonical: `~/.ssh/*` on a machine where
/// home is a symlink becomes the real home's.
fn canonical_pattern(p: &str) -> String {
    let fixed = fixed_prefix(p);
    // Up to the last `/` of the fixed part: the folder that exists.
    let cut = fixed.rfind('/').unwrap_or(0);
    let (dir, tail) = p.split_at(cut);
    if dir.is_empty() {
        return p.to_string();
    }
    let real = canonical(Path::new(dir));
    format!("{}{tail}", real.display())
}

/// Package caches outside the project that package managers write:
/// Cargo's registry and git checkouts (and its lock files), npm's cache,
/// pip's, and Go's module and build caches. Each honours its usual
/// variable (`CARGO_HOME`, `npm_config_cache`, `PIP_CACHE_DIR`,
/// `GOMODCACHE`, `GOCACHE`), and a `CARGO_TARGET_DIR` set for every build
/// is included too. Rustup's toolchains are not among them: a
/// toolchain runs outside the sandbox later, so a command must not be
/// able to change one.
pub fn cache_dirs() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let var = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let cargo = var("CARGO_HOME").unwrap_or_else(|| home.join(".cargo"));
    let mut out = vec![
        cargo.join("registry"),
        cargo.join("git"),
        cargo.join(".package-cache"),
        cargo.join(".package-cache-mutate"),
        cargo.join(".global-cache"),
        var("npm_config_cache").unwrap_or_else(|| home.join(".npm")),
    ];
    match var("PIP_CACHE_DIR") {
        Some(p) => out.push(p),
        None => {
            out.push(home.join(".cache").join("pip"));
            if cfg!(target_os = "macos") {
                out.push(home.join("Library").join("Caches").join("pip"));
            }
        }
    }
    let gopath = var("GOPATH")
        .and_then(|g| std::env::split_paths(&g).next())
        .unwrap_or_else(|| home.join("go"));
    out.push(var("GOMODCACHE").unwrap_or_else(|| gopath.join("pkg").join("mod")));
    // A shared Cargo target folder set for every build is where builds go.
    if let Some(t) = var("CARGO_TARGET_DIR").filter(|t| t.is_absolute()) {
        out.push(t);
    }
    match var("GOCACHE") {
        Some(p) => out.push(p),
        None if cfg!(target_os = "macos") => {
            out.push(home.join("Library").join("Caches").join("go-build"))
        }
        None => out.push(home.join(".cache").join("go-build")),
    }
    out
}

/// What the protected list is made from besides the rules: CodeSmith's own
/// key and runtime files (the remote-access key and database, the shell
/// daemon's and the bus's runtime folders, which hold their tokens) and,
/// on macOS, the keychains.
fn own_protected() -> Vec<String> {
    let data = crate::paths::data_dir();
    let state = crate::paths::state_dir();
    let mut out = vec![
        format!("{}/remote-key*", data.display()),
        format!("{}/remote.db*", data.display()),
        format!("{}/remote", data.display()),
        state.join("shelld").display().to_string(),
        state.join("bus").display().to_string(),
        format!("{}/remote*", state.display()),
    ];
    if cfg!(target_os = "macos") {
        if let Some(h) = dirs::home_dir() {
            out.push(h.join("Library").join("Keychains").display().to_string());
        }
        out.push("/Library/Keychains".into());
    }
    out
}

/// The protected paths and their readable carve-outs: the `read` secret
/// guards that name absolute paths, `read` deny rules that do, and
/// [`own_protected`]. Two guards are narrowed so agents keep working:
/// `~/.ssh` keeps `known_hosts`, `config` and public keys readable, and in
/// `~/.gnupg` only the private keys and revocation certificates are
/// protected (public keyrings and the trust database stay readable).
/// Guards on file names anywhere (`*.env`, `.git-credentials`) are left
/// to the permission rules: a sandbox can't express them without refusing
/// every project's `.env`.
pub fn protected(rules: &[Rule]) -> (Vec<String>, Vec<String>) {
    let homes: Vec<String> = dirs::home_dir()
        .into_iter()
        .flat_map(|h| {
            let real = h.canonicalize().ok();
            std::iter::once(h).chain(real)
        })
        .map(|h| h.display().to_string().trim_end_matches('/').to_string())
        .collect();
    let mut prot: Vec<String> = Vec::new();
    let mut readable: Vec<String> = Vec::new();
    let push = |v: &mut Vec<String>, p: String| {
        let p = canonical_pattern(&p);
        if !v.contains(&p) {
            v.push(p);
        }
    };
    let guards = crate::permission::secret_guards();
    let named = guards.iter().chain(rules.iter().filter(|r| {
        r.effect == Effect::Deny && r.action == "read" && !r.builtin && r.policy.is_none()
    }));
    for r in named {
        if r.action != "read" || !r.resource.starts_with('/') {
            continue;
        }
        if r.guard && r.effect != Effect::Ask {
            continue;
        }
        let res = r.resource.trim_end_matches('/');
        let gnupg = homes
            .iter()
            .find(|h| res == format!("{h}/.gnupg") || res == format!("{h}/.gnupg/*"));
        if let Some(h) = gnupg {
            for p in ["private-keys-v1.d", "secring.gpg", "openpgp-revocs.d"] {
                push(&mut prot, format!("{h}/.gnupg/{p}"));
            }
            continue;
        }
        if let Some(h) = homes.iter().find(|h| res == format!("{h}/.ssh")) {
            for p in ["known_hosts*", "config", "config.d/*", "*.pub"] {
                push(&mut readable, format!("{h}/.ssh/{p}"));
            }
        }
        // `<dir>/*` is the folder: protect it whole (its listing too).
        let res = res.strip_suffix("/*").unwrap_or(res);
        push(&mut prot, res.to_string());
    }
    for p in own_protected() {
        push(&mut prot, p);
    }
    (prot, readable)
}

/// The agent sockets a command may connect to: `SSH_AUTH_SOCK` from its
/// environment and GPG's agent socket (`gpgconf --list-dirs`, else
/// `~/.gnupg/S.gpg-agent`).
pub fn agent_sockets(env: Option<&std::collections::BTreeMap<String, String>>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let ssh = match env {
        Some(e) => e.get("SSH_AUTH_SOCK").cloned(),
        None => std::env::var("SSH_AUTH_SOCK").ok(),
    };
    if let Some(s) = ssh.filter(|s| s.starts_with('/')) {
        out.push(canonical(Path::new(&s)));
    }
    if let Some(g) = gnupg_home(env) {
        for name in ["S.gpg-agent", "S.gpg-agent.extra", "S.keyboxd"] {
            out.push(canonical(&g.join(name)));
        }
    }
    if let Some(run) = gpg_socket_dir() {
        for name in ["S.gpg-agent", "S.keyboxd"] {
            let p = canonical(&run.join(name));
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

/// GPG's home folder: `GNUPGHOME` from the command's environment, else
/// `~/.gnupg`.
fn gnupg_home(env: Option<&std::collections::BTreeMap<String, String>>) -> Option<PathBuf> {
    match env {
        Some(e) => e.get("GNUPGHOME").cloned(),
        None => std::env::var("GNUPGHOME").ok(),
    }
    .filter(|s| !s.is_empty())
    .map(PathBuf::from)
    .or_else(|| dirs::home_dir().map(|h| h.join(".gnupg")))
}

/// Where GPG puts its sockets on Linux (`/run/user/<uid>/gnupg`).
fn gpg_socket_dir() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        let d = PathBuf::from(format!("/run/user/{uid}/gnupg"));
        return d.is_dir().then_some(d);
    }
    #[allow(unreachable_code)]
    None
}

/// The session's own temp folder: `codesmith-sandbox-<session>` in the
/// system temp folder, private to this user. Created when missing.
pub fn session_tmp(session_id: &str) -> std::io::Result<PathBuf> {
    let safe: String = session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_".contains(c) {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    #[cfg(unix)]
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    #[cfg(not(unix))]
    let uid = 0;
    let dir = std::env::temp_dir().join(format!("codesmith-sandbox-{uid}-{safe}"));
    crate::paths::owned_private_dir(&dir)?;
    Ok(canonical(&dir))
}

/// The folders a configured rule or a saved approval lets commands write
/// outside the project: `external_directory` allows naming one folder
/// (`/x/y/*` or `/x/y`), not a wildcard, `/`, the home folder or above it,
/// nor a shared temp folder. The permission rules don't tell reading from
/// writing apart, so an approval for a folder counts for both, as it does
/// for the `shell` tool's own checks.
pub fn approved_dirs(rules: &[Rule]) -> Vec<PathBuf> {
    let home = dirs::home_dir().map(|h| canonical(&h));
    let mut out: Vec<PathBuf> = Vec::new();
    for r in rules {
        if r.action != "external_directory" || r.effect != Effect::Allow || r.policy.is_some() {
            continue;
        }
        let base = r.resource.strip_suffix("/*").unwrap_or(&r.resource);
        let base = match base.strip_prefix("~/") {
            Some(rest) => match dirs::home_dir() {
                Some(h) => h.join(rest).display().to_string(),
                None => continue,
            },
            None => base.to_string(),
        };
        if !base.starts_with('/') || has_wildcard(&base) {
            continue;
        }
        let p = canonical(Path::new(&base));
        let broad = p.parent().is_none()
            || home.as_ref().is_some_and(|h| h.starts_with(&p))
            || crate::permission::shared_dir(&p);
        if !broad && !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// What a policy is built from.
pub struct Inputs<'a> {
    /// The project folder (`None` when the session is in a folder too
    /// broad to be one: home or `/`).
    pub project: Option<&'a Path>,
    pub session_id: &'a str,
    pub settings: &'a Settings,
    /// Folders approved for this command (by its checks, or a rerun after
    /// the sandbox refused a write).
    pub grants: &'a [PathBuf],
    /// The configured rules and saved approvals.
    pub rules: &'a [Rule],
    /// The command's environment, when the session sets one.
    pub env: Option<&'a std::collections::BTreeMap<String, String>>,
}

/// Build the policy for one command.
pub fn policy(inp: &Inputs<'_>) -> std::io::Result<Policy> {
    let tmpdir = session_tmp(inp.session_id)?;
    let mut write: Vec<PathBuf> = Vec::new();
    let mut add = |p: PathBuf| {
        let p = canonical(&p);
        if !write.iter().any(|w| p.starts_with(w)) {
            write.retain(|w| !w.starts_with(&p));
            write.push(p);
        }
    };
    if let Some(p) = inp.project {
        add(p.to_path_buf());
        for g in git_dirs(p) {
            add(g);
        }
    }
    add(tmpdir.clone());
    add(crate::paths::tmp_dir());
    add(crate::paths::session_output_dir(inp.session_id));
    if inp.settings.caches != Some(false) {
        for c in cache_dirs() {
            add(c);
        }
    }
    for w in &inp.settings.write {
        let p = match w.strip_prefix("~/") {
            Some(rest) => match dirs::home_dir() {
                Some(h) => h.join(rest),
                None => continue,
            },
            None if w == "~" => match dirs::home_dir() {
                Some(h) => h,
                None => continue,
            },
            None => PathBuf::from(w),
        };
        if p.is_absolute() {
            add(p);
        }
    }
    // GPG opens its trust database for writing even to sign, and keeps
    // its random seed: those two files, nothing else in its folder (a
    // writable `gpg-agent.conf` could name a program for the agent to
    // run outside the sandbox).
    if let Some(g) = gnupg_home(inp.env) {
        add(g.join("trustdb.gpg"));
        add(g.join("random_seed"));
    }
    for d in approved_dirs(inp.rules) {
        add(d);
    }
    for g in inp.grants {
        add(g.clone());
    }
    let (protected, readable) = protected(inp.rules);
    Ok(Policy {
        network: match inp.settings.network() {
            Network::None => Network::None,
            _ => Network::All,
        },
        write,
        protected,
        readable,
        sockets: agent_sockets(inp.env),
        tmpdir,
    })
}

/// The git folders a linked worktree writes outside itself: its own git
/// folder (`.git` is a file naming it) and the common one it shares with
/// the main checkout. A commit in the worktree writes objects and refs
/// there. Nothing for an ordinary checkout, whose `.git` is inside it.
pub fn git_dirs(project: &Path) -> Vec<PathBuf> {
    let dotgit = project.join(".git");
    let Ok(text) = std::fs::read_to_string(&dotgit) else {
        return Vec::new();
    };
    let Some(dir) = text.trim().strip_prefix("gitdir:") else {
        return Vec::new();
    };
    let dir = Path::new(dir.trim());
    let gitdir = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        project.join(dir)
    };
    if !gitdir.is_dir() {
        return Vec::new();
    }
    let mut out = vec![canonical(&gitdir)];
    if let Ok(common) = std::fs::read_to_string(gitdir.join("commondir")) {
        let c = Path::new(common.trim());
        let c = if c.is_absolute() {
            c.to_path_buf()
        } else {
            gitdir.join(c)
        };
        if c.is_dir() {
            out.push(canonical(&c));
        }
    }
    out
}

/// The fewest paths that cover everything under `root` but what a
/// protected pattern covers, for a sandbox that can only allow (Landlock):
/// a folder with nothing protected under it is one entry; one with
/// something protected under it is replaced by its entries, each judged
/// the same way. Inside a protected folder, only the `readable` carve-outs
/// are listed. A symlink is listed only when what it points to isn't
/// protected. Entries made later directly in a split folder aren't
/// covered.
pub fn cover(root: &Path, protected: &[String], readable: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let root_s = root.display().to_string();
    let prot_root = protected.iter().any(|p| covers(p, &root_s));
    walk(root, protected, readable, prot_root, 0, &mut out);
    out
}

/// Whether something a pattern covers may be at or under `dir`.
fn reaches_under(p: &str, dir: &str) -> bool {
    let d = format!("{}/", dir.trim_end_matches('/'));
    let fixed = fixed_prefix(p);
    if has_wildcard(p) {
        fixed.starts_with(&d) || d.starts_with(fixed) || covers(p, dir)
    } else {
        let f = format!("{}/", fixed.trim_end_matches('/'));
        f.starts_with(&d)
    }
}

fn walk(
    dir: &Path,
    protected: &[String],
    readable: &[String],
    inside_protected: bool,
    depth: usize,
    out: &mut Vec<PathBuf>,
) {
    let dir_s = dir.display().to_string();
    if !inside_protected {
        if !protected.iter().any(|p| reaches_under(p, &dir_s)) {
            out.push(dir.to_path_buf());
            return;
        }
    } else if !readable.iter().any(|p| reaches_under(p, &dir_s)) {
        return;
    }
    // Deep trees with something protected at every level would be a
    // pattern problem: stop rather than walk the disk.
    if depth > 32 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.sort();
    for c in entries {
        let s = c.display().to_string();
        let prot = inside_protected || protected.iter().any(|p| covers(p, &s));
        let carved = readable.iter().any(|p| covers(p, &s));
        let Ok(meta) = std::fs::symlink_metadata(&c) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            // Landlock follows it: list it only when its target is fine.
            let Ok(real) = c.canonicalize() else { continue };
            let r = real.display().to_string();
            let target_prot =
                protected.iter().any(|p| covers(p, &r)) && !readable.iter().any(|p| covers(p, &r));
            if (!prot || carved) && !target_prot && !reaches_any(protected, &r) {
                out.push(c);
            }
            continue;
        }
        if prot && carved {
            out.push(c);
            continue;
        }
        if meta.is_dir() {
            walk(&c, protected, readable, prot, depth + 1, out);
        } else if !prot {
            out.push(c);
        }
    }
}

/// Whether something protected is under `dir` (so granting `dir` whole
/// would grant it).
fn reaches_any(protected: &[String], dir: &str) -> bool {
    protected.iter().any(|p| reaches_under(p, dir))
}

/// What a failed command's output says the sandbox refused: the first path
/// it names next to "Operation not permitted" or "Permission denied" that
/// the policy doesn't let it write, that this user could write without the
/// sandbox, and that isn't protected. The folder to offer is the path's
/// own when it is a folder, else the folder it is in (or the nearest one
/// that exists).
pub fn refused_write(output: &str, cwd: &Path, policy: &Policy) -> Option<Refusal> {
    for line in output.lines() {
        let lower = line.to_ascii_lowercase();
        if !(lower.contains("operation not permitted")
            || lower.contains("permission denied")
            || lower.contains("read-only file system"))
        {
            continue;
        }
        for cand in path_candidates(line) {
            let p = Path::new(&cand);
            let abs = if p.is_absolute() {
                p.to_path_buf()
            } else {
                cwd.join(p)
            };
            let real = canonical(&abs);
            if policy.protects(&real) {
                return Some(Refusal::Protected(real));
            }
            if policy.writes(&real) || !writable_without_sandbox(&real) {
                continue;
            }
            let folder = if real.is_dir() {
                real.clone()
            } else {
                let mut f = real.parent().map(Path::to_path_buf).unwrap_or_default();
                while !f.as_os_str().is_empty() && !f.is_dir() {
                    match f.parent() {
                        Some(p) => f = p.to_path_buf(),
                        None => break,
                    }
                }
                f
            };
            return Some(Refusal::Write { path: real, folder });
        }
    }
    None
}

/// What the sandbox refused, read from a command's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// A write to `path` outside the folders the command may write;
    /// `folder` is what to offer.
    Write { path: PathBuf, folder: PathBuf },
    /// A protected path.
    Protected(PathBuf),
}

/// Whether this process (outside any sandbox) could write `path`: it, or
/// the nearest folder above it that exists, is writable to this user.
fn writable_without_sandbox(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let mut p = path;
        loop {
            if std::fs::symlink_metadata(p).is_ok() {
                let Ok(c) = std::ffi::CString::new(p.as_os_str().as_bytes()) else {
                    return false;
                };
                // SAFETY: `c` is a valid NUL-terminated path.
                return unsafe { libc::access(c.as_ptr(), libc::W_OK) } == 0;
            }
            match p.parent() {
                Some(parent) => p = parent,
                None => return false,
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

/// The path-like words of an error line: quoted strings and words with a
/// `/` in them, without a trailing `:` or `,`.
fn path_candidates(line: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    // Quoted: '…', "…", `…', ‘…’.
    for (open, close) in [
        ('\'', '\''),
        ('"', '"'),
        ('`', '\''),
        ('‘', '’'),
        ('“', '”'),
    ] {
        let mut rest = line;
        while let Some(i) = rest.find(open) {
            let after = &rest[i + open.len_utf8()..];
            let Some(j) = after.find(close) else { break };
            let q = &after[..j];
            if q.contains('/') && !out.iter().any(|o| o == q) {
                out.push(q.to_string());
            }
            rest = &after[j + close.len_utf8()..];
        }
    }
    for w in line.split_whitespace() {
        let w = w.trim_matches(|c: char| "'\"`‘’“”(),;".contains(c));
        let w = w.trim_end_matches(':');
        if w.contains('/') && !w.contains("://") && !out.iter().any(|o| o == w) {
            out.push(w.to_string());
        }
    }
    out
}

/// The note added to a `sudo` command's refusal.
pub const SUDO_NOTE: &str = "sudo can't gain privileges inside CodeSmith's sandbox, so the agent \
can't run it. A way to enter your password for one command, which then runs that command \
outside the sandbox, is planned (docs/design/askpass.md). Until then, run it yourself in your \
own terminal.";

/// Whether `program` (a command's first word) is `sudo` or `doas`.
pub fn is_sudo(program: &str) -> bool {
    let name = program.rsplit('/').next().unwrap_or(program);
    matches!(name, "sudo" | "doas")
}

/// A human-readable description of a policy, for `codesmith debug
/// sandbox`.
pub fn describe(policy: &Policy) -> String {
    let mut s = String::new();
    s.push_str(&format!("network: {}\n", policy.network.as_str()));
    s.push_str(&format!("TMPDIR: {}\n", policy.tmpdir.display()));
    s.push_str("write:\n");
    for w in &policy.write {
        s.push_str(&format!("  {}\n", w.display()));
    }
    s.push_str("read: everywhere except\n");
    for p in &policy.protected {
        s.push_str(&format!("  {p}\n"));
    }
    if !policy.readable.is_empty() {
        s.push_str("still readable inside those:\n");
        for p in &policy.readable {
            s.push_str(&format!("  {p}\n"));
        }
    }
    s.push_str("agent sockets:\n");
    for p in &policy.sockets {
        s.push_str(&format!("  {}\n", p.display()));
    }
    s
}

#[cfg(test)]
mod tests;
