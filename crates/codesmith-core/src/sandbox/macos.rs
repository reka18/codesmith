//! macOS: each command runs under `/usr/bin/sandbox-exec -p <profile>`,
//! with a Seatbelt profile made for it from the [`Policy`]. Folders reach
//! the profile as `-D` parameters (`(param "W0")`), never as profile text;
//! only the protected patterns, which need regular expressions, are
//! written into it, regex-escaped and string-escaped by
//! [`sbpl_string`]. Nothing from a command line goes into a profile.
//!
//! Compiled on every platform so the profile can be tested anywhere; only
//! macOS runs it.

use super::{has_wildcard, Network, Policy, Support};
use std::ffi::OsString;
use std::path::Path;

pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Mach services for the network, allowed only with `network: all`:
/// DNS, network configuration and the network daemon.
const MACH_NETWORK: &[&str] = &[
    "com.apple.dnssd.service",
    "com.apple.networkd",
    "com.apple.nehelper",
    "com.apple.nesessionmanager",
    "com.apple.SystemConfiguration.DNSConfiguration",
    "com.apple.SystemConfiguration.configd",
    "com.apple.symptomsd",
    "com.apple.usymptomsd",
];

/// `s` as a Seatbelt string literal: quoted, with `\` and `"` escaped.
/// Control characters can't appear in one, so a value holding any is
/// refused (`None`).
pub fn sbpl_string(s: &str) -> Option<String> {
    if s.chars().any(char::is_control) {
        return None;
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '\\' || c == '"' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    Some(out)
}

/// A permission pattern as an anchored regular expression: `*` is any
/// run of characters, `?` one, everything else literal. A pattern without
/// a wildcard covers the path and everything under it.
pub fn pattern_regex(p: &str) -> String {
    let mut re = String::from("^");
    let mut lit = String::new();
    let flush = |lit: &mut String, re: &mut String| {
        re.push_str(&regex::escape(lit));
        lit.clear();
    };
    for c in p.chars() {
        match c {
            '*' => {
                flush(&mut lit, &mut re);
                re.push_str(".*");
            }
            '?' => {
                flush(&mut lit, &mut re);
                re.push('.');
            }
            c => lit.push(c),
        }
    }
    flush(&mut lit, &mut re);
    if !has_wildcard(p) {
        re.push_str("(/.*)?");
    }
    re.push('$');
    re
}

/// A filter for a pattern: `(subpath …)` for a plain path, `(regex …)`
/// otherwise. `None` when it can't be written safely.
fn filter(p: &str, params: &mut Vec<(String, String)>, prefix: &str) -> Option<String> {
    if has_wildcard(p) {
        Some(format!("(regex {})", sbpl_string(&pattern_regex(p))?))
    } else {
        Some(format!("(subpath {})", param(params, prefix, p)?))
    }
}

/// Add `value` as a `-D` parameter and return the expression naming it.
fn param(params: &mut Vec<(String, String)>, prefix: &str, value: &str) -> Option<String> {
    if value.contains('\0') {
        return None;
    }
    let key = format!(
        "{prefix}{}",
        params.iter().filter(|(k, _)| k.starts_with(prefix)).count()
    );
    params.push((key.clone(), value.to_string()));
    Some(format!("(param {})", sbpl_string(&key)?))
}

/// The profile for `policy` and the parameters it reads. A path that
/// can't be expressed is left out of the write list (a refusal, never a
/// grant); a protected pattern that can't be makes the whole profile
/// fail, since leaving it out would allow it.
pub fn profile(policy: &Policy) -> Result<(String, Vec<(String, String)>), String> {
    let mut params: Vec<(String, String)> = Vec::new();
    let mut p = String::new();
    p.push_str(BASE);
    // Writes.
    let mut writes = Vec::new();
    for w in &policy.write {
        let s = w.display().to_string();
        if let Some(f) = param(&mut params, "W", &s) {
            writes.push(format!("(subpath {f})"));
        }
    }
    if !writes.is_empty() {
        p.push_str(&format!("(allow file-write*\n  {})\n", writes.join("\n  ")));
    }
    // Protected: never read or written, whatever was allowed above.
    let mut prot = Vec::new();
    for pat in &policy.protected {
        prot.push(
            filter(pat, &mut params, "P")
                .ok_or_else(|| format!("can't express the protected path {pat:?}"))?,
        );
    }
    if !prot.is_empty() {
        p.push_str(&format!(
            "(deny file-read* file-write*\n  {})\n",
            prot.join("\n  ")
        ));
    }
    let mut carve = Vec::new();
    for pat in &policy.readable {
        if let Some(f) = filter(pat, &mut params, "R") {
            carve.push(f);
        }
    }
    if !carve.is_empty() {
        p.push_str(&format!("(allow file-read*\n  {})\n", carve.join("\n  ")));
    }
    // The network.
    match policy.network {
        Network::All | Network::Allowlist => {
            p.push_str("(allow network*)\n(allow system-socket)\n");
            p.push_str("(allow mach-lookup\n");
            for m in MACH_NETWORK {
                p.push_str(&format!("  (global-name \"{m}\")\n"));
            }
            p.push_str(")\n");
        }
        Network::None => {
            // Local Unix sockets: agents, the shell daemon, a test's own.
            p.push_str(
                "(allow network-outbound (remote unix-socket))\n\
                 (allow network-bind network-inbound (local unix-socket))\n\
                 (allow system-socket (socket-domain AF_UNIX))\n",
            );
        }
    }
    // The agent sockets, by name, whatever the network mode.
    let mut socks = Vec::new();
    for s in &policy.sockets {
        if let Some(f) = param(&mut params, "S", &s.display().to_string()) {
            socks.push(format!("(remote unix-socket (path-literal {f}))"));
        }
    }
    if !socks.is_empty() {
        p.push_str(&format!(
            "(allow network-outbound\n  {})\n",
            socks.join("\n  ")
        ));
    }
    Ok((p, params))
}

/// What every profile starts with: closed by default; programs may run
/// and fork (a setuid program can't: the sandbox refuses it, which is
/// what keeps `sudo` out; `/bin/ps`, setuid on macOS and only printing,
/// runs outside the sandbox so it can see processes), signal and inspect their own sandbox's processes, read
/// files, use pipes and ptys, and look up a few Mach services: user and
/// group lookups, notifications, logging, preferences (read; `cfprefsd`
/// checks a sandboxed client's writes itself), the security server for
/// TLS trust, FSEvents for file watchers, and power assertions. Not Launch
/// Services or Apple Events, through which a command could start a
/// program outside the sandbox.
const BASE: &str = r#"(version 1)
(deny default)
(allow process-exec process-fork)
(allow process-exec (literal "/bin/ps") (with no-sandbox))
(allow signal (target same-sandbox))
(allow process-info* (target same-sandbox))
(allow process-info-pidinfo process-info-setcontrol)
(allow file-read*)
(allow file-write-data file-ioctl
  (literal "/dev/null")
  (literal "/dev/zero")
  (literal "/dev/tty")
  (literal "/dev/stdout")
  (literal "/dev/stderr")
  (literal "/dev/dtracehelper")
  (regex #"^/dev/fd/[0-9]+$"))
(allow pseudo-tty)
(allow file-read* file-write* file-ioctl (literal "/dev/ptmx") (regex #"^/dev/ttys[0-9]+$"))
(allow sysctl-read)
(allow sysctl-write (sysctl-name "kern.grade_cputype"))
(allow iokit-open (iokit-registry-entry-class "RootDomainUserClient"))
(allow ipc-posix-sem)
(allow ipc-posix-shm)
(allow user-preference-read)
(allow mach-lookup
  (global-name "com.apple.system.opendirectoryd.libinfo")
  (global-name "com.apple.system.opendirectoryd.membership")
  (global-name "com.apple.system.notification_center")
  (global-name "com.apple.system.logger")
  (global-name "com.apple.logd")
  (global-name "com.apple.diagnosticd")
  (global-name "com.apple.cfprefsd.daemon")
  (global-name "com.apple.cfprefsd.agent")
  (local-name "com.apple.cfprefsd.agent")
  (global-name "com.apple.bsd.dirhelper")
  (global-name "com.apple.SecurityServer")
  (global-name "com.apple.trustd")
  (global-name "com.apple.trustd.agent")
  (global-name "com.apple.ocspd")
  (global-name "com.apple.FSEvents")
  (global-name "com.apple.PowerManagement.control")
  (global-name "com.apple.coreservices.quarantine-resolver"))
"#;

/// The program and arguments that run `program args…` in the sandbox.
pub fn wrap(
    policy: &Policy,
    program: &Path,
    args: &[String],
) -> Result<(String, Vec<String>), String> {
    let (profile, params) = profile(policy)?;
    let mut out: Vec<String> = Vec::new();
    for (k, v) in params {
        out.push("-D".into());
        out.push(format!("{k}={v}"));
    }
    out.push("-p".into());
    out.push(profile);
    out.push(program.display().to_string());
    out.extend(args.iter().cloned());
    Ok((SANDBOX_EXEC.to_string(), out))
}

/// [`wrap`] as OS strings, for `std::process::Command`.
pub fn wrap_os(
    policy: &Policy,
    program: &Path,
    args: &[String],
) -> Result<(OsString, Vec<OsString>), String> {
    let (p, a) = wrap(policy, program, args)?;
    Ok((p.into(), a.into_iter().map(OsString::from).collect()))
}

/// Whether `sandbox-exec` is there and applies a profile here (it fails
/// inside another sandbox, for one).
pub fn probe() -> Support {
    if !Path::new(SANDBOX_EXEC).is_file() {
        return Support::No(format!("{SANDBOX_EXEC} is missing"));
    }
    let out = std::process::Command::new(SANDBOX_EXEC)
        .args(["-p", "(version 1)\n(allow default)\n", "/usr/bin/true"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output();
    match out {
        Ok(o) if o.status.success() => Support::Yes,
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
            Support::No(if err.is_empty() {
                format!("{SANDBOX_EXEC} failed ({})", o.status)
            } else {
                format!("{SANDBOX_EXEC} failed: {err}")
            })
        }
        Err(e) => Support::No(format!("can't run {SANDBOX_EXEC}: {e}")),
    }
}
