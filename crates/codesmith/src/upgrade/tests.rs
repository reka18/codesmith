//! Tests ported from OpenCode's `cli/src/services/updater.test.ts`,
//! `cli/test/upgrade.test.ts` and `cli/test/updater-install.test.ts`
//! (MIT). Network tests run against a local fixture server, never GitHub.

use super::*;
use std::collections::HashMap;
use std::io::BufRead;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------- updater.test.ts

#[test]
fn oc_reads_update_policy_from_jsonc() {
    assert_eq!(
        decode_policy("{ // preference\n \"update\": \"notify\",\n}"),
        Some(Policy::Notify)
    );
    assert_eq!(
        decode_policy(r#"{ "update": "disable" }"#),
        Some(Policy::Disable)
    );
    assert_eq!(decode_policy(r#"{ "update": "auto" }"#), Some(Policy::Auto));
    assert_eq!(decode_policy(r#"{ "update": "invalid" }"#), None);
}

#[test]
fn oc_maps_the_v1_update_policy() {
    assert_eq!(
        decode_policy(r#"{ "autoupdate": false }"#),
        Some(Policy::Disable)
    );
    assert_eq!(
        decode_policy(r#"{ "autoupdate": "notify" }"#),
        Some(Policy::Notify)
    );
    assert_eq!(
        decode_policy(r#"{ "autoupdate": true }"#),
        Some(Policy::Auto)
    );
}

fn notify(current: &str, latest: &str) -> Action {
    action(current, latest, Policy::Notify)
}

#[test]
fn oc_reports_every_available_release() {
    assert_eq!(notify("1.2.3", "1.2.4"), Action::Notify);
    assert_eq!(notify("1.2.3", "1.3.0"), Action::Notify);
    assert_eq!(notify("1.2.3", "2.0.0"), Action::Notify);
    assert_eq!(notify("1.2.3", "1.2.3"), Action::None);
}

#[test]
fn oc_automatically_installs_every_available_release_when_enabled() {
    assert_eq!(action("1.2.3", "1.2.4", Policy::Auto), Action::Auto);
    assert_eq!(action("1.2.3", "1.2.3", Policy::Auto), Action::None);
}

#[test]
fn oc_skips_when_updates_are_disabled() {
    assert_eq!(action("1.2.3", "1.2.4", Policy::Disable), Action::None);
}

#[test]
fn oc_reports_up_to_date_only_when_versions_match() {
    assert_eq!(notify("1.2.3", "1.2.3"), Action::None);
}

#[test]
fn oc_reports_when_latest_is_lower() {
    assert_eq!(notify("1.2.4", "1.2.3"), Action::Notify);
}

#[test]
fn oc_accepts_strict_release_version_variants() {
    assert_eq!(notify("v1.2.3", " 1.2.4\n"), Action::Notify);
    assert_eq!(notify("1.2.3-alpha.1", "1.2.3-alpha.2"), Action::Notify);
    assert_eq!(
        notify("0.0.0-dev-17403", "0.0.0-dev-17403.2"),
        Action::Notify
    );
    assert_eq!(
        notify("0.0.0-next-17403", "0.0.0-beta-17404"),
        Action::Notify
    );
    assert_eq!(notify("1.2.3+old", "1.2.3+new"), Action::None);
    assert_eq!(notify("v1.2.3+old", "1.2.3"), Action::None);
}

#[test]
fn oc_preserves_strict_validity() {
    for v in [
        "=1.2.3",
        "V1.2.3",
        "1.2",
        "1.2.3.4",
        "01.2.3",
        "1.02.3",
        "1.2.03",
        "1.2.3-01",
        "1.2.3-",
        "1.2.3+",
        "1.2.3-alpha..1",
        "1.2.3_alpha",
        "9007199254740992.0.0",
        "0.9007199254740992.0",
        "0.0.9007199254740992",
    ] {
        assert_eq!(notify("1.2.3", v), Action::None, "{v}");
    }
}

#[test]
fn oc_handles_numeric_limits_without_losing_precision() {
    assert_eq!(
        notify("9007199254740991.0.0", "9007199254740991.0.1"),
        Action::Notify
    );
    assert_eq!(
        notify("9007199254740990.0.0", "9007199254740991.0.0"),
        Action::Notify
    );
}

#[test]
fn oc_preserves_equality_for_oversized_numeric_prerelease_identifiers() {
    assert_eq!(
        notify("1.0.0-9007199254740992", "1.0.0-9007199254740993"),
        Action::None
    );
    assert_eq!(
        notify("1.0.0-9007199254740991", "1.0.0-9007199254740992"),
        Action::Notify
    );
}

#[test]
fn oc_rejects_versions_longer_than_semvers_limit_before_trimming() {
    assert_eq!(
        notify("1.2.3", &format!("{}1.2.3", " ".repeat(251))),
        Action::None
    );
    assert_eq!(
        notify("1.2.3", &format!("1.2.4+{}", "a".repeat(250))),
        Action::Notify
    );
    assert_eq!(
        notify("1.2.3", &format!("{}1.2.4", " ".repeat(252))),
        Action::None
    );
}

// ---------------------------------------------------------------- upgrade.test.ts

/// OpenCode's `fixture/upgrade.ts`: an updater that records what the
/// command asks of it.
struct Fake {
    events: Vec<String>,
    detected: Detected,
    latest_error: bool,
    install_error: bool,
}

impl Fake {
    fn new() -> Self {
        Fake {
            events: Vec::new(),
            detected: Detected::Method(Method::Binary),
            latest_error: false,
            install_error: false,
        }
    }
}

impl Updater for Fake {
    fn method(&mut self) -> Detected {
        self.events.push("method".into());
        self.detected.clone()
    }
    async fn latest(&mut self) -> Result<String> {
        self.events.push("latest".into());
        if self.latest_error {
            bail!("the releases page returned HTTP 503")
        }
        Ok("0.0.0-beta-new".into())
    }
    async fn upgrade(&mut self, version: &str) -> Result<()> {
        self.events.push(format!("upgrade {version}"));
        if self.install_error {
            bail!("cannot write to /usr/local/bin: Permission denied (os error 13)")
        }
        Ok(())
    }
}

/// Parse `codesmith upgrade ARGS` and run it against `fake`; returns the
/// output with any error appended, and whether it succeeded.
async fn cli(fake: &mut Fake, args: &[&str]) -> (String, bool) {
    let argv = ["codesmith", "upgrade"].iter().chain(args).copied();
    let (target, method, check) = match crate::cli::parse_upgrade(argv) {
        Ok(parsed) => parsed,
        Err(e) => return (e.to_string(), false),
    };
    let mut out = Vec::new();
    let result = command(
        fake,
        "0.0.0-beta-old",
        target.as_deref(),
        method,
        check,
        &mut out,
    )
    .await;
    let mut text = String::from_utf8(out).unwrap();
    if let Err(e) = &result {
        text.push_str(&format!("{e:#}"));
    }
    (text, result.is_ok())
}

#[test]
fn oc_upgrade_is_registered_in_root_help_and_documents_its_options() {
    let root = crate::cli::help(&[]);
    let help = crate::cli::help(&["upgrade"]);
    assert!(root.contains("upgrade"), "{root}");
    assert!(help.contains("[TARGET]"), "{help}");
    assert!(help.contains("--method"), "{help}");
    assert!(help.contains("-m"), "{help}");
}

#[tokio::test]
async fn oc_upgrade_detects_the_installation_method_and_resolves_the_latest_version() {
    let mut fake = Fake::new();
    let (out, ok) = cli(&mut fake, &[]).await;
    assert!(ok, "{out}");
    assert_eq!(fake.events, ["method", "latest", "upgrade 0.0.0-beta-new"]);
    assert!(out.contains("Upgrade complete"), "{out}");
}

#[tokio::test]
async fn oc_upgrade_resolves_the_latest_version_for_an_explicit_method_without_detection() {
    let mut fake = Fake::new();
    fake.detected = Detected::Unknown;
    let (out, ok) = cli(&mut fake, &["--method", "binary"]).await;
    assert!(ok, "{out}");
    assert_eq!(fake.events, ["latest", "upgrade 0.0.0-beta-new"]);
}

#[tokio::test]
async fn oc_upgrade_accepts_an_explicit_version_and_method_without_detection_or_a_lookup() {
    let mut fake = Fake::new();
    let (out, ok) = cli(&mut fake, &["v0.0.0-beta-target", "--method", "binary"]).await;
    assert!(ok, "{out}");
    assert_eq!(fake.events, ["upgrade v0.0.0-beta-target"]);
    assert!(out.contains("0.0.0-beta-old → 0.0.0-beta-target"), "{out}");
}

#[tokio::test]
async fn oc_upgrade_accepts_the_short_method_flag_and_an_explicit_major_upgrade() {
    let mut fake = Fake::new();
    let (out, ok) = cli(&mut fake, &["2.0.0", "-m", "binary"]).await;
    assert!(ok, "{out}");
    assert_eq!(fake.events, ["upgrade 2.0.0"]);
}

#[tokio::test]
async fn oc_upgrade_skips_the_already_installed_version() {
    let mut fake = Fake::new();
    let (out, ok) = cli(&mut fake, &["v0.0.0-beta-old"]).await;
    assert!(ok, "{out}");
    assert_eq!(fake.events, ["method"]);
    assert!(out.contains("already installed"), "{out}");
}

#[tokio::test]
async fn oc_upgrade_requires_an_explicit_method_when_detection_fails() {
    let mut fake = Fake::new();
    fake.detected = Detected::Unknown;
    let (out, ok) = cli(&mut fake, &[]).await;
    assert!(!ok);
    assert_eq!(fake.events, ["method"]);
    assert!(out.contains("pass --method"), "{out}");
}

#[tokio::test]
async fn oc_upgrade_rejects_unsupported_methods_before_attempting_an_upgrade() {
    let mut fake = Fake::new();
    let (_, ok) = cli(&mut fake, &["--method", "apt"]).await;
    assert!(!ok);
    assert!(fake.events.is_empty());
}

#[tokio::test]
async fn oc_upgrade_reports_version_lookup_failures_without_installing() {
    let mut fake = Fake::new();
    fake.latest_error = true;
    let (out, ok) = cli(&mut fake, &[]).await;
    assert!(!ok);
    assert_eq!(fake.events, ["method", "latest"]);
    assert!(out.contains("Update check failed"), "{out}");
}

#[tokio::test]
async fn oc_upgrade_reports_installation_failures_with_a_nonzero_exit_code() {
    let mut fake = Fake::new();
    fake.install_error = true;
    let (out, ok) = cli(&mut fake, &[]).await;
    assert!(!ok);
    assert!(out.contains("Upgrade failed"), "{out}");
    assert!(out.contains("Permission denied"), "{out}");
    assert!(!out.contains("Upgrade complete"), "{out}");
}

// CodeSmith's own: package-manager installs name their command and never
// reach the network, even with --check off; --check only reports.
#[tokio::test]
async fn package_manager_installs_name_their_command_and_check_only_reports() {
    for (method, command) in [(Method::Brew, BREW), (Method::Cargo, CARGO)] {
        let mut fake = Fake::new();
        fake.detected = Detected::Method(method);
        let (out, ok) = cli(&mut fake, &[]).await;
        assert!(!ok);
        assert_eq!(fake.events, ["method"]);
        assert!(out.contains(command), "{out}");

        let mut fake = Fake::new();
        fake.detected = Detected::Method(method);
        let (out, ok) = cli(&mut fake, &["--check"]).await;
        assert!(ok, "{out}");
        assert_eq!(fake.events, ["method", "latest"]);
        assert!(out.contains("0.0.0-beta-new is available"), "{out}");
        assert!(out.contains(command), "{out}");
    }
    let mut fake = Fake::new();
    fake.detected = Detected::Source;
    let (out, ok) = cli(&mut fake, &[]).await;
    assert!(!ok);
    assert!(out.contains("source checkout"), "{out}");
}

// ---------------------------------------------------------------- the fixture server

type Route = (u16, Vec<(String, String)>, Vec<u8>);
/// (request line, user agent, every header name).
type Seen = (String, String, Vec<String>);

#[derive(Clone, Default)]
struct Server {
    routes: Arc<Mutex<HashMap<String, Route>>>,
    requests: Arc<Mutex<Vec<Seen>>>,
    base: String,
}

impl Server {
    fn start() -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut server = Server {
            base: format!(
                "http://127.0.0.1:{}/releases",
                listener.local_addr().unwrap().port()
            ),
            ..Default::default()
        };
        let s = server.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                let (mut agent, mut names) = (String::new(), Vec::new());
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = h.split_once(':') {
                        let k = k.trim().to_ascii_lowercase();
                        if k == "user-agent" {
                            agent = v.trim().to_string();
                        }
                        names.push(k);
                    }
                }
                let line = line.trim().to_string();
                let path = line.split(' ').nth(1).unwrap_or("").to_string();
                s.requests.lock().unwrap().push((line, agent, names));
                let (status, headers, body) =
                    s.routes.lock().unwrap().get(&path).cloned().unwrap_or((
                        404,
                        vec![],
                        b"not found".to_vec(),
                    ));
                let mut response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                    body.len()
                );
                for (k, v) in headers {
                    response.push_str(&format!("{k}: {v}\r\n"));
                }
                response.push_str("\r\n");
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        server.base = s.base;
        server
    }

    fn route(&self, path: &str, status: u16, headers: &[(&str, &str)], body: &[u8]) {
        self.routes.lock().unwrap().insert(
            path.to_string(),
            (
                status,
                headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                body.to_vec(),
            ),
        );
    }

    fn requests(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.0.clone())
            .collect()
    }

    /// Publish `version` for `target` with a fake binary that reports
    /// `reports`; returns the tarball's name.
    fn publish(&self, version: &str, target: &str, reports: &str, sums: Option<&str>) -> String {
        let work = tempfile::tempdir().unwrap();
        let name = format!("codesmith-{version}-{target}");
        std::fs::create_dir(work.path().join(&name)).unwrap();
        let bin = work.path().join(&name).join("codesmith");
        std::fs::write(&bin, format!("#!/bin/sh\necho 'codesmith {reports}'\n")).unwrap();
        std::fs::write(work.path().join(&name).join("LICENSE"), "MIT").unwrap();
        let file = format!("{name}.tar.gz");
        let status = std::process::Command::new("tar")
            .env("COPYFILE_DISABLE", "1")
            .arg("-C")
            .arg(work.path())
            .args(["-czf", &file, &name])
            .current_dir(work.path())
            .status()
            .unwrap();
        assert!(status.success());
        let tarball = std::fs::read(work.path().join(&file)).unwrap();
        let sum = hex(&Sha256::digest(&tarball));
        let dir = format!("/releases/download/v{version}");
        self.route(&format!("{dir}/{file}"), 200, &[], &tarball);
        let sums = sums
            .map(str::to_string)
            .unwrap_or_else(|| format!("{sum}  {file}\n0000  codesmith-{version}-other.tar.gz\n"));
        self.route(&format!("{dir}/SHA256SUMS"), 200, &[], sums.as_bytes());
        file
    }
}

const T: &str = "aarch64-apple-darwin";

/// An installed copy in a fresh folder that reports `0.1.0`.
fn installed() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let exe = bin.join("codesmith");
    std::fs::write(&exe, "#!/bin/sh\necho 'codesmith 0.1.0'\n").unwrap();
    let exe = exe.canonicalize().unwrap();
    (dir, exe)
}

fn leftovers(exe: &Path) -> Vec<String> {
    std::fs::read_dir(exe.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n != "codesmith")
        .collect()
}

// ---------------------------------------------------------------- updater-install.test.ts

// "${method} installs the explicit V2 package version without a leading v":
// a `v` target downloads the tarball named without it.
#[tokio::test]
async fn oc_installs_the_explicit_version_without_a_leading_v() {
    let server = Server::start();
    let file = server.publish("2.3.4-beta.1", T, "2.3.4-beta.1", None);
    let (_dir, exe) = installed();
    install(&server.base, "0.1.0", Some(T), &exe, "v2.3.4-beta.1")
        .await
        .unwrap();
    assert!(std::fs::read_to_string(&exe)
        .unwrap()
        .contains("2.3.4-beta.1"));
    assert_eq!(
        server.requests(),
        [
            "GET /releases/download/v2.3.4-beta.1/SHA256SUMS HTTP/1.1".to_string(),
            format!("GET /releases/download/v2.3.4-beta.1/{file} HTTP/1.1"),
        ]
    );
    assert!(leftovers(&exe).is_empty(), "{:?}", leftovers(&exe));
}

// "npm upgrades fail when the running copy keeps its old version": here the
// downloaded binary must report the version asked for before it replaces
// anything.
#[tokio::test]
async fn oc_upgrades_fail_when_the_new_binary_reports_another_version() {
    let server = Server::start();
    server.publish("2.0.0", T, "1.9.0", None);
    let (_dir, exe) = installed();
    let e = install(&server.base, "0.1.0", Some(T), &exe, "2.0.0")
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("not codesmith 2.0.0"), "{e:#}");
    assert!(std::fs::read_to_string(&exe).unwrap().contains("0.1.0"));
    assert!(leftovers(&exe).is_empty());
}

// "${manager.label} installs print the ${manager.name} command instead of
// updating in place", "Homebrew Core installs check and upgrade the Core
// formula" and "Homebrew tap installs of ${formula} use the published tap
// formula": Homebrew and cargo installs are detected by path and never
// replaced, even when the binary method is forced.
#[tokio::test]
async fn oc_package_manager_installs_print_their_command_instead_of_updating_in_place() {
    let server = Server::start();
    let dir = tempfile::tempdir().unwrap();
    let cellar = dir.path().join("Cellar/codesmith/0.1.0/bin");
    std::fs::create_dir_all(&cellar).unwrap();
    let exe = cellar.join("codesmith");
    std::fs::write(&exe, "old").unwrap();
    assert_eq!(detect(&exe, &[]), Detected::Method(Method::Brew));
    let e = install(&server.base, "0.1.0", Some(T), &exe, "2.0.0")
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains(BREW), "{e:#}");
    assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old");
    assert!(server.requests().is_empty());

    let cargo = dir.path().join("cargo/bin");
    assert_eq!(
        detect(&cargo.join("codesmith"), std::slice::from_ref(&cargo)),
        Detected::Method(Method::Cargo)
    );

    // A copy in a system package manager's folder is refused before any
    // request or file is touched (the path is only classified).
    for system in [
        "/usr/bin/codesmith",
        "/nix/store/abc-codesmith/bin/codesmith",
    ] {
        let e = install(&server.base, "0.1.0", Some(T), Path::new(system), "2.0.0")
            .await
            .unwrap_err();
        assert!(e.to_string().contains("system package manager"), "{e:#}");
    }
    assert!(server.requests().is_empty());
}

// "bun isolates and removes its install cache after exit ${exitCode}" and
// "curl uses the V2 installer and cleans its directory: ${failure}": the
// download folder is removed after success, a failed download and a bad
// checksum, and a failure leaves the old binary.
#[tokio::test]
async fn oc_cleans_its_directory_after_success_and_failure() {
    for failure in ["success", "download", "checksum"] {
        let server = Server::start();
        let sums = match failure {
            "checksum" => Some(format!("{}  codesmith-2.0.0-{T}.tar.gz\n", "ab".repeat(32))),
            _ => None,
        };
        let file = server.publish("2.0.0", T, "2.0.0", sums.as_deref());
        if failure == "download" {
            server.route(&format!("/releases/download/v2.0.0/{file}"), 500, &[], b"");
        }
        let (_dir, exe) = installed();
        let result = install(&server.base, "0.1.0", Some(T), &exe, "2.0.0").await;
        assert_eq!(
            result.is_ok(),
            failure == "success",
            "{failure}: {result:?}"
        );
        let expect = if failure == "success" {
            "2.0.0"
        } else {
            "0.1.0"
        };
        assert!(
            std::fs::read_to_string(&exe).unwrap().contains(expect),
            "{failure}"
        );
        assert!(leftovers(&exe).is_empty(), "{failure}");
        if failure == "checksum" {
            assert!(format!("{:#}", result.unwrap_err()).contains("checksum mismatch"));
        }
    }
}

// "invalid version targets never execute a command or create a cache".
#[tokio::test]
async fn oc_invalid_version_targets_never_make_a_request_or_create_a_folder() {
    let server = Server::start();
    let (_dir, exe) = installed();
    for v in [
        "",
        "latest",
        "2.3",
        "01.2.3",
        "vv2.3.4",
        "2.3.4; echo unsafe",
        "--global",
        "v2.3.4\n--force",
        "../../2.3.4",
    ] {
        let e = install(&server.base, "0.1.0", Some(T), &exe, v)
            .await
            .unwrap_err();
        assert_eq!(e.to_string(), format!("Invalid version: {v}"));
    }
    assert!(server.requests().is_empty());
    assert!(leftovers(&exe).is_empty());
}

// "install failures expose stderr and process errors do not report
// success": HTTP failures and an unlisted tarball are reported as such.
#[tokio::test]
async fn oc_install_failures_say_what_failed() {
    let server = Server::start();
    let (_dir, exe) = installed();
    let e = install(&server.base, "0.1.0", Some(T), &exe, "2.0.0")
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("returned HTTP 404"), "{e:#}");
    server.publish("2.0.0", T, "2.0.0", Some("00  something-else.tar.gz\n"));
    let e = install(&server.base, "0.1.0", Some(T), &exe, "2.0.0")
        .await
        .unwrap_err();
    assert!(
        format!("{e:#}").contains("is not listed in SHA256SUMS"),
        "{e:#}"
    );
    let e = install(&server.base, "0.1.0", None, &exe, "2.0.0")
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("no CodeSmith releases"), "{e:#}");
}

