#![cfg(unix)]
//! The release signing scripts, run against stand-ins: `scripts/sign-release.sh`
//! (minisign, on Skeptic-Mac), `scripts/sign-macos.sh` (Developer ID and
//! notarization, on skeptic-macmini) and the preflight of
//! `scripts/release.sh --draft-upload`.
//!
//! Each test copies the script into a temp "checkout" and runs it with a
//! `PATH` of stand-in `gh`, `minisign`, `codesign` and `xcrun` ahead of the
//! system's tools only, so the real ones (and GitHub, and any real key) are
//! never reached. The stand-ins log every call; the tests assert what was
//! run, and above all what was uploaded.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const VERSION: &str = "2.0.0";
const TAG: &str = "v2.0.0";
const TARGETS: [&str; 4] = [
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "aarch64-unknown-linux-musl",
    "x86_64-unknown-linux-musl",
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The system tools the scripts need besides the stand-ins.
fn have_system_tools() -> bool {
    ["/bin/sh", "/usr/bin/shasum", "/usr/bin/tar"]
        .iter()
        .all(|p| Path::new(p).exists())
}

const GH: &str = r#"#!/bin/sh
echo "gh $*" >> "$STUB_LOG"
case "$1 $2" in
    "release view")
        case "$*" in
            *isDraft*) [ -n "${STUB_NO_RELEASE:-}" ] && exit 1; echo "${STUB_DRAFT:-true}" ;;
            *assets*) ls "$DRAFT" ;;
        esac ;;
    "release download")
        dir=""
        while [ $# -gt 0 ]; do [ "$1" = --dir ] && dir="$2"; shift; done
        cp "$DRAFT"/* "$dir"/ ;;
    "release upload") echo "UPLOAD $*" >> "$STUB_LOG" ;;
    "release delete-asset") echo "DELETE $*" >> "$STUB_LOG" ;;
    "release create") echo "CREATE $*" >> "$STUB_LOG" ;;
    "auth status") ;;
    *) exit 1 ;;
esac
"#;

const MINISIGN: &str = r#"#!/bin/sh
echo "minisign $*" >> "$STUB_LOG"
case "$1" in
    -S)
        out=""
        while [ $# -gt 0 ]; do [ "$1" = -x ] && out="$2"; shift; done
        printf 'untrusted comment: stub\n%s\ntrusted comment: stub\nstub\n' "${STUB_SIG_LINE:-RUstub}" > "$out" ;;
    -V)
        [ -n "${STUB_VERIFY_FAILS:-}" ] && exit 1
        echo "${STUB_COMMENT:-codesmith 2.0.0 SHA256SUMS timestamp:1}" ;;
esac
"#;

const CODESIGN: &str = r#"#!/bin/sh
echo "codesign $*" >> "$STUB_LOG"
case "$1" in
    --verify) exit "${STUB_CODESIGN_VERIFY:-0}" ;;
    -dv)
        echo "Authority=${STUB_AUTHORITY:-Developer ID Application: Test (TEAMID1234)}" >&2
        echo "TeamIdentifier=${STUB_TEAM-TEAMID1234}" >&2 ;;
esac
"#;

const XCRUN: &str = r#"#!/bin/sh
echo "xcrun $*" >> "$STUB_LOG"
case "$*" in
    *notarytool\ submit*) echo "status: Accepted" ;;
    *show-sdk-path*) echo /tmp ;;
esac
"#;

const DITTO: &str = "#!/bin/sh\necho \"ditto $*\" >> \"$STUB_LOG\"\n";

/// The Team ID the stand-in `codesign` reports, and the tests pin.
const TEAM: &str = "TEAMID1234";

/// release.sh's BUILT-SHA256SUMS, made the same way from the tarballs in
/// `$1` (the draft): each tarball's hash, and for the macOS ones each
/// member as tar lists it with its file's hash (64 dashes for a folder).
const MANIFEST: &str = r#"
set -eu
cd "$1"
x=$(mktemp -d)
for t in codesmith-*.tar.gz; do
    shasum -a 256 "$t"
    case "$t" in
        *apple-darwin.tar.gz)
            rm -rf "$x"/*
            tar -xzf "$t" -C "$x"
            tar -tzf "$t" | LC_ALL=C sort | while IFS= read -r m; do
                case "$m" in
                    */) h=$(printf '%064d' 0 | tr 0 -) ;;
                    *) h=$(shasum -a 256 "$x/$m" 2>/dev/null | cut -c1-64) ;;
                esac
                printf '%s  %s//%s
' "$h" "$t" "$m"
            done ;;
    esac
done | LC_ALL=C sort -k 2 > BUILT-SHA256SUMS
rm -rf "$x"
"#;

struct World {
    dir: tempfile::TempDir,
    /// The BUILT-SHA256SUMS hash release.sh "printed".
    pin: std::cell::RefCell<String>,
}

