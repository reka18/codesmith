//! `codesmith upgrade` and the opt-in update check: OpenCode's updater
//! (`packages/cli/src/services/updater.ts`, MIT), adapted to how CodeSmith
//! is distributed.
//!
//! CodeSmith releases are GitHub releases of `reka18/codesmith`: one
//! `codesmith-<version>-<target>.tar.gz` per target, a `SHA256SUMS` file
//! and `SHA256SUMS.minisig`, its minisign signature. A copy installed from a
//! tarball (`install.sh` or by hand) replaces itself: the signature must
//! verify with a key compiled in from `release_key.pub` and name the version
//! asked for, the tarball is checked against `SHA256SUMS`, the binary in it
//! must report the expected version, and it is renamed over the running
//! one, so the old binary stays whole until the new one is in place. A copy
//! a package manager installed (Homebrew, `cargo install`) is never touched:
//! the command names the package manager's own command instead.
//!
//! Every request is a plain GET with a `codesmith/<version>` user agent and
//! no query string, cookies or credentials. Update checks are off unless the
//! global config opts in (OpenCode checks by default).

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Where releases live; `$CODESMITH_RELEASES_URL` overrides it (mirrors,
/// testing), as `$CODESMITH_BASE_URL` does for `install.sh`.
pub const RELEASES: &str = "https://github.com/reka18/codesmith/releases";
const BREW: &str = "brew upgrade reka18/tap/codesmith";
const CARGO: &str = "cargo install --locked --git https://github.com/reka18/codesmith codesmith";
const MAX_SUMS: usize = 64 * 1024;
const MAX_SIGNATURE: usize = 4 * 1024;
const MAX_TARBALL: usize = 256 * 1024 * 1024;
const MAX_UNPACKED: u64 = 512 * 1024 * 1024;

/// This build's release target, if releases are built for it.
pub const TARGET: Option<&str> = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    Some("aarch64-apple-darwin")
} else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
    Some("x86_64-apple-darwin")
} else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    Some("x86_64-unknown-linux-musl")
} else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
    Some("aarch64-unknown-linux-musl")
} else {
    None
};

// ---------------------------------------------------------------------------
// Versions and the update policy (updater-action.ts).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    Disable,
    Notify,
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    Notify,
    Auto,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Release {
    pub core: String,
    pub prerelease: Vec<String>,
}

/// The `update` preference in a config file's text (`"disable"`, `"notify"`
/// or `"auto"`), or OpenCode's older `autoupdate` (`false`, `"notify"`,
/// `true`). `None` when the file is not valid JSONC or doesn't say.
pub fn decode_policy(text: &str) -> Option<Policy> {
    let value: serde_json::Value =
        serde_json::from_str(&codesmith_core::config::strip_jsonc(text)).ok()?;
    let object = value.as_object()?;
    if let Some(v) = object.get("update") {
        return match v.as_str()? {
            "disable" => Some(Policy::Disable),
            "notify" => Some(Policy::Notify),
            "auto" => Some(Policy::Auto),
            _ => None,
        };
    }
    match object.get("autoupdate")? {
        serde_json::Value::Bool(false) => Some(Policy::Disable),
        serde_json::Value::Bool(true) => Some(Policy::Auto),
        serde_json::Value::String(s) if s == "notify" => Some(Policy::Notify),
        _ => None,
    }
}

/// What to do about `latest` when `current` is running: anything but the
/// same release (an older one too, for a rollback) is reported.
pub fn action(current: &str, latest: &str, policy: Policy) -> Action {
    if policy == Policy::Disable {
        return Action::None;
    }
    match (
        parse_release_version(current),
        parse_release_version(latest),
    ) {
        (Some(c), Some(l)) if !same_release(&c, &l) => match policy {
            Policy::Notify => Action::Notify,
            _ => Action::Auto,
        },
        _ => Action::None,
    }
}