// "method detection identifies ${method} using the V2 package": by path.
#[test]
fn oc_method_detection_identifies_the_installation() {
    let p = Path::new;
    let none: &[PathBuf] = &[];
    assert_eq!(
        detect(
            p("/opt/homebrew/Cellar/codesmith/0.2.0/bin/codesmith"),
            none
        ),
        Detected::Method(Method::Brew)
    );
    assert_eq!(
        detect(
            p("/home/linuxbrew/.linuxbrew/Cellar/codesmith/0.2.0/bin/codesmith"),
            none
        ),
        Detected::Method(Method::Brew)
    );
    assert_eq!(
        detect(
            p("/home/me/.cargo/bin/codesmith"),
            &[PathBuf::from("/home/me/.cargo/bin")]
        ),
        Detected::Method(Method::Cargo)
    );
    assert_eq!(
        detect(p("/src/codesmith/target/release/codesmith"), none),
        Detected::Source
    );
    assert_eq!(
        detect(
            p("/src/codesmith/target/aarch64-apple-darwin/debug/codesmith"),
            none
        ),
        Detected::Source
    );
    assert_eq!(
        detect(p("/src/codesmith/target/debug/deps/codesmith-1234"), none),
        Detected::Source
    );
    assert_eq!(detect(p("/usr/bin/codesmith"), none), Detected::System);
    assert_eq!(
        detect(p("/nix/store/abc-codesmith/bin/codesmith"), none),
        Detected::System
    );
    for binary in ["/home/me/.local/bin/codesmith", "/usr/local/bin/codesmith"] {
        assert_eq!(detect(p(binary), none), Detected::Method(Method::Binary));
    }
}