impl World {
    /// A checkout holding `scripts/<script>` and the real `release_key.pub`,
    /// stand-ins for the tools, a draft with four tarballs whose macOS
    /// binaries are "Developer ID-signed" (by the codesign stand-in), and a
    /// fake secret key outside the checkout.
    fn new(script: &str) -> World {
        let w = World {
            dir: tempfile::tempdir().unwrap(),
            pin: Default::default(),
        };
        let repo = w.repo();
        fs::create_dir_all(repo.join("scripts")).unwrap();
        fs::create_dir_all(repo.join("crates/codesmith/src")).unwrap();
        fs::copy(
            repo_root().join("scripts").join(script),
            repo.join("scripts").join(script),
        )
        .unwrap();
        fs::copy(
            repo_root().join("crates/codesmith/src/release_key.pub"),
            repo.join("crates/codesmith/src/release_key.pub"),
        )
        .unwrap();
        for (name, body) in [
            ("gh", GH),
            ("minisign", MINISIGN),
            ("codesign", CODESIGN),
            ("xcrun", XCRUN),
            ("ditto", DITTO),
        ] {
            w.stub(name, body);
        }
        fs::create_dir_all(w.path("keys")).unwrap();
        fs::write(w.key(), "not a real key\n").unwrap();
        fs::create_dir_all(w.draft()).unwrap();
        fs::create_dir_all(w.path("cwd")).unwrap();
        fs::write(w.path("log"), "").unwrap();
        w.publish_draft();
        w
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }
    fn repo(&self) -> PathBuf {
        self.path("repo")
    }
    fn draft(&self) -> PathBuf {
        self.path("draft")
    }
    fn key(&self) -> PathBuf {
        self.path("keys/codesmith.key")
    }

    fn stub(&self, name: &str, body: &str) {
        let bin = self.path("bin");
        fs::create_dir_all(&bin).unwrap();
        let file = bin.join(name);
        fs::write(&file, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    /// Four tarballs on the draft, and a SHA256SUMS that matches them.
    fn publish_draft(&self) {
        let stage = self.path("stage");
        for t in TARGETS {
            let name = format!("codesmith-{VERSION}-{t}");
            fs::create_dir_all(stage.join(&name)).unwrap();
            fs::write(stage.join(&name).join("codesmith"), format!("binary {t}\n")).unwrap();
            fs::write(stage.join(&name).join("LICENSE"), "MIT\n").unwrap();
            let ok = Command::new("/usr/bin/tar")
                .env("COPYFILE_DISABLE", "1")
                .arg("-C")
                .arg(&stage)
                .arg("-czf")
                .arg(self.draft().join(format!("{name}.tar.gz")))
                .arg(&name)
                .status()
                .unwrap()
                .success();
            assert!(ok);
        }
        let out = Command::new("/usr/bin/shasum")
            .current_dir(self.draft())
            .args(["-a", "256"])
            .args(TARGETS.map(|t| format!("codesmith-{VERSION}-{t}.tar.gz")))
            .output()
            .unwrap();
        assert!(out.status.success());
        fs::write(self.draft().join("SHA256SUMS"), out.stdout).unwrap();
        self.built();
    }

    /// Make BUILT-SHA256SUMS from the draft's tarballs as they are now (as
    /// if release.sh had built them), and pin its hash.
    fn built(&self) {
        let ok = Command::new("/bin/sh")
            .args(["-c", MANIFEST, "manifest"])
            .arg(self.draft())
            .env("TMPDIR", self.dir.path())
            .status()
            .unwrap()
            .success();
        assert!(ok);
        *self.pin.borrow_mut() = sha256(&self.draft().join("BUILT-SHA256SUMS"));
    }

    /// Replace the draft tarball for `target` with one of `members`
    /// (`(path under the stage, contents)`; a `->` in contents makes a
    /// symlink to the rest), as tar packs them from `name`.
    fn retar(&self, target: &str, members: &[(&str, &str)], top: &[&str]) {
        let name = format!("codesmith-{VERSION}-{target}");
        let stage = self.path(&format!("restage-{target}"));
        let _ = fs::remove_dir_all(&stage);
        fs::create_dir_all(stage.join(&name)).unwrap();
        for (rel, body) in members {
            let p = stage.join(&name).join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            match body.strip_prefix("->") {
                Some(to) => std::os::unix::fs::symlink(to, &p).unwrap(),
                None => fs::write(&p, body).unwrap(),
            }
        }
        for t in top {
            fs::write(stage.join(t), "evil\n").unwrap();
        }
        let ok = Command::new("/usr/bin/tar")
            .env("COPYFILE_DISABLE", "1")
            .arg("-C")
            .arg(&stage)
            .arg("-czf")
            .arg(self.draft().join(format!("{name}.tar.gz")))
            .arg(&name)
            .args(top)
            .status()
            .unwrap()
            .success();
        assert!(ok);
    }

    /// Rewrite the draft's SHA256SUMS to match its tarballs, as an attacker
    /// who changed one would.
    fn resum(&self) {
        let out = Command::new("/usr/bin/shasum")
            .current_dir(self.draft())
            .args(["-a", "256"])
            .args(TARGETS.map(|t| format!("codesmith-{VERSION}-{t}.tar.gz")))
            .output()
            .unwrap();
        fs::write(self.draft().join("SHA256SUMS"), out.stdout).unwrap();
    }

    fn command(&self, script: &str, args: &[&str]) -> Command {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg(self.repo().join("scripts").join(script))
            .args(args)
            .current_dir(self.path("cwd"))
            .env_clear()
            .env(
                "PATH",
                format!(
                    "{}:/usr/bin:/bin:/usr/sbin:/sbin",
                    self.path("bin").display()
                ),
            )
            .env("HOME", self.path("home"))
            .env("TMPDIR", self.dir.path())
            .env("STUB_LOG", self.path("log"))
            .env("DRAFT", self.draft())
            .env("CODESMITH_MINISIGN_KEY", self.key())
            .env("CODESMITH_BUILT_SUMS_SHA256", self.pin.borrow().as_str())
            .env("CODESMITH_TEAM_ID", TEAM)
            .stdin(std::process::Stdio::null());
        cmd
    }

    fn run(&self, script: &str, args: &[&str], env: &[(&str, &str)]) -> (bool, String) {
        let mut cmd = self.command(script, args);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), text)
    }

    fn log(&self) -> String {
        fs::read_to_string(self.path("log")).unwrap()
    }

