//! The sandbox's own tests: its settings and decisions, the profile and
//! path-cover building blocks (with bounded property tests), reading a
//! refusal from a command's output, and, through the `shell` tool, the
//! escapes it exists to stop, the protected paths, the network, the
//! failure-to-prompt rerun, unsupported machines and the compatibility
//! that must survive it.
//!
//! The tests that run commands check this machine's [`support`] first: on
//! one that can't sandbox (a container or kernel without Landlock) they
//! assert the refusal instead, so they hold on any CI runner.

use super::*;
use crate::permission::{Effect, Reply, Request, Rule};
use crate::tool::shell::{run, Jobs};
use crate::tool::testing::{Env, Recorder};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

// ---------- settings and decisions ----------

#[test]
fn defaults_are_on_with_the_network_open_and_caches_writable() {
    let s = Settings::default();
    assert!(s.enabled());
    assert_eq!(s.network(), Network::All);
    assert_eq!(s.unsupported(), Unsupported::Refuse);
    assert_eq!(decide(&s, &Support::Yes), Decision::Enforce);
    let home = dirs::home_dir().unwrap();
    let caches = cache_dirs();
    assert!(caches.iter().any(|c| c.starts_with(&home)), "{caches:?}");
    let p = policy(&Inputs {
        project: None,
        session_id: "defaults",
        settings: &s,
        grants: &[],
        rules: &[],
        env: None,
    })
    .unwrap();
    assert_eq!(p.network, Network::All);
    for c in &caches {
        assert!(p.writes(c), "{} writable: {:?}", c.display(), p.write);
    }
    let off = Settings {
        caches: Some(false),
        ..Default::default()
    };
    let p = policy(&Inputs {
        project: None,
        session_id: "defaults",
        settings: &off,
        grants: &[],
        rules: &[],
        env: None,
    })
    .unwrap();
    for c in &caches {
        assert!(!p.writes(c), "{} not writable", c.display());
    }
}

#[test]
fn settings_parse_from_config_json() {
    let s: Settings = serde_json::from_value(json!({
        "enabled": false,
        "network": "none",
        "allow": ["internal.example"],
        "write": ["~/scratch"],
        "caches": false,
        "unsupported": "run"
    }))
    .unwrap();
    assert_eq!(s.enabled, Some(false));
    assert_eq!(s.network(), Network::None);
    assert_eq!(s.write, vec!["~/scratch".to_string()]);
    assert_eq!(s.caches, Some(false));
    assert_eq!(s.unsupported(), Unsupported::Run);
    // A mode that doesn't exist is an error, not "all".
    assert!(serde_json::from_value::<Settings>(json!({"network": "some"})).is_err());
    assert!(serde_json::from_value::<Settings>(json!({"unsupported": "maybe"})).is_err());
}

#[test]
fn each_decision_says_why_and_what_to_change() {
    let no = Support::No("no Landlock".into());
    match decide(&Settings::default(), &no) {
        Decision::Refuse { message } => {
            assert!(message.contains("no Landlock"), "{message}");
            assert!(message.contains(RUN_SETTING), "{message}");
        }
        d => panic!("{d:?}"),
    }
    let run = Settings {
        unsupported: Some(Unsupported::Run),
        ..Default::default()
    };
    assert!(matches!(decide(&run, &no), Decision::Unsandboxed { .. }));
    // "run" only matters where the sandbox can't work.
    assert_eq!(decide(&run, &Support::Yes), Decision::Enforce);
    let off = Settings {
        enabled: Some(false),
        ..Default::default()
    };
    for support in [Support::Yes, no.clone()] {
        match decide(&off, &support) {
            Decision::Unsandboxed { notice } => assert!(notice.contains("enabled"), "{notice}"),
            d => panic!("{d:?}"),
        }
    }
    let allowlist = Settings {
        network: Some(Network::Allowlist),
        ..Default::default()
    };
    match decide(&allowlist, &Support::Yes) {
        Decision::Refuse { message } => assert!(message.contains("allowlist"), "{message}"),
        d => panic!("{d:?}"),
    }
}

/// A repository can't set `sandbox.*`: CodeSmith reads no config file
/// from it. A per-repo file in the global folder can.
#[test]
fn a_repository_cannot_set_the_sandbox_but_a_per_repo_file_can() {
    let global = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let root = project.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let off = r#"{"sandbox": {"enabled": false, "unsupported": "run", "write": ["/"]}}"#;
    for f in [
        "codesmith.json",
        "codesmith.jsonc",
        ".codesmith/codesmith.json",
        ".codesmith/codesmith.jsonc",
        "opencode.json",
        ".opencode/opencode.json",
    ] {
        let p = root.join(f);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, off).unwrap();
    }
    let sources = crate::config::Sources {
        global_dir: global.path().to_path_buf(),
        file: None,
        content: None,
        project: true,
        per_repo: true,
    };
    let loaded = crate::config::load_from(&root, &sources).unwrap();
    assert_eq!(loaded.config.sandbox, Settings::default());
    // The per-repo file in the global folder is your own config.
    let dir = global.path().join(crate::per_repo::DIR);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("p.json"),
        format!(
            r#"{{"path": {}, "sandbox": {{"enabled": false}}}}"#,
            serde_json::to_string(&root.display().to_string()).unwrap()
        ),
    )
    .unwrap();
    let loaded = crate::config::load_from(&root, &sources).unwrap();
    assert_eq!(
        loaded.config.sandbox.enabled,
        Some(false),
        "{:?}",
        loaded.warnings
    );
}