// ---------------------------------------------------------------- GitHub releases and the check

#[tokio::test]
async fn the_latest_release_is_where_releases_latest_redirects_with_a_plain_get() {
    let server = Server::start();
    let tag = format!("{}/tag/v0.3.0", server.base);
    server.route("/releases/latest", 302, &[("Location", &tag)], b"");
    assert_eq!(
        latest_release(&server.base, "0.1.0").await.unwrap(),
        "0.3.0"
    );
    let requests = server.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1, "the redirect is not followed");
    let (line, agent, names) = &requests[0];
    assert_eq!(line, "GET /releases/latest HTTP/1.1");
    assert_eq!(agent, "codesmith/0.1.0");
    for name in names {
        assert!(
            ["host", "user-agent", "accept", "accept-encoding"].contains(&name.as_str()),
            "unexpected header {name}"
        );
    }
    // No release yet: GitHub redirects to the releases page.
    let releases = server.base.clone();
    server.route("/releases/latest", 302, &[("Location", &releases)], b"");
    let e = latest_release(&server.base, "0.1.0").await.unwrap_err();
    assert!(e.to_string().contains("no published release"), "{e}");
}

#[tokio::test]
async fn releases_must_use_https_unless_they_are_on_loopback() {
    let e = latest_release("http://example.com/releases", "0.1.0")
        .await
        .unwrap_err();
    assert!(e.to_string().contains("must use https"), "{e}");
}