    fn uploads(&self) -> Vec<String> {
        self.log()
            .lines()
            .filter(|l| {
                l.starts_with("UPLOAD") || l.starts_with("DELETE") || l.starts_with("CREATE")
            })
            .map(str::to_string)
            .collect()
    }

    fn signed(&self) -> bool {
        self.log().lines().any(|l| l.starts_with("minisign -S"))
    }
}

fn sha256(p: &Path) -> String {
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(fs::read(p).unwrap());
    d.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// sign-release.sh

#[test]
fn sign_release_signs_the_final_sums_and_uploads_only_the_signature() {
    if !have_system_tools() {
        return;
    }
    let w = World::new("sign-release.sh");
    let (ok, out) = w.run("sign-release.sh", &[TAG], &[]);
    assert!(ok, "{out}");
    let log = w.log();
    // Signed with the given key, a trusted comment naming the version.
    let sign = log.lines().find(|l| l.starts_with("minisign -S")).unwrap();
    assert!(
        sign.contains(&format!("-s {}", w.key().display())),
        "{sign}"
    );
    assert!(
        sign.contains(&format!("-t codesmith {VERSION} SHA256SUMS timestamp:")),
        "{sign}"
    );
    // Re-verified against the pinned key, and only that.
    let verify: Vec<&str> = log
        .lines()
        .filter(|l| l.starts_with("minisign -V"))
        .collect();
    assert_eq!(verify.len(), 1, "{log}");
    assert!(verify[0].contains("-P RWTFKe6qhvSs57s1pw5x6aJRK0jdZr42tg1NgNp3a9rAJ3HersFsRoMV"));
    // The macOS binaries were checked for a Developer ID signature.
    assert_eq!(log.matches("codesign --verify").count(), 2, "{log}");
    let uploads = w.uploads();
    assert_eq!(uploads.len(), 1, "{uploads:?}");
    assert!(
        uploads[0].contains(&format!("release upload {TAG}")),
        "{uploads:?}"
    );
    assert!(uploads[0].ends_with("/SHA256SUMS.minisig"), "{uploads:?}");
    assert!(uploads[0].contains("--clobber"));
    assert!(out.contains("gh release edit v2.0.0"), "{out}");
}

#[test]
fn sign_release_dry_run_signs_but_uploads_nothing() {
    if !have_system_tools() {
        return;
    }
    let w = World::new("sign-release.sh");
    let (ok, out) = w.run("sign-release.sh", &[TAG], &[("CODESMITH_DRY_RUN", "1")]);
    assert!(ok, "{out}");
    assert!(w.signed());
    assert!(w.uploads().is_empty(), "{:?}", w.uploads());
    let kept = w.path("cwd/codesmith-signed-2.0.0");
    assert!(kept.join("SHA256SUMS").exists() && kept.join("SHA256SUMS.minisig").exists());
}

#[test]
fn sign_release_check_talks_to_nothing() {
    if !have_system_tools() {
        return;
    }
    let w = World::new("sign-release.sh");
    let (ok, out) = w.run("sign-release.sh", &["--check"], &[]);
    assert!(ok, "{out}");
    assert_eq!(w.log(), "", "--check ran something");

    // And it fails, still running nothing, without the secret key.
    fs::remove_file(w.key()).unwrap();
    let (ok, out) = w.run("sign-release.sh", &["--check"], &[]);
    assert!(!ok && out.contains("no minisign secret key"), "{out}");
    assert_eq!(w.log(), "");
}

#[test]
fn sign_release_refuses_sums_that_do_not_match_the_macos_assets() {
    if !have_system_tools() {
        return;
    }
    let w = World::new("sign-release.sh");
    // The mini re-tarred a binary but SHA256SUMS still has CI's hash (or the
    // other way round): the draft's tarball no longer matches.
    let tarball = w
        .draft()
        .join("codesmith-2.0.0-aarch64-apple-darwin.tar.gz");
    let mut bytes = fs::read(&tarball).unwrap();
    bytes.extend_from_slice(b"changed");
    fs::write(&tarball, bytes).unwrap();
    let (ok, out) = w.run("sign-release.sh", &[TAG], &[]);
    assert!(!ok, "{out}");
    assert!(out.contains("do not match its SHA256SUMS"), "{out}");
    assert!(!w.signed() && w.uploads().is_empty(), "{}", w.log());
}

#[test]
fn sign_release_refuses_sums_that_miss_or_add_a_tarball() {
    if !have_system_tools() {
        return;
    }
    let sums_path = |w: &World| w.draft().join("SHA256SUMS");
    // A macOS line missing.
    let w = World::new("sign-release.sh");
    let sums = fs::read_to_string(sums_path(&w)).unwrap();
    let without: String = sums
        .lines()
        .filter(|l| !l.contains("x86_64-apple-darwin"))
        .map(|l| format!("{l}\n"))
        .collect();
    fs::write(sums_path(&w), without).unwrap();
    let (ok, out) = w.run("sign-release.sh", &[TAG], &[]);
    assert!(
        !ok && out.contains("does not list codesmith-2.0.0-x86_64-apple-darwin"),
        "{out}"
    );
    assert!(!w.signed() && w.uploads().is_empty());

    // An extra line.
    let w = World::new("sign-release.sh");
    let mut sums = fs::read_to_string(sums_path(&w)).unwrap();
    sums.push_str(&format!("{}  evil.tar.gz\n", "0".repeat(64)));
    fs::write(sums_path(&w), sums).unwrap();
    let (ok, out) = w.run("sign-release.sh", &[TAG], &[]);
    assert!(
        !ok && out.contains("lines other than the four tarballs"),
        "{out}"
    );
    assert!(!w.signed() && w.uploads().is_empty());

    // Another version's tarballs.
    let w = World::new("sign-release.sh");
    let (ok, out) = w.run("sign-release.sh", &["v2.0.1"], &[]);
    assert!(
        !ok && out.contains("does not list codesmith-2.0.1"),
        "{out}"
    );
    assert!(!w.signed() && w.uploads().is_empty());
}

#[test]
fn sign_release_refuses_macos_binaries_the_mini_has_not_signed() {
    if !have_system_tools() {
        return;
    }
    let w = World::new("sign-release.sh");
    let (ok, out) = w.run("sign-release.sh", &[TAG], &[("STUB_CODESIGN_VERIFY", "1")]);
    assert!(!ok && out.contains("is not code-signed"), "{out}");
    assert!(!w.signed() && w.uploads().is_empty());

    let w = World::new("sign-release.sh");
    let (ok, out) = w.run(
        "sign-release.sh",
        &[TAG],
        &[("STUB_AUTHORITY", "Apple Development: x")],
    );
    assert!(
        !ok && out.contains("not signed with a Developer ID"),
        "{out}"
    );
    assert!(!w.signed() && w.uploads().is_empty());
}

#[test]
fn sign_release_refuses_a_published_or_missing_release() {
    if !have_system_tools() {
        return;
    }
    let w = World::new("sign-release.sh");
    let (ok, out) = w.run("sign-release.sh", &[TAG], &[("STUB_DRAFT", "false")]);
    assert!(!ok && out.contains("already published"), "{out}");
    assert!(!w.signed() && w.uploads().is_empty());

    let (ok, out) = w.run("sign-release.sh", &[TAG], &[("STUB_NO_RELEASE", "1")]);
    assert!(!ok && out.contains("no release v2.0.0"), "{out}");
    assert!(!w.signed() && w.uploads().is_empty());
}

#[test]
fn sign_release_refuses_a_secret_key_inside_the_repository() {
    if !have_system_tools() {
        return;
    }
    let w = World::new("sign-release.sh");
    for rel in ["codesmith.key", "crates/codesmith/src/codesmith.key"] {
        let inside = w.repo().join(rel);
        fs::write(&inside, "not a real key\n").unwrap();
        for args in [&[TAG][..], &["--check"][..]] {
            let (ok, out) = w.run(
                "sign-release.sh",
                args,
                &[("CODESMITH_MINISIGN_KEY", inside.to_str().unwrap())],
            );
            assert!(
                !ok && out.contains("is inside the repository"),
                "{rel}: {out}"
            );
        }
    }
    // Reached through a symlink to the checkout, too.
    std::os::unix::fs::symlink(w.repo(), w.path("link")).unwrap();
    let via = w.path("link/codesmith.key");
    let (ok, out) = w.run(
        "sign-release.sh",
        &[TAG],
        &[("CODESMITH_MINISIGN_KEY", via.to_str().unwrap())],
    );
    assert!(!ok && out.contains("is inside the repository"), "{out}");
    assert_eq!(w.log(), "", "ran something with a key in the repo");
}

#[test]
fn sign_release_refuses_a_bad_signature_and_uploads_nothing() {
    if !have_system_tools() {
        return;
    }
    let cases: [(&[(&str, &str)], &str); 3] = [
        (
            &[("STUB_VERIFY_FAILS", "1")],
            "does not verify with any key",
        ),
        (&[("STUB_SIG_LINE", "RWlegacy")], "legacy signature"),
        (
            &[("STUB_COMMENT", "codesmith 1.9.0 SHA256SUMS timestamp:1")],
            "unexpected trusted comment",
        ),
    ];
    for (env, expect) in cases {
        let w = World::new("sign-release.sh");
        let (ok, out) = w.run("sign-release.sh", &[TAG], env);
        assert!(!ok && out.contains(expect), "{expect}: {out}");
        assert!(w.uploads().is_empty(), "{expect}: {:?}", w.uploads());
    }
}

/// Finding 2 of security review 8: a Linux tarball swapped on the draft,
/// with SHA256SUMS rewritten to match, is refused: it is not in the manifest.
#[test]
fn sign_release_refuses_a_swapped_linux_tarball() {
    if !have_system_tools() {
        return;
    }
    let w = World::new("sign-release.sh");
    w.retar(
        "aarch64-unknown-linux-musl",
        &[("codesmith", "evil\n")],
        &[],
    );
    w.resum();
    let (ok, out) = w.run("sign-release.sh", &[TAG], &[]);
    assert!(
        !ok && out.contains("is not the one release.sh built"),
        "{out}"
    );
    assert!(!w.signed() && w.uploads().is_empty(), "{}", w.log());
}

#[test]
fn sign_release_refuses_a_foreign_team_or_no_pinned_team() {
    if !have_system_tools() {
        return;
    }
    let w = World::new("sign-release.sh");
    let (ok, out) = w.run("sign-release.sh", &[TAG], &[("STUB_TEAM", "EVILTEAM99")]);
    assert!(
        !ok && out.contains("signed by team EVILTEAM99, not TEAMID1234"),
        "{out}"
    );
    assert!(!w.signed() && w.uploads().is_empty());
    // No TeamIdentifier at all (ad hoc), or another team with a Developer
    // ID authority naming ours.
    let w = World::new("sign-release.sh");
    let (ok, out) = w.run("sign-release.sh", &[TAG], &[("STUB_TEAM", "")]);
    assert!(!ok && out.contains("not TEAMID1234"), "{out}");
    assert!(!w.signed());
    for team in ["", "teamid1234", "TEAM ID", "TEAM;ID"] {
        let w = World::new("sign-release.sh");
        let (ok, out) = w.run("sign-release.sh", &[TAG], &[("CODESMITH_TEAM_ID", team)]);
        assert!(
            !ok && out.contains("set CODESMITH_TEAM_ID"),
            "{team:?}: {out}"
        );
        assert!(
            !w.signed() && !w.log().contains("release download"),
            "{team:?}"
        );
    }
}

/// The macOS tarballs must hold what was built, but for the binary: an
/// extra member (a `SHA256SUMS` inside the folder), a changed file, a
/// missing one, or a symlink is refused, even with SHA256SUMS rewritten.
#[test]
fn sign_release_refuses_macos_tarballs_with_other_contents() {
    if !have_system_tools() {
        return;
    }
    type Case<'a> = (&'a [(&'a str, &'a str)], &'a [&'a str], &'a str);
    let cases: [Case; 6] = [
        (
            &[
                ("codesmith", "signed\n"),
                ("LICENSE", "MIT\n"),
                ("SHA256SUMS", "x\n"),
            ],
            &[],
            "holds other files than release.sh built",
        ),
        (
            &[("codesmith", "signed\n"), ("LICENSE", "GPL\n")],
            &[],
            "holds other files than release.sh built",
        ),
        (
            &[("codesmith", "signed\n")],
            &[],
            "holds other files than release.sh built",
        ),
        (
            &[("codesmith", "signed\n"), ("LICENSE", "->/etc/passwd")],
            &[],
            "has a link",
        ),
        (
            &[("codesmith", "->/bin/sh"), ("LICENSE", "MIT\n")],
            &[],
            "has a link",
        ),
        (
            &[("codesmith", "signed\n"), ("LICENSE", "MIT\n")],
            &["SHA256SUMS"],
            "outside",
        ),
    ];
    for (members, top, expect) in cases {
        let w = World::new("sign-release.sh");
        w.retar("x86_64-apple-darwin", members, top);
        w.resum();
        let (ok, out) = w.run("sign-release.sh", &[TAG], &[]);
        assert!(!ok && out.contains(expect), "{members:?} {top:?}: {out}");
        assert!(!w.signed() && w.uploads().is_empty(), "{members:?}");
    }
    // Only the binary may differ from the build: a re-signed binary passes.
    let w = World::new("sign-release.sh");
    w.retar(
        "x86_64-apple-darwin",
        &[("codesmith", "signed by the mini\n"), ("LICENSE", "MIT\n")],
        &[],
    );
    w.resum();
    let (ok, out) = w.run("sign-release.sh", &[TAG], &[]);
    assert!(ok, "{out}");
    assert!(w.signed());
}