// ---------- profile building blocks ----------

#[test]
fn sbpl_strings_escape_quotes_and_backslashes_and_refuse_control_characters() {
    assert_eq!(macos::sbpl_string("/a b").unwrap(), r#""/a b""#);
    assert_eq!(macos::sbpl_string(r#"/a"b\c"#).unwrap(), r#""/a\"b\\c""#);
    assert_eq!(macos::sbpl_string("/a\nb"), None);
    assert_eq!(macos::sbpl_string("/a\0b"), None);
}

/// A tiny deterministic generator, so the property tests are bounded and
/// repeatable without a dependency.
struct Gen(u64);

impl Gen {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn pick<'a>(&mut self, of: &'a [&'a str]) -> &'a str {
        of[(self.next() % of.len() as u64) as usize]
    }
    fn path(&mut self, wild: bool) -> String {
        const PARTS: &[&str] = &[
            "a", "b", ".ssh", "x y", "q\"t", "back\\s", "é", "(p)", "[c]", "$d", ".", "a.b",
        ];
        let n = 1 + self.next() % 4;
        let mut s = String::new();
        for _ in 0..n {
            s.push('/');
            s.push_str(self.pick(PARTS));
            if wild && self.next().is_multiple_of(5) {
                s.push(if self.next().is_multiple_of(2) {
                    '*'
                } else {
                    '?'
                });
            }
        }
        s
    }
}

/// Property: the Seatbelt regex for a pattern matches exactly the paths
/// the pattern covers, and survives SBPL quoting.
#[test]
fn pattern_regexes_match_what_patterns_cover() {
    let mut g = Gen(0x5eed_1234);
    for _ in 0..400 {
        let pat = g.path(true);
        let re = regex::Regex::new(&macos::pattern_regex(&pat)).unwrap();
        assert!(macos::sbpl_string(&macos::pattern_regex(&pat)).is_some());
        for _ in 0..20 {
            let path = if g.next().is_multiple_of(3) {
                format!("{}{}", pat.replace(['*', '?'], "z"), g.path(false))
            } else {
                g.path(false)
            };
            assert_eq!(
                re.is_match(&path),
                covers(&pat, &path),
                "pattern {pat:?} path {path:?}"
            );
        }
    }
}

#[test]
fn a_profile_never_holds_a_folder_as_text() {
    let p = Policy {
        write: vec![PathBuf::from("/w/\"); (allow default) ;")],
        protected: vec!["/p/sec\"ret*".into(), "/q".into()],
        readable: vec!["/q/ok".into()],
        sockets: vec![PathBuf::from("/s/sock")],
        tmpdir: PathBuf::from("/t"),
        network: Network::None,
    };
    let (text, params) = macos::profile(&p).unwrap();
    assert!(!text.contains("(allow default)"), "{text}");
    assert!(!text.contains("/w/"), "{text}");
    assert!(params.iter().any(|(_, v)| v.starts_with("/w/")));
    // Network none: no blanket network allow.
    assert!(!text.contains("(allow network*)"), "{text}");
    // A protected pattern that can't be written fails the profile.
    let bad = Policy {
        protected: vec!["/p/a\nb*".into()],
        ..p
    };
    assert!(macos::profile(&bad).is_err());
}

/// The escaping is the real thing: `sandbox-exec` accepts a profile whose
/// folders and patterns hold quotes, backslashes and spaces, and enforces
/// it.
#[cfg(target_os = "macos")]
#[test]
fn sandbox_exec_accepts_hostile_folder_names() {
    if !supported() {
        return;
    }
    let d = tempfile::tempdir().unwrap();
    let b = d.path().canonicalize().unwrap();
    let w = b.join("w \"q\" \\b (x)");
    let s = b.join("s \"q\"");
    std::fs::create_dir_all(&w).unwrap();
    std::fs::create_dir_all(&s).unwrap();
    std::fs::write(s.join("secret"), "s3cret").unwrap();
    let p = Policy {
        write: vec![w.clone()],
        protected: vec![format!("{}/sec*", s.display())],
        tmpdir: w.clone(),
        ..Default::default()
    };
    let (prog, args) = macos::wrap(
        &p,
        Path::new("/bin/sh"),
        &[
            "-c".into(),
            r#"echo ok > "$1/f"; cat "$2/secret"; echo x > "$3/nope""#.into(),
            "sh".into(),
            w.display().to_string(),
            s.display().to_string(),
            b.display().to_string(),
        ],
    )
    .unwrap();
    let out = std::process::Command::new(prog)
        .args(args)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(std::fs::read_to_string(w.join("f")).unwrap(), "ok\n");
    assert!(!text.contains("s3cret"), "{text}");
    assert!(!b.join("nope").exists());
}

// ---------- the path cover (Linux's read and write trees) ----------

fn tree(base: &Path, files: &[&str]) {
    for f in files {
        let p = base.join(f);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "x").unwrap();
    }
}

fn under(p: &Path, cover: &[PathBuf]) -> bool {
    cover.iter().any(|c| p.starts_with(c))
}

#[test]
fn the_cover_leaves_out_protected_paths_and_keeps_everything_else() {
    let d = tempfile::tempdir().unwrap();
    let b = d.path().canonicalize().unwrap();
    tree(
        &b,
        &[
            "home/.ssh/id_ed25519",
            "home/.ssh/known_hosts",
            "home/.ssh/id_ed25519.pub",
            "home/.aws/credentials",
            "home/.config/codesmith/codesmith.json",
            "home/.config/codesmith/other.txt",
            "home/.config/other/x",
            "home/work/project/src/main.rs",
            "home/.local/share/codesmith/auth.json",
            "home/.local/share/codesmith/auth.json.bak",
            "home/.local/share/codesmith/tool-output/a",
            "etc/hosts",
        ],
    );
    std::os::unix::fs::symlink(b.join("home/.ssh"), b.join("home/sshlink")).unwrap();
    std::os::unix::fs::symlink(b.join("etc"), b.join("home/etclink")).unwrap();
    let s = |p: &str| b.join(p).display().to_string();
    let prot = vec![
        s("home/.ssh"),
        s("home/.aws"),
        s("home/.config/codesmith/codesmith.json"),
        format!("{}*", s("home/.local/share/codesmith/auth.json")),
    ];
    let readable = vec![
        format!("{}/known_hosts*", s("home/.ssh")),
        format!("{}/*.pub", s("home/.ssh")),
    ];
    let c = cover(&b, &prot, &readable);
    for bad in [
        "home/.ssh/id_ed25519",
        "home/.aws/credentials",
        "home/.config/codesmith/codesmith.json",
        "home/.local/share/codesmith/auth.json",
        "home/.local/share/codesmith/auth.json.bak",
    ] {
        assert!(!under(&b.join(bad), &c), "{bad} covered by {c:?}");
    }
    // Granting a symlink grants its target: one into a protected folder
    // is left out, one elsewhere kept.
    assert!(!c.contains(&b.join("home/sshlink")), "{c:?}");
    assert!(c.contains(&b.join("home/etclink")), "{c:?}");
    for good in [
        "home/.ssh/known_hosts",
        "home/.ssh/id_ed25519.pub",
        "home/.config/codesmith/other.txt",
        "home/.config/other/x",
        "home/work/project/src/main.rs",
        "home/.local/share/codesmith/tool-output/a",
        "etc/hosts",
    ] {
        assert!(under(&b.join(good), &c), "{good} not covered by {c:?}");
    }
    // Whole folders with nothing protected stay one entry.
    assert!(c.contains(&b.join("home/work")), "{c:?}");
    assert!(c.contains(&b.join("etc")), "{c:?}");
}

/// Property: over random trees and protected sets, nothing protected is
/// covered, and every file not protected is.
#[test]
fn the_cover_is_exact_over_random_trees() {
    let mut g = Gen(0xc0ffee);
    const NAMES: &[&str] = &["a", "b", "c", ".s", "k.json", "k.json.bak"];
    for round in 0..40 {
        let d = tempfile::tempdir().unwrap();
        let b = d.path().canonicalize().unwrap();
        let mut files = Vec::new();
        for _ in 0..12 {
            let depth = 1 + g.next() % 4;
            let mut p = String::new();
            for i in 0..depth {
                if i > 0 {
                    p.push('/');
                }
                p.push_str(g.pick(NAMES));
                if i + 1 < depth {
                    p.push('d');
                }
            }
            files.push(p);
        }
        let refs: Vec<&str> = files.iter().map(String::as_str).collect();
        tree(&b, &refs);
        let mut prot = Vec::new();
        for f in files.iter().take(3) {
            let full = b.join(f);
            prot.push(if g.next().is_multiple_of(2) {
                full.parent().unwrap().display().to_string()
            } else {
                format!("{}*", full.display())
            });
        }
        let c = cover(&b, &prot, &[]);
        for f in &files {
            let p = b.join(f);
            let s = p.display().to_string();
            let protected = prot.iter().any(|x| covers(x, &s));
            assert_eq!(
                under(&p, &c),
                !protected,
                "round {round}: {f} protected={protected} prot={prot:?} cover={c:?}"
            );
        }
    }
}

// ---------- reading a refusal ----------

#[test]
fn a_refused_write_is_read_from_each_tools_error() {
    let d = tempfile::tempdir().unwrap();
    let b = d.path().canonicalize().unwrap();
    let proj = b.join("proj");
    let out = b.join("out");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::create_dir_all(out.join("sub")).unwrap();
    let p = Policy {
        write: vec![proj.clone()],
        protected: vec![b.join("secret").display().to_string()],
        ..Default::default()
    };
    let o = out.display().to_string();
    let want = |line: &str, path: PathBuf, folder: &Path| {
        assert_eq!(
            refused_write(line, &proj, &p),
            Some(Refusal::Write {
                path,
                folder: folder.to_path_buf()
            }),
            "{line}"
        );
    };
    want(
        &format!("/bin/sh: {o}/f: Operation not permitted"),
        out.join("f"),
        &out,
    );
    want(
        &format!("sh: 1: cannot create {o}/f: Permission denied"),
        out.join("f"),
        &out,
    );
    want(
        &format!("touch: cannot touch '{o}/f': Permission denied"),
        out.join("f"),
        &out,
    );
    want(
        &format!("touch: {o}/sub: Operation not permitted"),
        out.join("sub"),
        &out.join("sub"),
    );
    want(
        &format!("PermissionError: [Errno 1] Operation not permitted: '{o}/new/deep/f'"),
        out.join("new/deep/f"),
        &out,
    );
    want(
        &format!("error: could not lock config file {o}/.git/config: Operation not permitted"),
        out.join(".git/config"),
        &out,
    );
    want(
        &format!("mkdir: cannot create directory ‘{o}/x’: Permission denied"),
        out.join("x"),
        &out,
    );
    // xcrun's cache note isn't a refusal worth a folder; a real one on
    // the same output still is.
    let xcrun = format!(
        "xcrun: error: couldn't create cache file '{o}/xcrun_db-AbC123' (errno=Operation not permitted)"
    );
    assert_eq!(refused_write(&xcrun, &proj, &p), None);
    want(
        &format!(
            "{xcrun}
/bin/sh: {o}/f: Operation not permitted"
        ),
        out.join("f"),
        &out,
    );
    // Relative to the working directory.
    want("cp: ../out/f: Operation not permitted", out.join("f"), &out);
    // Inside the project, or nothing like a path, or a failure that isn't
    // a permission one: not the sandbox's.
    assert_eq!(refused_write("x: Operation not permitted", &proj, &p), None);
    assert_eq!(
        refused_write(
            &format!("{}/f: Operation not permitted", proj.display()),
            &proj,
            &p
        ),
        None
    );
    assert_eq!(
        refused_write(&format!("{o}/f: No such file or directory"), &proj, &p),
        None
    );
    // A path this user couldn't write anyway: the sandbox isn't why.
    if unsafe { libc::geteuid() } != 0 {
        assert_eq!(
            refused_write("/usr/bin/x: Permission denied", &proj, &p),
            None
        );
    }
    // A protected one says so.
    assert_eq!(
        refused_write(
            &format!("cat: {}/secret: Operation not permitted", b.display()),
            &proj,
            &p
        ),
        Some(Refusal::Protected(b.join("secret")))
    );
}

#[test]
fn the_protected_list_comes_from_the_guards_and_deny_rules() {
    let deny = Rule::new("read", "/opt/vault/*", Effect::Deny);
    let (prot, readable) = protected(&[deny]);
    let home = canonical(&dirs::home_dir().unwrap()).display().to_string();
    for p in [
        ".ssh",
        ".aws",
        ".minisign",
        ".netrc",
        ".docker/config.json",
        ".kube/config",
    ] {
        assert!(prot.contains(&format!("{home}/{p}")), "{p}: {prot:?}");
    }
    assert!(prot.iter().any(|p| p.ends_with("/auth.json*")), "{prot:?}");
    assert!(
        prot.iter().any(|p| p.ends_with("/mcp-auth.json*")),
        "{prot:?}"
    );
    assert!(prot.iter().any(|p| p.ends_with("/remote-key*")), "{prot:?}");
    assert!(
        prot.contains(&format!("{home}/.gnupg/private-keys-v1.d")),
        "{prot:?}"
    );
    // The public keyring stays readable: only the private keys are.
    assert!(!prot.contains(&format!("{home}/.gnupg")), "{prot:?}");
    assert!(prot.iter().any(|p| p == "/opt/vault"), "{prot:?}");
    assert!(
        readable.contains(&format!("{home}/.ssh/known_hosts*")),
        "{readable:?}"
    );
    assert!(
        readable.contains(&format!("{home}/.ssh/*.pub")),
        "{readable:?}"
    );
    if cfg!(target_os = "macos") {
        assert!(
            prot.contains(&format!("{home}/Library/Keychains")),
            "{prot:?}"
        );
    }
    // Name-anywhere guards (`*.env`) are the rules' alone.
    assert!(!prot.iter().any(|p| p.contains(".env")), "{prot:?}");
    let pol = Policy {
        protected: prot,
        readable,
        ..Default::default()
    };
    assert!(pol.refuses_read(Path::new(&format!("{home}/.ssh/id_ed25519"))));
    assert!(!pol.refuses_read(Path::new(&format!("{home}/.ssh/known_hosts"))));
    assert!(!pol.refuses_read(Path::new(&format!("{home}/.ssh/id_ed25519.pub"))));
}

#[test]
fn only_narrow_external_directory_allows_become_write_folders() {
    let d = tempfile::tempdir().unwrap();
    let b = d.path().canonicalize().unwrap();
    let a = |r: &str| Rule::new("external_directory", r, Effect::Allow);
    let home = dirs::home_dir().unwrap();
    let dirs = approved_dirs(&[
        a(&format!("{}/*", b.join("ok").display())),
        a("*"),
        a("/*"),
        a(&format!("{}/*", home.display())),
        a("~/*"),
        a(&format!("{}/*/x", b.display())),
        a("/tmp/*"),
        a("relative/*"),
        Rule::new(
            "external_directory",
            &format!("{}/*", b.join("asked").display()),
            Effect::Ask,
        ),
    ]);
    assert_eq!(dirs, vec![b.join("ok")]);
}

#[test]
fn a_linked_worktree_may_write_its_git_folders() {
    let d = tempfile::tempdir().unwrap();
    let b = d.path().canonicalize().unwrap();
    let common = b.join("main/.git");
    let own = common.join("worktrees/wt");
    std::fs::create_dir_all(&own).unwrap();
    std::fs::write(own.join("commondir"), "../..\n").unwrap();
    let wt = b.join("wt");
    std::fs::create_dir_all(&wt).unwrap();
    std::fs::write(wt.join(".git"), format!("gitdir: {}\n", own.display())).unwrap();
    assert_eq!(git_dirs(&wt), vec![own.clone(), common.clone()]);
    // An ordinary checkout: nothing outside.
    std::fs::create_dir_all(b.join("plain/.git")).unwrap();
    assert!(git_dirs(&b.join("plain")).is_empty());
}

#[test]
fn sudo_is_recognised_by_name() {
    for p in ["sudo", "/usr/bin/sudo", "doas"] {
        assert!(is_sudo(p), "{p}");
    }
    for p in ["sudoku", "su", "pseudo"] {
        assert!(!is_sudo(p), "{p}");
    }
}

#[test]
fn the_session_temp_folder_is_private_and_stable() {
    let a = session_tmp("ses/../x y").unwrap();
    let b = session_tmp("ses/../x y").unwrap();
    assert_eq!(a, b);
    assert!(a
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("codesmith-sandbox-"));
    assert!(!a.display().to_string().contains(".."));
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&a).unwrap().permissions().mode() & 0o077,
        0
    );
    let _ = std::fs::remove_dir_all(&a);
}

#[test]
fn jobs_default_to_the_settings_in_force() {
    let j = Jobs::default();
    assert_eq!(j.sandbox_settings(), current());
}

// ---------- through the shell tool ----------

/// Every permission rule allows everything; whatever is asked (only the
/// sandbox's rerun) is refused, as with nobody to ask.
fn allow_all() -> (Env, Arc<Recorder>) {
    let (env, rec) = Env::recording(vec![Rule::new("*", "*", Effect::Allow)]);
    rec.deny_action("external_directory");
    (env, rec)
}

fn supported() -> bool {
    support() == Support::Yes
}

/// A folder outside the project, and a file in the project naming it (so
/// commands can compute the path where the analysis can't see it).
fn outside(env: &Env) -> (tempfile::TempDir, PathBuf) {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().canonicalize().unwrap();
    std::fs::write(env.root().join("p"), p.display().to_string()).unwrap();
    (d, p)
}

async fn sh(ctx: &crate::tool::Ctx<'_>, command: &str) -> Result<String, String> {
    run(ctx, &json!({"command": command}))
        .await
        .map(|o| o.content)
}

/// Run `command`; on a machine that can't sandbox, check it was refused
/// and say so (`None`).
async fn sandboxed(ctx: &crate::tool::Ctx<'_>, command: &str) -> Option<String> {
    let r = sh(ctx, command).await;
    if !supported() {
        let e = r.expect_err("refused where the sandbox can't work");
        assert!(e.contains(RUN_SETTING), "{e}");
        return None;
    }
    Some(r.unwrap_or_else(|e| e))
}

fn perl() -> Option<&'static str> {
    ["/usr/bin/perl", "/bin/perl"]
        .into_iter()
        .find(|p| Path::new(p).is_file())
}