/// A strict semver version, with an optional leading `v` and surrounding
/// whitespace, at most 256 characters before trimming, each number within
/// JavaScript's safe integers (as OpenCode checks).
pub fn parse_release_version(input: &str) -> Option<Release> {
    if input.encode_utf16().count() > 256 {
        return None;
    }
    let s = input.trim();
    let s = s.strip_prefix('v').unwrap_or(s);
    let (rest, build) = match s.split_once('+') {
        Some((r, b)) => (r, Some(b)),
        None => (s, None),
    };
    if let Some(b) = build {
        if !identifiers(b) {
            return None;
        }
    }
    let (core, pre) = match rest.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (rest, None),
    };
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3 || parts.iter().any(|p| invalid_component(p)) {
        return None;
    }
    let prerelease: Vec<String> = match pre {
        Some(p) => {
            if !identifiers(p) {
                return None;
            }
            let ids: Vec<String> = p.split('.').map(str::to_string).collect();
            if ids
                .iter()
                .any(|i| i.len() > 1 && i.starts_with('0') && numeric(i))
            {
                return None;
            }
            ids
        }
        None => Vec::new(),
    };
    Some(Release {
        core: core.to_string(),
        prerelease,
    })
}

fn numeric(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn identifiers(s: &str) -> bool {
    s.split('.')
        .all(|i| !i.is_empty() && i.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
}

fn invalid_component(value: &str) -> bool {
    const MAX: &str = "9007199254740991";
    if !numeric(value) || (value.len() > 1 && value.starts_with('0')) {
        return true;
    }
    if value.len() != MAX.len() {
        return value.len() > MAX.len();
    }
    value > MAX
}

fn same_release(a: &Release, b: &Release) -> bool {
    a.core == b.core
        && a.prerelease.len() == b.prerelease.len()
        && a.prerelease.iter().zip(&b.prerelease).all(|(x, y)| {
            // semver compares oversized numeric identifiers as JavaScript
            // numbers, so two that round to the same double are equal.
            x == y || (numeric(x) && numeric(y) && x.parse::<f64>().ok() == y.parse::<f64>().ok())
        })
}

/// Whether release `a` comes before release `b` in semver order: the
/// numbers first, then a prerelease before its release, prerelease
/// identifiers compared in turn (numbers numerically and before words,
/// words in ASCII order), a shorter list first when the rest agree. Equal
/// releases ([`same_release`]) are not older.
fn older(a: &Release, b: &Release) -> bool {
    use std::cmp::Ordering;
    if same_release(a, b) {
        return false;
    }
    // The numbers fit JavaScript's safe integers (parse_release_version).
    let nums = |r: &Release| -> Vec<u64> {
        r.core
            .split('.')
            .map(|p| p.parse().unwrap_or(u64::MAX))
            .collect()
    };
    match nums(a).cmp(&nums(b)) {
        Ordering::Less => return true,
        Ordering::Greater => return false,
        Ordering::Equal => {}
    }
    match (a.prerelease.is_empty(), b.prerelease.is_empty()) {
        (true, true) => return false,
        (true, false) => return false,
        (false, true) => return true,
        (false, false) => {}
    }
    for (x, y) in a.prerelease.iter().zip(&b.prerelease) {
        let o = match (numeric(x), numeric(y)) {
            (true, true) => {
                let (fx, fy) = (
                    x.parse::<f64>().unwrap_or(f64::MAX),
                    y.parse::<f64>().unwrap_or(f64::MAX),
                );
                fx.partial_cmp(&fy).unwrap_or(Ordering::Equal)
            }
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (false, false) => x.as_bytes().cmp(y.as_bytes()),
        };
        if o != Ordering::Equal {
            return o == Ordering::Less;
        }
    }
    a.prerelease.len() < b.prerelease.len()
}

/// Whether `version` is an older release than `current` (both valid).
pub fn is_older(version: &str, current: &str) -> bool {
    match (
        parse_release_version(version),
        parse_release_version(current),
    ) {
        (Some(v), Some(c)) => older(&v, &c),
        _ => false,
    }
}

fn bare(version: &str) -> &str {
    let v = version.trim();
    v.strip_prefix('v').unwrap_or(v)
}

// ---------------------------------------------------------------------------
// How this copy was installed.

/// How to upgrade: replace the binary from a release tarball, or name a
/// package manager's command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Method {
    /// A release tarball (install.sh or a manual download): replace the binary.
    Binary,
    /// Homebrew: print `brew upgrade`.
    Brew,
    /// cargo install: print the `cargo install` command.
    Cargo,
}

impl std::fmt::Display for Method {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Method::Binary => "binary",
            Method::Brew => "brew",
            Method::Cargo => "cargo",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detected {
    Method(Method),
    /// Built from a source checkout (`target/debug`, `target/release`).
    Source,
    /// Somewhere a system package manager owns (`/usr/bin`, the Nix store,
    /// ...): never replaced, whatever method is asked for.
    System,
    /// The running executable could not be found.
    Unknown,
}

/// Classify an executable by where it lives. `cargo_bins` are the folders
/// `cargo install` puts binaries in (`$CARGO_HOME/bin`, `~/.cargo/bin`).
pub fn detect(exe: &Path, cargo_bins: &[PathBuf]) -> Detected {
    let names: Vec<&str> = exe
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    if names
        .windows(2)
        .any(|w| w[0] == "Cellar" && w[1] == "codesmith")
    {
        return Detected::Method(Method::Brew);
    }
    let parent = exe.parent().unwrap_or(Path::new(""));
    if cargo_bins
        .iter()
        .any(|b| b.as_path() == parent || b.canonicalize().ok().as_deref() == Some(parent))
    {
        return Detected::Method(Method::Cargo);
    }
    let n = names.len();
    let built = |i: usize| matches!(names[i], "debug" | "release" | "deps");
    if (n >= 3 && built(n - 2) && names[..n - 2].contains(&"target"))
        || (n >= 4 && built(n - 3) && names[n - 2] == "deps")
    {
        return Detected::Source;
    }
    const SYSTEM: &[&str] = &[
        "/usr/bin/",
        "/usr/sbin/",
        "/bin/",
        "/sbin/",
        "/usr/lib/",
        "/usr/libexec/",
        "/usr/share/",
        "/nix/",
        "/gnu/store/",
        "/snap/",
        "/opt/local/",
        "/var/lib/flatpak/",
    ];
    let s = exe.to_string_lossy();
    if SYSTEM.iter().any(|p| s.starts_with(p)) {
        return Detected::System;
    }
    Detected::Method(Method::Binary)
}

/// The running executable, symlinks resolved, and how it was installed.
pub fn this_copy() -> (Option<PathBuf>, Detected) {
    let Some(exe) = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok())
    else {
        return (None, Detected::Unknown);
    };
    let detected = detect(&exe, &cargo_bins());
    (Some(exe), detected)
}

/// Where `cargo install` puts binaries.
fn cargo_bins() -> Vec<PathBuf> {
    let mut bins = Vec::new();
    if let Some(c) = std::env::var_os("CARGO_HOME") {
        bins.push(PathBuf::from(c).join("bin"));
    }
    if let Some(h) = std::env::var_os("HOME") {
        bins.push(PathBuf::from(h).join(".cargo").join("bin"));
    }
    bins
}

fn managed(method: Method) -> Option<String> {
    match method {
        Method::Brew => Some(format!(
            "CodeSmith is managed by Homebrew, which keeps its own record of the installed version, so CodeSmith won't update itself.\nRun: {BREW}"
        )),
        Method::Cargo => Some(format!(
            "CodeSmith was installed with cargo install, so CodeSmith won't update itself.\nRun: {CARGO}"
        )),
        Method::Binary => None,
    }
}

const SOURCE: &str = "This build runs from a source checkout. Rebuild it, or install a release to use codesmith upgrade.";
const SYSTEM_OWNED: &str = "This copy is in a folder a system package manager owns, so CodeSmith won't update itself. Update it with that package manager.";
const UNKNOWN: &str = "Could not detect the installation method. If a package manager installed this copy, update it with that package manager; if you installed it from a release tarball, pass --method binary.";

// ---------------------------------------------------------------------------
// Release signatures.

/// The minisign public keys that sign releases (`release_key.pub`).
const RELEASE_KEYS: &str = include_str!("release_key.pub");

/// Printed by an upgrade while [`RELEASE_KEYS`] is still the placeholder.
pub(crate) const UNSIGNED: &str =
    "warning: CodeSmith releases aren't signed yet; checking SHA256SUMS only";

/// Which signatures an upgrade accepts.
pub(crate) enum Trust {
    /// No release key has been generated yet: checksums only, as before
    /// releases were signed.
    Unsigned,
    /// `SHA256SUMS.minisig` must verify with one of these keys.
    Keys(Vec<minisign_verify::PublicKey>),
}

/// The keys this build trusts.
pub(crate) fn release_keys() -> Result<Trust> {
    parse_keys(RELEASE_KEYS)
}

/// A key file: one base64 minisign public key per line, `#` comments, and
/// `PLACEHOLDER` alone for "no key yet". Anything else is an error, so a
/// damaged file refuses upgrades instead of turning signatures off.
pub(crate) fn parse_keys(text: &str) -> Result<Trust> {
    let mut keys = Vec::new();
    let mut placeholder = false;
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "PLACEHOLDER" {
            placeholder = true;
            continue;
        }
        keys.push(
            minisign_verify::PublicKey::from_base64(line)
                .map_err(|e| anyhow!("bad release key {line:?}: {e}"))?,
        );
    }
    match (placeholder, keys.is_empty()) {
        (true, true) => Ok(Trust::Unsigned),
        (false, false) => Ok(Trust::Keys(keys)),
        (true, false) => bail!("the release key list has both keys and PLACEHOLDER"),
        (false, true) => bail!("this build has no release key, so it cannot check a release"),
    }
}