#[test]
fn sign_release_refuses_a_manifest_without_the_printed_hash() {
    if !have_system_tools() {
        return;
    }
    let w = World::new("sign-release.sh");
    let pin = w.pin.borrow().clone();
    w.retar(
        "aarch64-unknown-linux-musl",
        &[("codesmith", "evil\n")],
        &[],
    );
    w.resum();
    w.built();
    *w.pin.borrow_mut() = pin;
    let (ok, out) = w.run("sign-release.sh", &[TAG], &[]);
    assert!(
        !ok && out.contains("does not have the hash release.sh printed"),
        "{out}"
    );
    assert!(!w.signed() && w.uploads().is_empty());
    for pin in ["", "0", &"A".repeat(64)] {
        let w = World::new("sign-release.sh");
        let (ok, out) = w.run(
            "sign-release.sh",
            &[TAG],
            &[("CODESMITH_BUILT_SUMS_SHA256", pin)],
        );
        assert!(
            !ok && out.contains("CODESMITH_BUILT_SUMS_SHA256"),
            "{pin:?}: {out}"
        );
        assert!(!w.signed() && !w.log().contains("release download"));
    }
    // --check needs neither.
    let w = World::new("sign-release.sh");
    let (ok, out) = w.run(
        "sign-release.sh",
        &["--check"],
        &[
            ("CODESMITH_BUILT_SUMS_SHA256", ""),
            ("CODESMITH_TEAM_ID", ""),
        ],
    );
    assert!(ok, "{out}");
}