#[test]
fn update_checks_are_off_unless_the_global_config_opts_in() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(policy(dir.path()), Policy::Disable);
    std::fs::write(dir.path().join("codesmith.json"), r#"{"update": "notify"}"#).unwrap();
    assert_eq!(policy(dir.path()), Policy::Notify);
    // codesmith.jsonc loads after codesmith.json, so it wins.
    std::fs::write(
        dir.path().join("codesmith.jsonc"),
        "{ // off again\n \"autoupdate\": false }",
    )
    .unwrap();
    assert_eq!(policy(dir.path()), Policy::Disable);
}

#[tokio::test]
async fn the_check_runs_at_most_daily_and_never_waits_for_the_network() {
    let server = Server::start();
    let tag = format!("{}/tag/v0.3.0", server.base);
    server.route("/releases/latest", 302, &[("Location", &tag)], b"");
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("update-check.json");

    let check = start_with("0.1.0", Method::Binary, &server.base, &cache, 1000);
    for _ in 0..200 {
        if check.task.as_ref().is_some_and(|t| t.is_finished()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        check.notice().await.as_deref(),
        Some("CodeSmith 0.3.0 is available (this is 0.1.0). Run: codesmith upgrade")
    );
    assert_eq!(server.requests().len(), 1);

    // Within a day the cached result is used, with no request.
    let check = start_with("0.1.0", Method::Brew, &server.base, &cache, 2000);
    assert!(check.task.is_none());
    assert!(check.notice().await.unwrap().ends_with(BREW));
    assert_eq!(server.requests().len(), 1);
    // Up to date: nothing to say.
    let check = start_with("0.3.0", Method::Binary, &server.base, &cache, 2000);
    assert_eq!(check.notice().await, None);
}

#[test]
fn extract_reads_pax_and_gnu_long_names() {
    // A GNU long-name entry naming a long path, then the file itself.
    let long = format!("{}/codesmith", "d".repeat(120));
    let mut tar = Vec::new();
    let header = |name: &str, kind: u8, size: usize| {
        let mut h = vec![0u8; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        let s = format!("{size:011o}");
        h[124..135].copy_from_slice(s.as_bytes());
        h[156] = kind;
        h
    };
    let pad = |v: &mut Vec<u8>| v.resize(v.len().div_ceil(512) * 512, 0);
    tar.extend(header("././@LongLink", b'L', long.len() + 1));
    tar.extend(long.as_bytes());
    tar.push(0);
    pad(&mut tar);
    tar.extend(header("short", b'0', 3));
    tar.extend(b"bin");
    pad(&mut tar);
    let pax = "20 path=x/codesmith\n".to_string();
    assert_eq!(pax.len(), 20);
    tar.extend(header("PaxHeader", b'x', pax.len()));
    tar.extend(pax.as_bytes());
    pad(&mut tar);
    tar.extend(header("ignored", b'0', 2));
    tar.extend(b"px");
    pad(&mut tar);
    tar.extend(vec![0u8; 1024]);
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&tar).unwrap();
    let tgz = gz.finish().unwrap();
    assert_eq!(extract(&tgz, &long).unwrap(), b"bin");
    assert_eq!(extract(&tgz, "x/codesmith").unwrap(), b"px");
    assert!(extract(&tgz, "short").is_err());
}