/// Check that `signature` (a `.minisig` file) signs `sums` with one of
/// `keys`, and that its trusted comment, which the signature covers too,
/// says these are the checksums of `version`: `codesmith <version>
/// SHA256SUMS`, optionally followed by more words. An older release's
/// signed `SHA256SUMS` served as another version is refused.
pub(crate) fn verify_sums(
    keys: &[minisign_verify::PublicKey],
    sums: &[u8],
    signature: &[u8],
    version: &str,
) -> Result<()> {
    const NOTHING: &str = "nothing was changed";
    let text = std::str::from_utf8(signature)
        .map_err(|_| anyhow!("SHA256SUMS.minisig is not a minisign signature; {NOTHING}"))?;
    let signature = minisign_verify::Signature::decode(text)
        .map_err(|e| anyhow!("SHA256SUMS.minisig is not a minisign signature ({e}); {NOTHING}"))?;
    // Prehashed (BLAKE2b) signatures only, as minisign makes by default.
    if !keys
        .iter()
        .any(|k| k.verify(sums, &signature, false).is_ok())
    {
        bail!("SHA256SUMS is not signed by a CodeSmith release key; {NOTHING}");
    }
    let comment = signature.trusted_comment();
    let expected = format!("codesmith {version} SHA256SUMS");
    if comment != expected && !comment.starts_with(&format!("{expected} ")) {
        bail!("the signed SHA256SUMS is for {comment:?}, not codesmith {version}; {NOTHING}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The command (handlers/upgrade.ts).

/// What the command needs from an updater; [`Github`] is the real one.
pub(crate) trait Updater {
    fn method(&mut self) -> Detected;
    async fn latest(&mut self) -> Result<String>;
    async fn upgrade(&mut self, version: &str) -> Result<()>;
}

/// `codesmith upgrade [TARGET] [--method M] [--check]`.
pub(crate) async fn command(
    updater: &mut impl Updater,
    current: &str,
    target: Option<&str>,
    method: Option<Method>,
    check: bool,
    out: &mut dyn Write,
) -> Result<()> {
    if let Some(t) = target {
        if parse_release_version(t).is_none() {
            bail!("Invalid version: {t}");
        }
    }
    let method = match method {
        Some(m) => m,
        None => match updater.method() {
            Detected::Method(m) => m,
            Detected::Source => bail!(SOURCE),
            Detected::System => bail!(SYSTEM_OWNED),
            Detected::Unknown => bail!(UNKNOWN),
        },
    };
    writeln!(out, "Using method: {method}")?;
    if !check {
        if let Some(message) = managed(method) {
            bail!(message);
        }
    }
    let named = target.is_some();
    let target = match target {
        Some(t) => t.to_string(),
        None => updater.latest().await.context("Update check failed")?,
    };
    let version = bare(&target);
    // The latest release older than this one (a release pulled, or a
    // releases page that lies) is never moved to unasked: only a version
    // named on the command line can go back.
    if !named && is_older(version, current) {
        if check {
            writeln!(
                out,
                "CodeSmith {current} is newer than the latest release ({version}); nothing to upgrade"
            )?;
            return Ok(());
        }
        bail!(
            "the latest release, {version}, is older than this copy ({current}); not downgrading. To go back to it, name it: codesmith upgrade {version}"
        );
    }
    if action(current, version, Policy::Notify) == Action::None {
        writeln!(
            out,
            "CodeSmith upgrade skipped: {version} is already installed"
        )?;
        return Ok(());
    }
    if check {
        writeln!(out, "CodeSmith {version} is available (this is {current})")?;
        let how = managed(method)
            .and_then(|m| m.lines().last().map(str::to_string))
            .unwrap_or_else(|| "Run: codesmith upgrade".into());
        writeln!(out, "{how}")?;
        return Ok(());
    }
    writeln!(out, "From {current} → {version}")?;
    writeln!(out, "Upgrading...")?;
    match updater.upgrade(&target).await {
        Ok(()) => {
            writeln!(out, "Upgrade complete")?;
            use std::io::IsTerminal;
            let colour = colour_for(std::io::stdout().is_terminal());
            write!(out, "{}", banner(Ending::Ok, colour))?;
            Ok(())
        }
        Err(e) => {
            writeln!(out, "Upgrade failed")?;
            Err(e)
        }
    }
}

/// The banners every installer shows when it ends (install.sh and the
/// Homebrew formula carry the same text).
pub(crate) const BANNER_OK: &str = include_str!("../../../packaging/banner.txt");
pub(crate) const BANNER_FAILED: &str = include_str!("../../../packaging/banner-failed.txt");

/// Which way an install ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ending {
    Ok,
    Failed,
}

/// Whether a stream gets colour: a terminal, without `NO_COLOR`, and not a
/// dumb one.
pub(crate) fn colour_for(terminal: bool) -> bool {
    terminal
        && std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TERM").map_or(true, |t| t != "dumb")
}

/// The banner as CodeSmith shows it: with the Enter prompt's row left
/// empty (nothing waits), coloured like install.sh's when `colour` is set.
pub(crate) fn banner(ending: Ending, colour: bool) -> String {
    let text = match ending {
        Ending::Ok => BANNER_OK,
        Ending::Failed => BANNER_FAILED,
    };
    let lines: Vec<&str> = text.lines().collect();
    const FRAME: &str = "\x1b[38;5;240m";
    const RESET: &str = "\x1b[0m";
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        let n = i + 1;
        let line = if n == 21 { lines[1] } else { line };
        if !colour {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        let c = match (ending, n) {
            (Ending::Ok, 3) => "1",
            (Ending::Failed, 3) => "1;38;5;196",
            (Ending::Ok, 5) => "1;38;5;226",
            (Ending::Ok, 6) => "1;38;5;220",
            (Ending::Ok, 7) => "1;38;5;214",
            (Ending::Ok, 8) => "1;38;5;208",
            (Ending::Ok, 9) => "1;38;5;202",
            (Ending::Ok, 10) => "1;38;5;196",
            (Ending::Ok, 12) => "1;38;5;255",
            (Ending::Ok, 13) => "1;38;5;254",
            (Ending::Ok, 14) => "1;38;5;253",
            (Ending::Ok, 15) => "1;38;5;189",
            (Ending::Ok, 16) => "1;38;5;153",
            (Ending::Ok, 17) => "1;38;5;117",
            (Ending::Ok, 19) => "38;5;214",
            (Ending::Failed, 5) => "1;38;5;203",
            (Ending::Failed, 6) => "1;38;5;196",
            (Ending::Failed, 7) => "1;38;5;160",
            (Ending::Failed, 8) => "1;38;5;124",
            (Ending::Failed, 9 | 10) => "1;38;5;88",
            (Ending::Failed, 12) => "1;38;5;250",
            (Ending::Failed, 13) => "1;38;5;248",
            (Ending::Failed, 14) => "1;38;5;246",
            (Ending::Failed, 15) => "1;38;5;244",
            (Ending::Failed, 16) => "1;38;5;242",
            (Ending::Failed, 17) => "1;38;5;240",
            (Ending::Failed, 19) => "38;5;196",
            _ => "",
        };
        match line.strip_prefix('║').and_then(|l| l.strip_suffix('║')) {
            Some(inner) => {
                let on = if c.is_empty() {
                    String::new()
                } else {
                    format!("\x1b[{c}m")
                };
                out.push_str(&format!(
                    "{FRAME}║{RESET}{on}{inner}{RESET}{FRAME}║{RESET}\n"
                ));
            }
            None => out.push_str(&format!("{FRAME}{line}{RESET}\n")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// GitHub releases.

/// Upgrades `exe` from the releases at `base`.
pub(crate) struct Github {
    pub base: String,
    pub exe: Option<PathBuf>,
    pub detected: Detected,
    pub target: Option<&'static str>,
    pub current: String,
}

impl Github {
    pub fn new(current: &str) -> Self {
        let (exe, detected) = this_copy();
        Github {
            base: std::env::var("CODESMITH_RELEASES_URL")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| RELEASES.to_string()),
            exe,
            detected,
            target: TARGET,
            current: current.to_string(),
        }
    }
}

fn loopback(url: &str) -> bool {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .is_some_and(|h| h == "127.0.0.1" || h == "localhost" || h == "[::1]")
}

/// HTTPS only (redirects included), unless the releases are on loopback.
fn client(base: &str, current: &str, follow: bool) -> Result<reqwest::Client> {
    let local = loopback(base);
    if !local && !base.starts_with("https://") {
        bail!("the releases URL must use https: {base}");
    }
    reqwest::Client::builder()
        .user_agent(format!("codesmith/{current}"))
        .https_only(!local)
        .redirect(if follow {
            reqwest::redirect::Policy::limited(10)
        } else {
            reqwest::redirect::Policy::none()
        })
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(300))
        .build()
        .context("building the HTTP client")
}

/// The newest published release: `<base>/latest` redirects to
/// `<base>/tag/v<version>` (drafts and prereleases are never "latest").
pub(crate) async fn latest_release(base: &str, current: &str) -> Result<String> {
    let url = format!("{}/latest", base.trim_end_matches('/'));
    let response = client(base, current, false)?
        .get(&url)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .with_context(|| format!("could not reach {url}"))?;
    let status = response.status();
    if !status.is_redirection() {
        bail!("{url} returned HTTP {}", status.as_u16());
    }
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let tag = location
        .split_once("/tag/")
        .map(|(_, t)| t.trim_end_matches('/'))
        .unwrap_or_default();
    if parse_release_version(tag).is_none() {
        bail!("no published release found at {url}");
    }
    Ok(bare(tag).to_string())
}

async fn download(client: &reqwest::Client, url: &str, max: usize) -> Result<Vec<u8>> {
    let mut response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("could not download {url}"))?;
    if !response.status().is_success() {
        bail!(
            "download failed: {url} returned HTTP {}",
            response.status().as_u16()
        );
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .with_context(|| format!("could not download {url}"))?
    {
        if body.len() + chunk.len() > max {
            bail!("{url} is larger than {max} bytes");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

impl Updater for Github {
    fn method(&mut self) -> Detected {
        self.detected.clone()
    }

    async fn latest(&mut self) -> Result<String> {
        latest_release(&self.base, &self.current).await
    }

    async fn upgrade(&mut self, version: &str) -> Result<()> {
        let exe = self
            .exe
            .clone()
            .ok_or_else(|| anyhow!("could not find the running executable"))?;
        let trust = release_keys()?;
        if matches!(trust, Trust::Unsigned) {
            eprintln!("{UNSIGNED}");
        }
        install(
            &trust,
            &self.base,
            &self.current,
            self.target,
            &exe,
            version,
        )
        .await
    }
}

/// Removes the download folder however the upgrade ends.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Download `version` for `target` from `base`, check it, and put it in
/// place of `exe`. Refuses a copy a package manager owns, whatever method
/// was asked for. With [`Trust::Keys`], `SHA256SUMS` is used only once its
/// signature verifies, before the tarball is even downloaded.
pub(crate) async fn install(
    trust: &Trust,
    base: &str,
    current: &str,
    target: Option<&str>,
    exe: &Path,
    version: &str,
) -> Result<()> {
    if parse_release_version(version).is_none() {
        bail!("Invalid version: {version}");
    }
    match detect(exe, &cargo_bins()) {
        Detected::Method(Method::Binary) | Detected::Unknown => {}
        Detected::Method(m) => bail!(managed(m).unwrap_or_default()),
        Detected::Source => bail!(SOURCE),
        Detected::System => bail!(SYSTEM_OWNED),
    }
    let target =
        target.ok_or_else(|| anyhow!("no CodeSmith releases are built for this platform"))?;
    let version = bare(version);
    let name = format!("codesmith-{version}-{target}");
    let file = format!("{name}.tar.gz");
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent folder", exe.display()))?;

    let client = client(base, current, true)?;
    let release = format!("{}/download/v{version}", base.trim_end_matches('/'));
    let sums = download(&client, &format!("{release}/SHA256SUMS"), MAX_SUMS).await?;
    if let Trust::Keys(keys) = trust {
        let url = format!("{release}/SHA256SUMS.minisig");
        let signature = download(&client, &url, MAX_SIGNATURE).await.map_err(|e| {
            anyhow!(
                "could not get the release signature, SHA256SUMS.minisig ({e:#}); this build installs only signed releases, so nothing was changed"
            )
        })?;
        verify_sums(keys, &sums, &signature, version)?;
    }
    let expected = String::from_utf8_lossy(&sums)
        .lines()
        .find_map(|line| {
            let (hash, listed) = line.trim().split_once(char::is_whitespace)?;
            let listed = listed.trim_start();
            (listed == file || listed.strip_prefix('*') == Some(file.as_str()))
                .then(|| hash.to_ascii_lowercase())
        })
        .ok_or_else(|| anyhow!("{file} is not listed in SHA256SUMS"))?;
    let tarball = download(&client, &format!("{release}/{file}"), MAX_TARBALL).await?;
    let actual = hex(&Sha256::digest(&tarball));
    if actual != expected {
        bail!(
            "checksum mismatch for {file} (expected {expected}, got {actual}); nothing was changed"
        );
    }
    let binary = extract(&tarball, &format!("{name}/codesmith"))?;

    // Next to the executable, so the final rename stays on one filesystem.
    let scratch = dir.join(format!(".codesmith-upgrade-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    private_dir(&scratch).with_context(|| {
        format!(
            "cannot write to {}; reinstall to a folder you own, or run the upgrade with permission to write there",
            dir.display()
        )
    })?;
    let scratch = Scratch(scratch);
    let new = scratch.0.join("codesmith");
    {
        let mut f = private_file(&new)?;
        f.write_all(&binary)?;
        f.sync_all()?;
    }
    check_version(&new, version)?;
    // Checked: now readable and runnable by everyone, as an install is.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&new, exe).with_context(|| format!("could not replace {}", exe.display()))?;
    Ok(())
}

/// Create `path` as a directory only its owner can use (0700), whatever
/// the umask; it must not exist yet.
fn private_dir(path: &Path) -> std::io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        b.mode(0o700);
        b.create(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        b.create(path)
    }
}

/// Create `path` as a new file only its owner can read, write or run
/// (0700), whatever the umask.
fn private_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        o.mode(0o700);
        let f = o.open(path)?;
        f.set_permissions(std::fs::Permissions::from_mode(0o700))?;
        Ok(f)
    }
    #[cfg(not(unix))]
    {
        o.open(path)
    }
}

/// The downloaded binary must run here and call itself `version`.
fn check_version(binary: &Path, version: &str) -> Result<()> {
    let output = std::process::Command::new(binary)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("the downloaded binary did not run ({})", binary.display()))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut words = text.split_whitespace();
    let reported = match (words.next(), words.next()) {
        (Some("codesmith"), Some(v)) => v.to_string(),
        _ => String::new(),
    };
    let same = match (
        parse_release_version(&reported),
        parse_release_version(version),
    ) {
        (Some(a), Some(b)) => same_release(&a, &b),
        _ => false,
    };
    if !output.status.success() || !same {
        bail!(
            "the downloaded binary reports {:?}, not codesmith {version}; nothing was changed",
            text.trim()
        );
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The regular file `want` from a gzipped tar (ustar, pax and GNU long
/// names).
fn extract(tgz: &[u8], want: &str) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    flate2::read::GzDecoder::new(tgz)
        .take(MAX_UNPACKED)
        .read_to_end(&mut data)
        .context("the release tarball is not valid gzip")?;
    let field = |b: &[u8]| {
        let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        String::from_utf8_lossy(&b[..end]).into_owned()
    };
    let mut offset = 0;
    let mut long: Option<String> = None;
    while offset + 512 <= data.len() {
        let header = &data[offset..offset + 512];
        if header.iter().all(|&b| b == 0) {
            break;
        }
        let size_text = field(&header[124..136]);
        let size = usize::from_str_radix(size_text.trim(), 8)
            .map_err(|_| anyhow!("the release tarball has a bad header"))?;
        let start = offset + 512;
        let end = start
            .checked_add(size)
            .filter(|&e| e <= data.len())
            .ok_or_else(|| anyhow!("the release tarball is truncated"))?;
        let body = &data[start..end];
        let mut name = field(&header[0..100]);
        if &header[257..262] == b"ustar" {
            let prefix = field(&header[345..500]);
            if !prefix.is_empty() {
                name = format!("{prefix}/{name}");
            }
        }
        let name = long.take().unwrap_or(name);
        match header[156] {
            b'x' => long = pax_path(body),
            b'L' => long = Some(field(body)),
            b'0' | 0 if name.trim_start_matches("./") == want => return Ok(body.to_vec()),
            _ => {}
        }
        offset = start + size.div_ceil(512) * 512;
    }
    bail!("the release tarball does not contain {want}")
}

fn pax_path(body: &[u8]) -> Option<String> {
    let mut rest = body;
    let mut path = None;
    while !rest.is_empty() {
        let space = rest.iter().position(|&b| b == b' ')?;
        let len: usize = std::str::from_utf8(&rest[..space]).ok()?.parse().ok()?;
        if len <= space || len > rest.len() {
            return None;
        }
        let record = std::str::from_utf8(&rest[space + 1..len]).ok()?;
        if let Some(p) = record.trim_end_matches('\n').strip_prefix("path=") {
            path = Some(p.to_string());
        }
        rest = &rest[len..];
    }
    path
}

// ---------------------------------------------------------------------------
// The opt-in update check.

/// The update policy: `update` (or `autoupdate`) in the global
/// `codesmith.json[c]` only, so a project can't turn on network requests;
/// off when neither says, or when `$CODESMITH_DISABLE_AUTOUPDATE` is set.
pub fn policy(config_dir: &Path) -> Policy {
    if std::env::var("CODESMITH_DISABLE_AUTOUPDATE")
        .is_ok_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true"))
    {
        return Policy::Disable;
    }
    codesmith_core::config::NAMES
        .iter()
        .filter_map(|n| std::fs::read_to_string(config_dir.join(n)).ok())
        .filter_map(|t| decode_policy(&t))
        .next_back()
        .unwrap_or(Policy::Disable)
}

const CHECK_EVERY: u64 = 24 * 60 * 60;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn read_cache(path: &Path) -> Option<(u64, String)> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    Some((v["checked"].as_u64()?, v["latest"].as_str()?.to_string()))
}

/// A started check; [`Check::notice`] reads its result.
pub struct Check {
    current: String,
    method: Method,
    cached: Option<String>,
    task: Option<tokio::task::JoinHandle<Option<String>>>,
}

/// Start the update check if the global config opts in: at most once a
/// day (the result is kept in the cache folder), never for a source build.
pub fn start(current: &str) -> Option<Check> {
    if policy(&codesmith_core::paths::config_dir()) == Policy::Disable {
        return None;
    }
    let Detected::Method(method) = this_copy().1 else {
        return None;
    };
    let base = std::env::var("CODESMITH_RELEASES_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| RELEASES.to_string());
    Some(start_with(
        current,
        method,
        &base,
        &codesmith_core::paths::cache_dir().join("update-check.json"),
        now(),
    ))
}

pub(crate) fn start_with(
    current: &str,
    method: Method,
    base: &str,
    cache: &Path,
    now: u64,
) -> Check {
    let cached = read_cache(cache);
    let fresh = cached
        .as_ref()
        .is_some_and(|(at, _)| *at <= now && now - at < CHECK_EVERY);
    let task = (!fresh).then(|| {
        let (base, current, cache) = (base.to_string(), current.to_string(), cache.to_path_buf());
        tokio::spawn(async move {
            let latest = latest_release(&base, &current).await.ok()?;
            let _ = codesmith_core::paths::ensure_parent(&cache);
            let _ = std::fs::write(
                &cache,
                serde_json::json!({"checked": now, "latest": latest}).to_string(),
            );
            Some(latest)
        })
    });
    Check {
        current: current.to_string(),
        method,
        cached: cached.map(|(_, l)| l),
        task,
    }
}

impl Check {
    /// A line to print if another release is out; never waits for the
    /// network (an unfinished check falls back to the last result).
    pub async fn notice(self) -> Option<String> {
        let latest = match self.task {
            Some(t) if t.is_finished() => t.await.ok().flatten().or(self.cached),
            Some(t) => {
                t.abort();
                self.cached
            }
            None => self.cached,
        }?;
        if action(&self.current, &latest, Policy::Notify) == Action::None
            || is_older(&latest, &self.current)
        {
            return None;
        }
        let how = match self.method {
            Method::Binary => "codesmith upgrade",
            Method::Brew => BREW,
            Method::Cargo => CARGO,
        };
        Some(format!(
            "CodeSmith {} is available (this is {}). Run: {how}",
            bare(&latest),
            self.current
        ))
    }
}

#[cfg(test)]
mod tests;