#[test]
fn sign_release_refuses_while_no_key_is_pinned() {
    if !have_system_tools() {
        return;
    }
    let w = World::new("sign-release.sh");
    fs::write(
        w.repo().join("crates/codesmith/src/release_key.pub"),
        "# none\nPLACEHOLDER\n",
    )
    .unwrap();
    let (ok, out) = w.run("sign-release.sh", &[TAG], &[]);
    assert!(!ok && out.contains("no release key"), "{out}");
    assert_eq!(w.log(), "");
}

// ---------------------------------------------------------------------------
// sign-macos.sh

#[cfg(target_os = "macos")]
fn sign_macos_env() -> [(&'static str, &'static str); 2] {
    [
        (
            "CODESMITH_SIGN_IDENTITY",
            "Developer ID Application: Test (TEAMID)",
        ),
        ("CODESMITH_NOTARY_PROFILE", "codesmith-notary"),
    ]
}

#[cfg(target_os = "macos")]
#[test]
fn sign_macos_never_runs_minisign_and_hands_over_to_sign_release() {
    if !have_system_tools() {
        return;
    }
    let w = World::new("sign-macos.sh");
    // A signature left on the draft by an earlier run.
    fs::write(w.draft().join("SHA256SUMS.minisig"), "stale\n").unwrap();
    let (ok, out) = w.run("sign-macos.sh", &[TAG], &sign_macos_env());
    assert!(ok, "{out}");
    let log = w.log();
    assert!(!log.contains("minisign"), "{log}");
    assert_eq!(log.matches("notarytool submit").count(), 2, "{log}");
    let uploads = w.uploads();
    assert_eq!(uploads.len(), 2, "{uploads:?}");
    let upload = &uploads[0];
    assert!(
        upload.starts_with(&format!("UPLOAD release upload {TAG}")),
        "{upload}"
    );
    assert!(
        upload.contains("aarch64-apple-darwin.tar.gz")
            && upload.contains("x86_64-apple-darwin.tar.gz")
    );
    assert!(upload.ends_with("/SHA256SUMS"), "{upload}");
    assert!(
        !upload.contains("minisig") && !upload.contains("linux"),
        "{upload}"
    );
    assert!(
        uploads[1].contains("delete-asset v2.0.0 SHA256SUMS.minisig"),
        "{uploads:?}"
    );
    assert!(
        !log.contains("release edit"),
        "sign-macos.sh must not publish"
    );
    assert!(
        out.contains("run `scripts/sign-release.sh v2.0.0` on the minisign Mac"),
        "{out}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn sign_macos_dry_run_uploads_nothing_and_needs_no_minisign() {
    if !have_system_tools() {
        return;
    }
    let w = World::new("sign-macos.sh");
    fs::remove_file(w.path("bin/minisign")).unwrap();
    fs::remove_file(w.key()).unwrap();
    let (ok, out) = w.run(
        "sign-macos.sh",
        &[TAG],
        &[sign_macos_env()[0], ("CODESMITH_DRY_RUN", "1")],
    );
    assert!(ok, "{out}");
    assert!(w.uploads().is_empty(), "{:?}", w.uploads());
    let kept = w.path("cwd/codesmith-signed-2.0.0");
    assert!(kept.join("SHA256SUMS").exists());
    assert!(!kept.join("SHA256SUMS.minisig").exists());
    // The Linux lines are CI's, the macOS lines are the re-tarred tarballs'.
    let before = fs::read_to_string(w.draft().join("SHA256SUMS")).unwrap();
    let after = fs::read_to_string(kept.join("SHA256SUMS")).unwrap();
    assert_eq!(after.lines().count(), 4);
    for line in before.lines().filter(|l| l.contains("linux")) {
        assert!(after.contains(line), "{after}");
    }
}

#[cfg(target_os = "macos")]
#[test]
fn sign_macos_refuses_a_published_release_or_mismatched_tarballs() {
    if !have_system_tools() {
        return;
    }
    let w = World::new("sign-macos.sh");
    let (ok, out) = w.run(
        "sign-macos.sh",
        &[TAG],
        &[
            sign_macos_env()[0],
            sign_macos_env()[1],
            ("STUB_DRAFT", "false"),
        ],
    );
    assert!(!ok && out.contains("already published"), "{out}");
    assert!(w.uploads().is_empty());

    let w = World::new("sign-macos.sh");
    let tarball = w.draft().join("codesmith-2.0.0-x86_64-apple-darwin.tar.gz");
    let mut bytes = fs::read(&tarball).unwrap();
    bytes.extend_from_slice(b"changed");
    fs::write(&tarball, bytes).unwrap();
    let (ok, out) = w.run("sign-macos.sh", &[TAG], &sign_macos_env());
    assert!(
        !ok && out.contains("does not match BUILT-SHA256SUMS"),
        "{out}"
    );
    assert!(w.uploads().is_empty());
    assert!(!w.log().contains("codesign --force"));
}

/// Finding 2 of security review 8: the draft is not trusted. A tarball
/// swapped on the draft, with SHA256SUMS rewritten to match it, used to be
/// signed; now each must match the manifest whose hash release.sh printed.
#[cfg(target_os = "macos")]
#[test]
fn sign_macos_refuses_a_swapped_tarball_even_with_matching_sums() {
    if !have_system_tools() {
        return;
    }
    for target in ["x86_64-unknown-linux-musl", "aarch64-apple-darwin"] {
        let w = World::new("sign-macos.sh");
        w.retar(
            target,
            &[("codesmith", "evil binary\n"), ("LICENSE", "MIT\n")],
            &[],
        );
        w.resum();
        let (ok, out) = w.run("sign-macos.sh", &[TAG], &sign_macos_env());
        assert!(
            !ok && out.contains("does not match BUILT-SHA256SUMS"),
            "{target}: {out}"
        );
        assert!(w.uploads().is_empty(), "{target}");
        assert!(!w.log().contains("codesign --force"), "{target}");
    }
}

#[cfg(target_os = "macos")]
#[test]
fn sign_macos_refuses_a_manifest_without_the_printed_hash() {
    if !have_system_tools() {
        return;
    }
    // The draft's manifest rewritten along with a swapped tarball.
    let w = World::new("sign-macos.sh");
    let pin = w.pin.borrow().clone();
    w.retar("aarch64-apple-darwin", &[("codesmith", "evil\n")], &[]);
    w.built();
    *w.pin.borrow_mut() = pin;
    let (ok, out) = w.run("sign-macos.sh", &[TAG], &sign_macos_env());
    assert!(
        !ok && out.contains("does not have the hash release.sh printed"),
        "{out}"
    );
    assert!(w.uploads().is_empty() && !w.log().contains("codesign --force"));
    // No hash, a malformed one, or no manifest on the draft.
    for (pin, expect) in [
        ("", "set CODESMITH_BUILT_SUMS_SHA256"),
        ("ABC", "set CODESMITH_BUILT_SUMS_SHA256"),
        ("abc", "must be 64 lowercase hex digits"),
    ] {
        let w = World::new("sign-macos.sh");
        let mut env = sign_macos_env().to_vec();
        env.push(("CODESMITH_BUILT_SUMS_SHA256", pin));
        let (ok, out) = w.run("sign-macos.sh", &[TAG], &env);
        assert!(!ok && out.contains(expect), "{pin:?}: {out}");
        assert_eq!(w.log(), "", "{pin:?}");
    }
    let w = World::new("sign-macos.sh");
    fs::remove_file(w.draft().join("BUILT-SHA256SUMS")).unwrap();
    let (ok, out) = w.run("sign-macos.sh", &[TAG], &sign_macos_env());
    assert!(!ok && out.contains("has no BUILT-SHA256SUMS"), "{out}");
    assert!(w.uploads().is_empty());
}

/// A macOS tarball with a symlink, a hardlink-free but unsafe layout, or a
/// member outside its folder (a top-level `SHA256SUMS` that unpacking used
/// to drop over the work folder's) is refused before signing, even when
/// the manifest lists it.
#[cfg(target_os = "macos")]
#[test]
fn sign_macos_refuses_tarballs_with_links_or_members_outside_their_folder() {
    if !have_system_tools() {
        return;
    }
    type Case<'a> = (&'a [(&'a str, &'a str)], &'a [&'a str], &'a str);
    let cases: [Case; 4] = [
        (&[("codesmith", "->/etc/passwd")], &[], "has a link"),
        (
            &[
                ("codesmith", "bin\n"),
                ("docs/x.md", "->../../../SHA256SUMS"),
            ],
            &[],
            "has a link",
        ),
        (&[("codesmith", "bin\n")], &["SHA256SUMS"], "outside"),
        (&[("codesmith", "bin\n")], &["BUILT-SHA256SUMS"], "outside"),
    ];
    for (members, top, expect) in cases {
        let w = World::new("sign-macos.sh");
        w.retar("aarch64-apple-darwin", members, top);
        w.built();
        let (ok, out) = w.run("sign-macos.sh", &[TAG], &sign_macos_env());
        assert!(!ok && out.contains(expect), "{members:?} {top:?}: {out}");
        assert!(w.uploads().is_empty());
        assert!(!w.log().contains("codesign --force"), "{}", w.log());
    }
}

