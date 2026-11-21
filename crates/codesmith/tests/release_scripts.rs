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
    -dv) echo "Authority=${STUB_AUTHORITY:-Developer ID Application: Test (TEAMID)}" >&2 ;;
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

struct World {
    dir: tempfile::TempDir,
}

impl World {
    /// A checkout holding `scripts/<script>` and the real `release_key.pub`,
    /// stand-ins for the tools, a draft with four tarballs whose macOS
    /// binaries are "Developer ID-signed" (by the codesign stand-in), and a
    /// fake secret key outside the checkout.
    fn new(script: &str) -> World {
        let w = World {
            dir: tempfile::tempdir().unwrap(),
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
    assert!(!ok && out.contains("do not match SHA256SUMS"), "{out}");
    assert!(w.uploads().is_empty());
    assert!(!w.log().contains("codesign --force"));
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