#[tokio::test(flavor = "multi_thread")]
async fn escape_an_interpreter_script_computing_a_path() {
    let Some(perl) = perl() else { return };
    let (env, _) = allow_all();
    let ctx = env.ctx();
    let (_d, out) = outside(&env);
    let cmd = format!(
        r#"{perl} -e 'open(F, ">", join("/", $ARGV[0], "f")) or die "open $ARGV[0]/f: $!\n"; print F "pwn"' "$(cat p)""#
    );
    if let Some(o) = sandboxed(&ctx, &cmd).await {
        assert!(!out.join("f").exists(), "{o}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn escape_an_unknown_program() {
    let (env, _) = allow_all();
    let ctx = env.ctx();
    let (_d, out) = outside(&env);
    let tool = env.write(
        "bin/mystery",
        "#!/bin/sh\nd=$(cat \"$1\")\necho pwn > \"$d/f\"\n",
    );
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    if let Some(o) = sandboxed(&ctx, "./bin/mystery p").await {
        assert!(!out.join("f").exists(), "{o}");
        assert!(o.contains("The sandbox refused a write"), "{o}");
    }
}

fn git_ok() -> bool {
    std::process::Command::new("sh")
        .args(["-c", "git --version"])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Plain git for tests: no user or system config, no signing, no
/// background maintenance.
const GIT: &str = "GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_COUNT=0 git -c maintenance.auto=false -c gc.auto=0 -c commit.gpgsign=false -c user.name=T -c user.email=t@example.com";

fn git_here(dir: &Path, args: &str) {
    let st = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{GIT} {args}"))
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .output()
        .unwrap();
    assert!(
        st.status.success(),
        "{}",
        String::from_utf8_lossy(&st.stderr)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn escape_git_dash_c_with_a_computed_folder() {
    if !git_ok() {
        return;
    }
    let (env, _) = allow_all();
    let ctx = env.ctx();
    let (_d, out) = outside(&env);
    git_here(&out, "init -q");
    let before = std::fs::read_to_string(out.join(".git/config")).unwrap();
    let cmd = format!(r#"{GIT} -C "$(cat p)" config escaped.yes true"#);
    if let Some(o) = sandboxed(&ctx, &cmd).await {
        assert_eq!(
            std::fs::read_to_string(out.join(".git/config")).unwrap(),
            before,
            "{o}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn escape_a_symlink_planted_in_the_project() {
    let (env, _) = allow_all();
    let ctx = env.ctx();
    let (_d, out) = outside(&env);
    if let Some(o) = sandboxed(
        &ctx,
        r#"ln -s "$(cat p)" link; echo pwn > link/f; mkdir link/sub"#,
    )
    .await
    {
        assert!(!out.join("f").exists(), "{o}");
        assert!(!out.join("sub").exists(), "{o}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn escape_a_hard_link() {
    let (env, _) = allow_all();
    let ctx = env.ctx();
    let (_d, out) = outside(&env);
    std::fs::write(out.join("f"), "orig").unwrap();
    if let Some(o) = sandboxed(&ctx, r#"ln "$(cat p)/f" hl; echo pwn >> hl"#).await {
        assert_eq!(
            std::fs::read_to_string(out.join("f")).unwrap(),
            "orig",
            "{o}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn escape_moving_the_project_folder() {
    let (env, _) = allow_all();
    let ctx = env.ctx();
    let root = env.root();
    let cmd = r#"d="$PWD"; mv "$d" "$d.moved"; echo pwn > "$d.moved/f""#;
    if let Some(o) = sandboxed(&ctx, cmd).await {
        assert!(root.is_dir(), "{o}");
        let moved = PathBuf::from(format!("{}.moved", root.display()));
        assert!(!moved.exists(), "{o}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_deny_rule_is_protected_inside_the_sandbox_too() {
    let d = tempfile::tempdir().unwrap();
    let out = d.path().canonicalize().unwrap();
    std::fs::write(out.join("secret"), "s3cret").unwrap();
    std::fs::write(out.join("public"), "hello").unwrap();
    let (env, rec) = Env::recording(vec![
        Rule::new("*", "*", Effect::Allow),
        Rule::new("read", &format!("{}/secret", out.display()), Effect::Deny),
    ]);
    rec.deny_action("external_directory");
    let ctx = env.ctx();
    std::fs::write(env.root().join("p"), out.display().to_string()).unwrap();
    if let Some(o) = sandboxed(&ctx, r#"d=$(cat p); cat "$d/public"; cat "$d/secret""#).await {
        assert!(o.contains("hello"), "{o}");
        assert!(!o.contains("s3cret"), "{o}");
        assert!(o.contains("The sandbox protects"), "{o}");
    }
}

/// The real home's protected folders: listing `~/.ssh` is refused (names
/// only; no secret is read either way), and `known_hosts` stays readable.
#[tokio::test(flavor = "multi_thread")]
async fn the_home_folders_secrets_are_refused() {
    let Some(home) = dirs::home_dir() else { return };
    let ssh = home.join(".ssh");
    if !ssh.is_dir() {
        return;
    }
    let (env, _) = allow_all();
    let ctx = env.ctx();
    if let Some(o) = sandboxed(
        &ctx,
        r#"ls "$HOME/.ssh" >/dev/null && echo LISTED; echo done"#,
    )
    .await
    {
        assert!(!o.contains("LISTED"), "{o}");
    }
    if ssh.join("known_hosts").is_file() {
        if let Some(o) = sandboxed(
            &ctx,
            r#"cat "$HOME/.ssh/known_hosts" >/dev/null && echo READ"#,
        )
        .await
        {
            assert!(o.contains("READ"), "{o}");
        }
    }
}

/// A Unix socket listener outside the project standing for an agent: it
/// accepts one connection and answers `ok`.
fn fake_agent(dir: &Path) -> (PathBuf, std::thread::JoinHandle<()>) {
    use std::io::Write;
    let sock = dir.join("agent.sock");
    let l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let h = std::thread::spawn(move || {
        if let Ok((mut s, _)) = l.accept() {
            let _ = s.write_all(b"agent-ok\n");
        }
    });
    (sock, h)
}

fn connect_tcp(perl: &str, port: u16) -> String {
    format!(
        r#"{perl} -MIO::Socket::INET -e '$s = IO::Socket::INET->new(PeerAddr => "127.0.0.1:{port}", Timeout => 5) or die "tcp: $!\n"; print "TCP-OK\n"'"#
    )
}

fn env_with(extra: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
    let mut m: std::collections::BTreeMap<String, String> = std::env::vars().collect();
    for (k, v) in extra {
        m.insert(k.to_string(), v.to_string());
    }
    m
}

#[tokio::test(flavor = "multi_thread")]
async fn agents_stay_reachable_and_network_none_refuses_tcp_and_udp() {
    let Some(perl) = perl() else { return };
    let (env, _) = allow_all();
    let ctx = env.ctx();
    let jobs = ctx.jobs.clone().unwrap();
    // Short: a Unix socket path has a length limit.
    let d = tempfile::Builder::new()
        .prefix("sb")
        .tempdir_in("/tmp")
        .unwrap();
    let dir = d.path().canonicalize().unwrap();
    let (sock, agent) = fake_agent(&dir);
    jobs.set_environment(Some(env_with(&[(
        "SSH_AUTH_SOCK",
        &sock.display().to_string(),
    )])));
    jobs.set_sandbox(Some(Settings {
        network: Some(Network::None),
        ..Default::default()
    }));
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = tcp.local_addr().unwrap().port();
    let cmd = format!(
        r#"{perl} -MIO::Socket::UNIX -e '$s = IO::Socket::UNIX->new(Peer => $ENV{{SSH_AUTH_SOCK}}) or die "unix: $!\n"; print scalar <$s>'; {}; {perl} -MIO::Socket::INET -e 'IO::Socket::INET->new(Proto => "udp", PeerAddr => "127.0.0.1:9") or die "udp: $!\n"; print "UDP-OK\n"'"#,
        connect_tcp(perl, port)
    );
    if let Some(o) = sandboxed(&ctx, &cmd).await {
        assert!(o.contains("agent-ok"), "the agent socket works: {o}");
        assert!(!o.contains("TCP-OK"), "{o}");
        assert!(!o.contains("UDP-OK"), "{o}");
        agent.join().unwrap();
    }
    // With the network open (the default), TCP works.
    jobs.set_sandbox(None);
    if let Some(o) = sandboxed(&ctx, &connect_tcp(perl, port)).await {
        assert!(o.contains("TCP-OK"), "{o}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_write_offers_that_folder_and_reruns_with_only_it() {
    let (env, rec) = Env::recording(vec![Rule::new("*", "*", Effect::Allow)]);
    let ctx = env.ctx();
    let (_d1, one) = outside(&env);
    let d2 = tempfile::tempdir().unwrap();
    let two = d2.path().canonicalize().unwrap();
    std::fs::write(env.root().join("p2"), two.display().to_string()).unwrap();
    let cmd =
        r#"echo a > "$(cat p)/f"; echo b > "$(cat p2)/g"; echo ran >> runs; test -f "$(cat p2)/g""#;
    let Some(o) = sandboxed(&ctx, cmd).await else {
        return;
    };
    let asked = rec.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1, "one offer: {asked:?}");
    let r: &Request = &asked[0];
    assert_eq!(r.action, "external_directory");
    assert_eq!(r.resources, vec![format!("{}/*", one.display())]);
    assert_eq!(r.access, Some(crate::permission::Access::Write));
    // Ran twice; the rerun wrote to the granted folder only.
    assert_eq!(
        std::fs::read_to_string(env.root().join("runs")).unwrap(),
        "ran\nran\n"
    );
    assert_eq!(std::fs::read_to_string(one.join("f")).unwrap(), "a\n");
    assert!(!two.join("g").exists(), "{o}");
    assert!(o.contains("ran again"), "{o}");
}

#[tokio::test(flavor = "multi_thread")]
async fn with_nobody_to_ask_a_refused_write_is_reported() {
    let (env, rec) = allow_all();
    let ctx = env.ctx();
    let (_d, out) = outside(&env);
    let Some(o) = sandboxed(&ctx, r#"echo a > "$(cat p)/f""#).await else {
        return;
    };
    assert!(o.contains("The sandbox refused a write to"), "{o}");
    assert!(o.contains(&out.join("f").display().to_string()), "{o}");
    assert!(!out.join("f").exists());
    assert_eq!(rec.actions(), vec!["external_directory".to_string()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn always_saves_the_folder_so_the_next_command_needs_no_rerun() {
    let env = Env {
        dir: tempfile::tempdir().unwrap(),
        gate: crate::permission::Gate::new(
            vec![Rule::new("*", "*", Effect::Allow)],
            Arc::new(crate::permission::Fixed(Reply::Always)),
        ),
    };
    let ctx = env.ctx();
    let (_d, out) = outside(&env);
    let Some(o) = sandboxed(&ctx, r#"echo a > "$(cat p)/f""#).await else {
        return;
    };
    assert!(o.contains("ran again"), "{o}");
    assert!(ctx
        .gate
        .saved()
        .iter()
        .any(|r| r.action == "external_directory" && r.resource == format!("{}/*", out.display())));
    let o = sandboxed(&ctx, r#"echo b > "$(cat p)/g""#).await.unwrap();
    assert!(!o.contains("ran again"), "{o}");
    assert_eq!(std::fs::read_to_string(out.join("g")).unwrap(), "b\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unsupported_machine_refuses_by_default_and_runs_with_the_notice_on_run() {
    let env = Env::new();
    let ctx = env.ctx();
    let jobs = ctx.jobs.clone().unwrap();
    jobs.set_sandbox_support(Some(Support::No("simulated".into())));
    let e = sh(&ctx, "echo ran > marker").await.unwrap_err();
    assert!(e.contains("simulated") && e.contains(RUN_SETTING), "{e}");
    assert!(!env.root().join("marker").exists());
    // Refusing isn't running unsandboxed: no notice.
    assert_eq!(jobs.take_sandbox_notice(), None);
    jobs.set_sandbox(Some(Settings {
        unsupported: Some(Unsupported::Run),
        ..Default::default()
    }));
    sh(&ctx, "echo ran > marker").await.unwrap();
    assert!(env.root().join("marker").exists());
    let n = jobs.take_sandbox_notice().expect("a notice");
    assert!(n.contains("simulated"), "{n}");
    assert_eq!(jobs.take_sandbox_notice(), None, "once per session");
}

/// Background commands are refused the same way on a machine that can't
/// sandbox.
#[tokio::test(flavor = "multi_thread")]
async fn an_unsupported_machine_refuses_background_commands_too() {
    let env = Env::new();
    let ctx = env.ctx();
    let jobs = ctx.jobs.clone().unwrap();
    jobs.set_sandbox_support(Some(Support::No("simulated".into())));
    let e = run(
        &ctx,
        &json!({"command": "echo ran > marker", "background": true}),
    )
    .await
    .unwrap_err();
    assert!(e.contains(RUN_SETTING), "{e}");
    assert_eq!(jobs.running(), 0);
    assert!(!env.root().join("marker").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn turned_off_commands_run_unsandboxed_with_one_notice() {
    let (env, _) = allow_all();
    let ctx = env.ctx();
    let jobs = ctx.jobs.clone().unwrap();
    jobs.set_sandbox(Some(Settings {
        enabled: Some(false),
        ..Default::default()
    }));
    let (_d, out) = outside(&env);
    sh(&ctx, r#"echo a > "$(cat p)/f""#).await.unwrap();
    assert!(out.join("f").exists());
    assert!(jobs.take_sandbox_notice().is_some());
    assert_eq!(jobs.take_sandbox_notice(), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn sudo_is_refused_with_a_pointer_to_askpass() {
    let env = Env::new();
    let ctx = env.ctx();
    let e = sh(&ctx, "sudo touch marker").await.unwrap_err();
    if supported() {
        assert!(e.contains("askpass.md"), "{e}");
    } else {
        assert!(e.contains(RUN_SETTING), "{e}");
    }
    assert!(!env.root().join("marker").exists());
    // Hidden in a script, it fails in the sandbox with the same pointer.
    let s = env.write("up.sh", "#!/bin/sh\nexec sudo -n true\n");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&s, std::fs::Permissions::from_mode(0o755)).unwrap();
    let have_sudo = ["/usr/bin/sudo", "/bin/sudo"]
        .iter()
        .any(|p| Path::new(p).exists());
    if supported() && have_sudo {
        let o = sh(&ctx, "./up.sh").await.unwrap_or_else(|e| e);
        assert!(o.contains("askpass.md"), "{o}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_background_job_and_its_children_are_sandboxed() {
    let (env, _) = allow_all();
    let ctx = env.ctx();
    let jobs = ctx.jobs.clone().unwrap();
    let (_d, out) = outside(&env);
    let r = run(
        &ctx,
        &json!({"command": r#"d=$(cat p); sh -c 'echo pwn > "$1/child"; echo child-done > child-done' x "$d" & echo pwn > "$d/f"; wait; echo bg-done > done"#, "background": true}),
    )
    .await;
    if !supported() {
        assert!(r.unwrap_err().contains(RUN_SETTING));
        return;
    }
    r.unwrap();
    while jobs.running() > 0 {
        jobs.wait_any().await;
    }
    assert!(env.root().join("done").exists());
    assert!(env.root().join("child-done").exists());
    assert!(!out.join("f").exists());
    assert!(!out.join("child").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn compatibility_temp_files_node_modules_and_git() {
    let (env, _) = allow_all();
    let ctx = env.ctx();
    // TMPDIR is the session's own folder, and it is writable. (macOS's
    // `mktemp` without a template ignores TMPDIR: it uses the per-user
    // temp folder, which the sandbox refuses like any other.)
    let Some(o) = sandboxed(
        &ctx,
        r#"f=$(mktemp "${TMPDIR}x.XXXXXX") && echo t > "$f" && cat "$f" && case "$TMPDIR" in *codesmith-sandbox-*) echo TMPDIR-OK;; esac; cat <<EOF
heredoc
EOF"#,
    )
    .await
    else {
        return;
    };
    assert!(
        o.contains("t\n") && o.contains("TMPDIR-OK") && o.contains("heredoc"),
        "{o}"
    );
    // npm-style installs into the project.
    let o = sandboxed(&ctx, "mkdir -p node_modules/.bin node_modules/left-pad && echo 'module.exports=1' > node_modules/left-pad/index.js && ln -sf ../left-pad/index.js node_modules/.bin/left-pad && mv node_modules/left-pad node_modules/lp && cat node_modules/lp/index.js").await.unwrap();
    assert!(o.contains("module.exports=1"), "{o}");
    if git_ok() {
        let cmd = format!("{GIT} init -q && echo x > a && {GIT} add a && {GIT} commit -qm one && {GIT} status --short && {GIT} log --oneline | wc -l");
        let o = sandboxed(&ctx, &cmd).await.unwrap();
        assert!(o.contains("?? node_modules/"), "{o}");
        assert!(o.trim().ends_with('1'), "{o}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn compatibility_cargo_builds_into_target() {
    let have = std::process::Command::new("cargo")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !have {
        return;
    }
    let env = Env::new();
    let ctx = env.ctx();
    env.write(
        "Cargo.toml",
        "[package]\nname = \"sbx\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    );
    env.write("src/main.rs", "fn main() { println!(\"built\"); }\n");
    let r = run(
        &ctx,
        &json!({"command": "env -u CARGO_TARGET_DIR cargo run --offline -q 2>&1", "timeout": 600000}),
    )
    .await;
    if !supported() {
        assert!(r.unwrap_err().contains(RUN_SETTING));
        return;
    }
    let o = r.unwrap().content;
    assert!(o.contains("built"), "{o}");
    assert!(env.root().join("target").is_dir());
}

/// Stress: many sandboxed commands at once, each with its own profile, all
/// succeed and none writes outside.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_sandboxed_commands_at_once() {
    if !supported() {
        return;
    }
    let (env, _) = allow_all();
    let (_d, out) = outside(&env);
    let env = Arc::new(env);
    let mut tasks = Vec::new();
    for i in 0..24 {
        let env = env.clone();
        tasks.push(tokio::spawn(async move {
            let ctx = env.ctx();
            let o = sh(
                &ctx,
                &format!(r#"echo {i} > in{i}; echo pwn > "$(cat p)/f{i}"; cat in{i}"#),
            )
            .await
            .unwrap_or_else(|e| e);
            assert!(o.contains(&i.to_string()), "{o}");
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    for i in 0..24 {
        assert!(!out.join(format!("f{i}")).exists());
        assert!(env.root().join(format!("in{i}")).exists());
    }
}