/// Only the binary is unpacked to be signed, and the final SHA256SUMS
/// takes the Linux lines from the manifest, not from the draft's SHA256SUMS.
#[cfg(target_os = "macos")]
#[test]
fn sign_macos_signs_only_the_binary_and_takes_linux_lines_from_the_manifest() {
    if !have_system_tools() {
        return;
    }
    let w = World::new("sign-macos.sh");
    let built = fs::read_to_string(w.draft().join("BUILT-SHA256SUMS")).unwrap();
    // A forged Linux line in the draft's SHA256SUMS is never used.
    fs::write(
        w.draft().join("SHA256SUMS"),
        format!(
            "{}  codesmith-2.0.0-x86_64-unknown-linux-musl.tar.gz\n",
            "0".repeat(64)
        ),
    )
    .unwrap();
    let (ok, out) = w.run(
        "sign-macos.sh",
        &[TAG],
        &[sign_macos_env()[0], ("CODESMITH_DRY_RUN", "1")],
    );
    assert!(ok, "{out}");
    let sums = fs::read_to_string(w.path("cwd/codesmith-signed-2.0.0/SHA256SUMS")).unwrap();
    assert_eq!(sums.lines().count(), 4, "{sums}");
    for line in built
        .lines()
        .filter(|l| l.contains("linux") && !l.contains("//"))
    {
        assert!(sums.contains(line), "{sums}");
    }
    assert!(!sums.contains(&"0".repeat(64)), "{sums}");
    let log = w.log();
    let signs: Vec<&str> = log
        .lines()
        .filter(|l| l.starts_with("codesign --force"))
        .map(|l| l.rsplit(' ').next().unwrap())
        .collect();
    assert_eq!(signs.len(), 2);
    for s in signs {
        assert!(s.ends_with("/codesmith") && s.contains("/bin-"), "{s}");
    }
    // The re-tarred tarball keeps the other members as built.
    let tarball = w.path("cwd/codesmith-signed-2.0.0/codesmith-2.0.0-aarch64-apple-darwin.tar.gz");
    let out = Command::new("/usr/bin/tar")
        .arg("-tzf")
        .arg(&tarball)
        .output()
        .unwrap();
    let mut names: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "codesmith-2.0.0-aarch64-apple-darwin/",
            "codesmith-2.0.0-aarch64-apple-darwin/LICENSE",
            "codesmith-2.0.0-aarch64-apple-darwin/codesmith",
        ]
    );
}

// ---------------------------------------------------------------------------
// release.sh's BUILT-SHA256SUMS

/// The part of release.sh that writes BUILT-SHA256SUMS, run on the draft's
/// tarballs, writes what the signing scripts expect (the tests' own
/// manifest), and prints its hash.
#[test]
fn release_sh_writes_the_manifest_the_signing_scripts_check() {
    if !have_system_tools() {
        return;
    }
    let w = World::new("sign-release.sh");
    let script = fs::read_to_string(repo_root().join("scripts/release.sh")).unwrap();
    let start = script.find("# >>> built manifest").unwrap();
    let end = script.find("# <<< built manifest").unwrap();
    let part = &script[start..end];
    let dist = w.path("dist");
    fs::create_dir_all(&dist).unwrap();
    for t in TARGETS {
        let n = format!("codesmith-{VERSION}-{t}.tar.gz");
        fs::copy(w.draft().join(&n), dist.join(&n)).unwrap();
    }
    let out = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("set -eu\n{part}\nprintf '%s\\n' \"$built_sha\""))
        .env("dist", &dist)
        .env("version", VERSION)
        .env("targets", TARGETS.join(" "))
        .env("TMPDIR", w.dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let theirs = fs::read_to_string(dist.join("BUILT-SHA256SUMS")).unwrap();
    let ours = fs::read_to_string(w.draft().join("BUILT-SHA256SUMS")).unwrap();
    assert_eq!(theirs, ours);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        sha256(&dist.join("BUILT-SHA256SUMS"))
    );
    // Four tarball lines, and every macOS member with its hash.
    assert_eq!(theirs.lines().filter(|l| !l.contains("//")).count(), 4);
    let mac = format!("codesmith-{VERSION}-aarch64-apple-darwin");
    for m in ["/", "/LICENSE", "/codesmith"] {
        assert!(
            theirs.contains(&format!("  {mac}.tar.gz//{mac}{m}\n")),
            "{m}: {theirs}"
        );
    }
    assert!(!theirs.contains("linux-musl.tar.gz//"), "{theirs}");
    assert!(!dist.join("BUILT-SHA256SUMS.tmp").exists());
}

// ---------------------------------------------------------------------------
// release.sh --draft-upload preflight

#[cfg(target_os = "macos")]
mod release_sh {
    use super::*;

    const CARGO: &str = r#"#!/bin/sh
echo "cargo $*" >> "$STUB_LOG"
[ "$*" = "zigbuild --help" ] && exit 0
exit 1
"#;
    const RUSTUP: &str = r#"#!/bin/sh
printf '%s\n' aarch64-apple-darwin x86_64-apple-darwin aarch64-unknown-linux-musl x86_64-unknown-linux-musl
"#;

    fn git(w: &World, args: &[&str]) {
        let ok = Command::new("git")
            .current_dir(w.repo())
            .env("HOME", w.path("home"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "tag.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    /// A committed checkout at version 2.0.0 with the real release key.
    fn world() -> Option<World> {
        if !have_system_tools() || !Path::new("/usr/bin/git").exists() {
            return None;
        }
        let w = World::new("release.sh");
        w.stub("cargo", CARGO);
        w.stub("rustup", RUSTUP);
        w.stub("zig", "#!/bin/sh\n");
        fs::write(
            w.repo().join("Cargo.toml"),
            "[workspace.package]\nversion = \"2.0.0\"\n",
        )
        .unwrap();
        fs::create_dir_all(w.path("home")).unwrap();
        git(&w, &["init", "-q"]);
        git(&w, &["add", "-A"]);
        git(&w, &["commit", "-qm", "Release 2.0.0"]);
        Some(w)
    }

    fn draft_upload(w: &World) -> (bool, String) {
        let mut cmd = w.command("release.sh", &["2.0.0", "--draft-upload"]);
        cmd.current_dir(w.repo())
            .env("HOME", w.path("home"))
            .env("GIT_CONFIG_NOSYSTEM", "1");
        let out = cmd.output().unwrap();
        let text = String::from_utf8_lossy(&out.stderr).into_owned();
        (out.status.success(), text)
    }

    fn nothing_built_or_created(w: &World) {
        let log = w.log();
        assert!(!log.lines().any(|l| l.starts_with("cargo build")), "{log}");
        assert!(w.uploads().is_empty(), "{log}");
    }

    #[test]
    fn refuses_an_unclean_tree() {
        let Some(w) = world() else { return };
        git(&w, &["tag", "v2.0.0"]);
        fs::write(w.repo().join("stray.txt"), "x").unwrap();
        let (ok, err) = draft_upload(&w);
        assert!(
            !ok && err.contains("the working tree is not clean"),
            "{err}"
        );
        nothing_built_or_created(&w);
    }

    #[test]
    fn refuses_without_the_tag() {
        let Some(w) = world() else { return };
        let (ok, err) = draft_upload(&w);
        assert!(!ok && err.contains("no tag v2.0.0"), "{err}");
        nothing_built_or_created(&w);
    }

    #[test]
    fn refuses_when_head_is_not_at_the_tag() {
        let Some(w) = world() else { return };
        git(&w, &["tag", "v2.0.0"]);
        git(&w, &["commit", "-q", "--allow-empty", "-m", "after"]);
        let (ok, err) = draft_upload(&w);
        assert!(!ok && err.contains("HEAD is not at v2.0.0"), "{err}");
        nothing_built_or_created(&w);
    }

    #[test]
    fn refuses_a_version_other_than_cargo_toml() {
        let Some(w) = world() else { return };
        let mut cmd = w.command("release.sh", &["2.0.1", "--draft-upload"]);
        cmd.current_dir(w.repo()).env("HOME", w.path("home"));
        let out = cmd.output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success() && err.contains("does not match Cargo.toml"),
            "{err}"
        );
        nothing_built_or_created(&w);
    }

    #[test]
    fn refuses_without_a_pinned_key_but_proceeds_with_one() {
        let Some(w) = world() else { return };
        // With the real key, a clean tree at the tag gets past preflight
        // and starts building (the cargo stand-in then fails).
        git(&w, &["tag", "v2.0.0"]);
        let (ok, err) = draft_upload(&w);
        assert!(!ok, "{err}");
        assert!(
            w.log().lines().any(|l| l.starts_with("cargo build")),
            "{err}"
        );
        assert!(w.uploads().is_empty());

        fs::write(
            w.repo().join("crates/codesmith/src/release_key.pub"),
            "PLACEHOLDER\n",
        )
        .unwrap();
        git(&w, &["commit", "-qam", "no key"]);
        git(&w, &["tag", "-f", "v2.0.0"]);
        fs::write(w.path("log"), "").unwrap();
        let (ok, err) = draft_upload(&w);
        assert!(!ok && err.contains("no release key"), "{err}");
        nothing_built_or_created(&w);
    }
}
