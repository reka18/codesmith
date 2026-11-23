//! Tests ported from OpenCode's `cli/src/services/updater.test.ts`,
//! `cli/test/upgrade.test.ts` and `cli/test/updater-install.test.ts`
//! (MIT). Network tests run against a local fixture server, never GitHub.

use super::*;
use std::collections::HashMap;
use std::io::BufRead;
use std::net::TcpListener;
use std::sync::{Arc, Mutex, OnceLock};

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
    /// What `latest` returns, when not the default.
    latest: Option<String>,
}

impl Fake {
    fn new() -> Self {
        Fake {
            events: Vec::new(),
            detected: Detected::Method(Method::Binary),
            latest_error: false,
            install_error: false,
            latest: None,
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
        // Newer than the running "0.0.0-beta-old" in semver order.
        Ok(self
            .latest
            .clone()
            .unwrap_or_else(|| "0.0.1-beta-new".into()))
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
    assert_eq!(fake.events, ["method", "latest", "upgrade 0.0.1-beta-new"]);
    assert!(out.contains("Upgrade complete"), "{out}");
    assert!(out.contains("CodeSmith forged successfully"), "{out}");
}

#[tokio::test]
async fn oc_upgrade_resolves_the_latest_version_for_an_explicit_method_without_detection() {
    let mut fake = Fake::new();
    fake.detected = Detected::Unknown;
    let (out, ok) = cli(&mut fake, &["--method", "binary"]).await;
    assert!(ok, "{out}");
    assert_eq!(fake.events, ["latest", "upgrade 0.0.1-beta-new"]);
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

/// Security review 8, finding 8: with no version named, `codesmith
/// upgrade` moved to whatever the releases page called latest, an older
/// release included. It now refuses to go back unless the version is named.
#[tokio::test]
async fn upgrade_never_moves_to_an_older_latest_unasked() {
    for older in [
        "0.0.0-alpha",
        "0.0.0-beta-ol",
        "0.0.0-beta",
        "0.0.0-1",
        "v0.0.0-beta-nld",
    ] {
        let mut fake = Fake::new();
        fake.latest = Some(older.into());
        let (out, ok) = cli(&mut fake, &[]).await;
        assert!(!ok, "{older}: {out}");
        assert_eq!(fake.events, ["method", "latest"], "{older}");
        assert!(out.contains("not downgrading"), "{older}: {out}");
        assert!(out.contains("codesmith upgrade "), "{out}");
        // --check only says so.
        let mut fake = Fake::new();
        fake.latest = Some(older.into());
        let (out, ok) = cli(&mut fake, &["--check"]).await;
        assert!(ok, "{older}: {out}");
        assert!(out.contains("newer than the latest release"), "{out}");
        assert!(!fake.events.iter().any(|e| e.starts_with("upgrade")));
    }
    // Named explicitly, an older version is a rollback the user asked for.
    let mut fake = Fake::new();
    let (out, ok) = cli(&mut fake, &["0.0.0-alpha", "-m", "binary"]).await;
    assert!(ok, "{out}");
    assert_eq!(fake.events, ["upgrade 0.0.0-alpha"]);
    // The same version is still skipped; a newer one still installs.
    for (latest, installs) in [
        ("0.0.0-beta-old", false),
        ("v0.0.0-beta-old+build", false),
        ("0.0.0-beta-old.1", true),
        ("0.0.0", true),
    ] {
        let mut fake = Fake::new();
        fake.latest = Some(latest.into());
        let (out, ok) = cli(&mut fake, &[]).await;
        assert!(ok, "{latest}: {out}");
        assert_eq!(
            fake.events.iter().any(|e| e.starts_with("upgrade")),
            installs,
            "{latest}: {out}"
        );
    }
}

#[test]
fn releases_order_as_semver_does() {
    let ordered = [
        "0.9.9",
        "1.0.0-0",
        "1.0.0-1",
        "1.0.0-10",
        "1.0.0-alpha",
        "1.0.0-alpha.1",
        "1.0.0-alpha.beta",
        "1.0.0-beta",
        "1.0.0-beta.2",
        "1.0.0-beta.11",
        "1.0.0-rc.1",
        "1.0.0",
        "1.0.1",
        "1.1.0",
        "1.10.0",
        "2.0.0",
        "9007199254740991.0.0",
    ];
    for (i, a) in ordered.iter().enumerate() {
        for (j, b) in ordered.iter().enumerate() {
            assert_eq!(is_older(a, b), i < j, "{a} < {b}");
        }
    }
    // Build metadata and a leading `v` do not count; invalid versions are
    // never older.
    assert!(!is_older("v1.0.0+x", "1.0.0"));
    assert!(!is_older("1.0.0", "v1.0.0+y"));
    assert!(!is_older("junk", "1.0.0"));
    assert!(!is_older("1.0.0", "junk"));
    assert!(
        !is_older("１.0.0", "2.0.0"),
        "full-width digits are not a version"
    );
}

#[tokio::test]
async fn the_update_notice_never_offers_an_older_release() {
    let check = |latest: &str| Check {
        current: "1.2.0".into(),
        method: Method::Binary,
        cached: Some(latest.into()),
        task: None,
    };
    assert_eq!(check("1.1.9").notice().await, None);
    assert_eq!(check("1.2.0-rc.1").notice().await, None);
    assert_eq!(check("1.2.0").notice().await, None);
    assert!(check("1.2.1")
        .notice()
        .await
        .unwrap()
        .contains("1.2.1 is available"));
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
    assert!(!out.contains("forged successfully"), "{out}");
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
        assert!(out.contains("0.0.1-beta-new is available"), "{out}");
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
        let signature =
            test_key().sign(sums.as_bytes(), &format!("codesmith {version} SHA256SUMS"));
        self.route(
            &format!("{dir}/SHA256SUMS.minisig"),
            200,
            &[],
            signature.as_bytes(),
        );
        file
    }
}

// ---------------------------------------------------------------- test signing keys

/// A throwaway minisign key pair, generated when a test runs; it signs
/// nothing but fixtures. Signatures are made independently of the verifier
/// (ring's Ed25519 over a BLAKE2b-512 prehash, from the `blake2` crate) in
/// minisign's format: `Ed`/`ED`, an 8-byte key id, then the key or signature,
/// and a global signature over the signature and the trusted comment.
struct TestKey {
    pair: ring::signature::Ed25519KeyPair,
    id: [u8; 8],
}

impl TestKey {
    fn generate() -> TestKey {
        use ring::rand::SecureRandom;
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let mut id = [0u8; 8];
        rng.fill(&mut id).unwrap();
        TestKey {
            pair: ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap(),
            id,
        }
    }

    /// The line a minisign `.pub` file (and `release_key.pub`) holds.
    fn public_line(&self) -> String {
        use ring::signature::KeyPair;
        let mut bin = b"Ed".to_vec();
        bin.extend_from_slice(&self.id);
        bin.extend_from_slice(self.pair.public_key().as_ref());
        b64(&bin)
    }

    fn trust(&self) -> Trust {
        Trust::Keys(vec![minisign_verify::PublicKey::from_base64(
            &self.public_line(),
        )
        .unwrap()])
    }

    /// A `.minisig` for `data`, as `minisign -S -t COMMENT` makes it.
    fn sign(&self, data: &[u8], comment: &str) -> String {
        use blake2::Digest as _;
        self.signature(b"ED", &blake2::Blake2b512::digest(data), comment)
    }

    /// A legacy (not prehashed) signature, as `minisign -S -l` makes it.
    fn sign_legacy(&self, data: &[u8], comment: &str) -> String {
        self.signature(b"Ed", data, comment)
    }

    fn signature(&self, algorithm: &[u8], signed: &[u8], comment: &str) -> String {
        let sig = self.pair.sign(signed);
        let mut bin = algorithm.to_vec();
        bin.extend_from_slice(&self.id);
        bin.extend_from_slice(sig.as_ref());
        let mut global = sig.as_ref().to_vec();
        global.extend_from_slice(comment.as_bytes());
        let global = self.pair.sign(&global);
        format!(
            "untrusted comment: signature from a CodeSmith test key\n{}\ntrusted comment: {comment}\n{}\n",
            b64(&bin),
            b64(global.as_ref())
        )
    }
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The key the fixture server signs every release with.
fn test_key() -> &'static TestKey {
    static KEY: OnceLock<TestKey> = OnceLock::new();
    KEY.get_or_init(TestKey::generate)
}

/// [`super::install`] trusting only [`test_key`], as a build with a real
/// key pinned would.
async fn install(
    base: &str,
    current: &str,
    target: Option<&str>,
    exe: &Path,
    version: &str,
) -> Result<()> {
    super::install(&test_key().trust(), base, current, target, exe, version).await
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
            "GET /releases/download/v2.3.4-beta.1/SHA256SUMS.minisig HTTP/1.1".to_string(),
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

// ---------------------------------------------------------------- release signatures

/// Install 2.0.0 trusting `trust`, expecting a refusal: returns the error,
/// and checks that nothing was written and the tarball never downloaded.
async fn refused(server: &Server, trust: &Trust) -> String {
    let (_dir, exe) = installed();
    let e = super::install(trust, &server.base, "0.1.0", Some(T), &exe, "2.0.0")
        .await
        .unwrap_err();
    assert!(std::fs::read_to_string(&exe).unwrap().contains("0.1.0"));
    assert!(leftovers(&exe).is_empty(), "{:?}", leftovers(&exe));
    assert!(
        !server.requests().iter().any(|r| r.contains(".tar.gz")),
        "{:?}",
        server.requests()
    );
    let e = format!("{e:#}");
    assert!(e.contains("nothing was changed"), "{e}");
    e
}

fn sign_release(server: &Server, version: &str, signature: &str) {
    server.route(
        &format!("/releases/download/v{version}/SHA256SUMS.minisig"),
        200,
        &[],
        signature.as_bytes(),
    );
}

fn sums_of(server: &Server, version: &str) -> Vec<u8> {
    server.routes.lock().unwrap()[&format!("/releases/download/v{version}/SHA256SUMS")]
        .2
        .clone()
}

#[tokio::test]
async fn a_release_signed_by_a_pinned_key_installs_and_the_signature_is_checked_first() {
    let server = Server::start();
    let file = server.publish("2.0.0", T, "2.0.0", None);
    // A trusted comment may carry more after the version, such as a time.
    let sums = sums_of(&server, "2.0.0");
    let signature = test_key().sign(&sums, "codesmith 2.0.0 SHA256SUMS timestamp:1");
    sign_release(&server, "2.0.0", &signature);
    let (_dir, exe) = installed();
    install(&server.base, "0.1.0", Some(T), &exe, "2.0.0")
        .await
        .unwrap();
    assert!(std::fs::read_to_string(&exe).unwrap().contains("2.0.0"));
    assert_eq!(
        server.requests(),
        [
            "GET /releases/download/v2.0.0/SHA256SUMS HTTP/1.1".to_string(),
            "GET /releases/download/v2.0.0/SHA256SUMS.minisig HTTP/1.1".to_string(),
            format!("GET /releases/download/v2.0.0/{file} HTTP/1.1"),
        ]
    );
}

#[tokio::test]
async fn a_signature_by_a_key_not_pinned_is_refused() {
    let server = Server::start();
    server.publish("2.0.0", T, "2.0.0", None);
    let e = refused(&server, &TestKey::generate().trust()).await;
    assert!(e.contains("not signed by a CodeSmith release key"), "{e}");
}

#[tokio::test]
async fn a_rotated_key_list_accepts_either_key() {
    let server = Server::start();
    server.publish("2.0.0", T, "2.0.0", None);
    let old = TestKey::generate();
    let text = format!(
        "# old, then new\n{}\n\n{}\n",
        old.public_line(),
        test_key().public_line()
    );
    let trust = parse_keys(&text).unwrap();
    let (_dir, exe) = installed();
    super::install(&trust, &server.base, "0.1.0", Some(T), &exe, "2.0.0")
        .await
        .unwrap();
    assert!(std::fs::read_to_string(&exe).unwrap().contains("2.0.0"));
}

#[tokio::test]
async fn a_tampered_sha256sums_is_refused() {
    let server = Server::start();
    server.publish("2.0.0", T, "2.0.0", None);
    // Signed as published, then a line changed: a matching bad tarball.
    let sums = String::from_utf8(sums_of(&server, "2.0.0")).unwrap();
    let evil = format!("{}  codesmith-2.0.0-{T}.tar.gz\n", "ab".repeat(32));
    server.route(
        "/releases/download/v2.0.0/SHA256SUMS",
        200,
        &[],
        evil.as_bytes(),
    );
    assert_ne!(sums, evil);
    let e = refused(&server, &test_key().trust()).await;
    assert!(e.contains("not signed by a CodeSmith release key"), "{e}");
}

#[tokio::test]
async fn a_tampered_trusted_comment_is_refused() {
    let server = Server::start();
    server.publish("2.0.0", T, "2.0.0", None);
    // 1.9.0's signed checksums, relabelled 2.0.0 without re-signing: the
    // file signature still holds, but the comment's global signature fails.
    let sums = sums_of(&server, "2.0.0");
    let signature = test_key().sign(&sums, "codesmith 1.9.0 SHA256SUMS");
    let forged = signature.replace(
        "trusted comment: codesmith 1.9.0 SHA256SUMS",
        "trusted comment: codesmith 2.0.0 SHA256SUMS",
    );
    assert_ne!(signature, forged);
    sign_release(&server, "2.0.0", &forged);
    let e = refused(&server, &test_key().trust()).await;
    assert!(e.contains("not signed by a CodeSmith release key"), "{e}");
}

#[tokio::test]
async fn a_signature_for_another_version_is_refused() {
    // An older release's genuine signed SHA256SUMS served as 2.0.0 (a
    // replay or a downgrade) fails on its trusted comment.
    for comment in [
        "codesmith 1.9.0 SHA256SUMS",
        "codesmith 2.0.0-beta.1 SHA256SUMS",
        "codesmith 2.0.00 SHA256SUMS",
        "codesmith 2.0.0 homebrew-formula",
        "codesmith 2.0.0",
        "codesmith 2.0.0 SHA256SUMSx",
        "timestamp:1700000000\tfile:SHA256SUMS\thashed",
        "",
    ] {
        let server = Server::start();
        server.publish("2.0.0", T, "2.0.0", None);
        let signature = test_key().sign(&sums_of(&server, "2.0.0"), comment);
        sign_release(&server, "2.0.0", &signature);
        let e = refused(&server, &test_key().trust()).await;
        assert!(e.contains("not codesmith 2.0.0"), "{comment:?}: {e}");
    }
}

#[tokio::test]
async fn a_missing_or_malformed_signature_is_refused_when_a_key_is_pinned() {
    let server = Server::start();
    server.publish("2.0.0", T, "2.0.0", None);
    server.route(
        "/releases/download/v2.0.0/SHA256SUMS.minisig",
        404,
        &[],
        b"",
    );
    let e = refused(&server, &test_key().trust()).await;
    assert!(e.contains("SHA256SUMS.minisig"), "{e}");
    assert!(e.contains("installs only signed releases"), "{e}");

    let sums = sums_of(&server, "2.0.0");
    for bad in [
        "".to_string(),
        "not a signature".to_string(),
        test_key().sign_legacy(&sums, "codesmith 2.0.0 SHA256SUMS"),
    ] {
        sign_release(&server, "2.0.0", &bad);
        let e = refused(&server, &test_key().trust()).await;
        assert!(e.contains("SHA256SUMS"), "{e}");
    }
}

#[tokio::test]
async fn with_the_placeholder_key_upgrades_check_checksums_only() {
    let server = Server::start();
    let file = server.publish("2.0.0", T, "2.0.0", None);
    // Even a bad signature is never fetched: nothing is signed yet.
    sign_release(&server, "2.0.0", "garbage");
    let trust = parse_keys("# no key yet\nPLACEHOLDER\n").unwrap();
    assert!(matches!(trust, Trust::Unsigned));
    let (_dir, exe) = installed();
    super::install(&trust, &server.base, "0.1.0", Some(T), &exe, "2.0.0")
        .await
        .unwrap();
    assert!(std::fs::read_to_string(&exe).unwrap().contains("2.0.0"));
    assert_eq!(
        server.requests(),
        [
            "GET /releases/download/v2.0.0/SHA256SUMS HTTP/1.1".to_string(),
            format!("GET /releases/download/v2.0.0/{file} HTTP/1.1"),
        ]
    );
    // Checksums are still checked.
    let (_dir, exe) = installed();
    server.route(
        "/releases/download/v2.0.0/SHA256SUMS",
        200,
        &[],
        format!("{}  {file}\n", "ab".repeat(32)).as_bytes(),
    );
    let e = super::install(&trust, &server.base, "0.1.0", Some(T), &exe, "2.0.0")
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("checksum mismatch"), "{e:#}");
    assert!(UNSIGNED.contains("aren't signed yet") && !UNSIGNED.contains('\n'));
}

#[test]
fn the_key_file_is_a_placeholder_or_keys_and_nothing_else() {
    // What this build compiled in parses.
    release_keys().unwrap();
    let key = test_key().public_line();
    assert!(matches!(parse_keys("PLACEHOLDER"), Ok(Trust::Unsigned)));
    assert!(matches!(parse_keys(&key), Ok(Trust::Keys(k)) if k.len() == 1));
    assert!(matches!(
        parse_keys(&format!("  {key}  \r\n# c\n{}\n", TestKey::generate().public_line())),
        Ok(Trust::Keys(k)) if k.len() == 2
    ));
    for bad in [
        String::new(),
        "# only comments\n".to_string(),
        format!("PLACEHOLDER\n{key}"),
        "RWnotakey".to_string(),
        format!("{key}x"),
        "untrusted comment: minisign public key 0123".to_string(),
    ] {
        assert!(parse_keys(&bad).is_err(), "{bad:?}");
    }
}

#[test]
fn a_signature_made_by_the_minisign_tool_verifies() {
    // A key pair and signature of b"test" made by the minisign command (the
    // test vector of the minisign-verify crate, not a CodeSmith key), so the
    // fixture signer above is not the only source of signatures checked.
    let keys = match parse_keys("RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3").unwrap()
    {
        Trust::Keys(keys) => keys,
        Trust::Unsigned => unreachable!(),
    };
    let signature = "untrusted comment: signature from minisign secret key\n\
        RUQf6LRCGA9i559r3g7V1qNyJDApGip8MfqcadIgT9CuhV3EMhHoN1mGTkUidF/z7SrlQgXdy8ofjb7bNJJylDOocrCo8KLzZwo=\n\
        trusted comment: timestamp:1556193335\tfile:test\n\
        y/rUw2y8/hOUYjZU71eHp/Wo1KZ40fGy2VJEDl34XMJM+TX48Ss/17u3IvIfbVR1FkZZSNCisQbuQY+bHwhEBg==\n";
    // The signature holds, so only its comment (minisign's default, which
    // names no version) is refused.
    let e = verify_sums(&keys, b"test", signature.as_bytes(), "2.0.0").unwrap_err();
    assert!(e.to_string().contains("not codesmith 2.0.0"), "{e}");
    let e = verify_sums(&keys, b"tesT", signature.as_bytes(), "2.0.0").unwrap_err();
    assert!(e.to_string().contains("not signed by"), "{e}");
}

/// install.sh can't read release_key.pub, so it lists the keys itself.
fn install_sh() -> String {
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../install.sh")).unwrap()
}

#[test]
fn install_sh_pins_the_same_keys_as_codesmith_upgrade() {
    let script = install_sh();
    let line = script
        .lines()
        .find(|l| l.starts_with("RELEASE_KEYS="))
        .expect("install.sh sets RELEASE_KEYS");
    let listed: Vec<&str> = line["RELEASE_KEYS=".len()..]
        .trim_matches('"')
        .split_whitespace()
        .collect();
    let pinned: Vec<&str> = RELEASE_KEYS
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#') && *l != "PLACEHOLDER")
        .collect();
    assert_eq!(listed, pinned, "update RELEASE_KEYS in install.sh");
}

// install.sh checks the signature with minisign when it is installed. The
// script is run with a key substituted for RELEASE_KEYS and a stand-in
// `minisign` that reports what the real one would (its checks are its own).
fn run_install_sh(
    server: &Server,
    keys: &str,
    minisign: Option<&str>,
) -> (bool, String, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let script: String = install_sh()
        .lines()
        .map(|l| {
            if l.starts_with("RELEASE_KEYS=") {
                format!("RELEASE_KEYS=\"{keys}\"\n")
            } else {
                format!("{l}\n")
            }
        })
        .collect();
    assert!(script.contains(&format!("RELEASE_KEYS=\"{keys}\"")));
    std::fs::write(dir.path().join("install.sh"), script).unwrap();
    let tools = dir.path().join("tools");
    std::fs::create_dir(&tools).unwrap();
    // Only the system's tools, so a real minisign on this machine is unseen.
    let mut path = format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", tools.display());
    if let Some(body) = minisign {
        let stub = tools.join("minisign");
        std::fs::write(&stub, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    } else {
        path = "/usr/bin:/bin:/usr/sbin:/sbin".to_string();
    }
    let output = std::process::Command::new("/bin/sh")
        .arg(dir.path().join("install.sh"))
        .env_clear()
        .env("PATH", path)
        .env("HOME", dir.path())
        .env("CODESMITH_VERSION", "2.0.0")
        .env("CODESMITH_INSTALL_DIR", dir.path().join("bin"))
        .env(
            "CODESMITH_BASE_URL",
            format!("{}/download/v2.0.0", server.base),
        )
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stderr).into_owned();
    (output.status.success(), text, dir)
}

#[test]
fn install_sh_checks_the_signature_when_minisign_is_installed() {
    if !Path::new("/usr/bin/curl").exists() || !Path::new("/usr/bin/tar").exists() {
        return; // install.sh needs curl and tar from the system.
    }
    // T is a macOS target; install.sh picks this machine's, so publish it.
    let target = TARGET.unwrap_or(T);
    let server = Server::start();
    server.publish("2.0.0", target, "2.0.0", None);
    let key = test_key().public_line();
    let installed = |dir: &tempfile::TempDir| dir.path().join("bin/codesmith").exists();
    let good = "echo 'codesmith 2.0.0 SHA256SUMS timestamp:1'";

    // Placeholder: checksums only, with a note.
    let (ok, err, dir) = run_install_sh(&server, "", Some("exit 1"));
    assert!(ok && installed(&dir), "{err}");
    assert!(err.contains("not signed yet"), "{err}");

    // A key, no minisign here: skipped with a note.
    let (ok, err, dir) = run_install_sh(&server, &key, None);
    assert!(ok && installed(&dir), "{err}");
    assert!(err.contains("install minisign"), "{err}");

    // A key and minisign: verified, with the version checked.
    let (ok, err, dir) = run_install_sh(&server, &key, Some(good));
    assert!(ok && installed(&dir), "{err}");
    assert!(err.contains("signature ok"), "{err}");
    assert!(err.contains("CodeSmith forged successfully"), "{err}");
    assert!(
        !err.contains("Press Enter"),
        "no prompt without a terminal: {err}"
    );

    let (ok, err, dir) = run_install_sh(&server, &key, Some("exit 1"));
    assert!(!ok && !installed(&dir), "{err}");
    assert!(
        err.contains("not signed by a CodeSmith release key"),
        "{err}"
    );
    assert!(
        !err.contains("forged successfully"),
        "no banner on failure: {err}"
    );

    let (ok, err, dir) = run_install_sh(
        &server,
        &key,
        Some("echo 'codesmith 1.9.0 SHA256SUMS timestamp:1'"),
    );
    assert!(!ok && !installed(&dir), "{err}");
    assert!(err.contains("not codesmith 2.0.0"), "{err}");

    // A legacy signature (minisign -l) is refused, as codesmith upgrade does.
    let sums = sums_of(&server, "2.0.0");
    let legacy = test_key().sign_legacy(&sums, "codesmith 2.0.0 SHA256SUMS");
    sign_release(&server, "2.0.0", &legacy);
    let (ok, err, dir) = run_install_sh(&server, &key, Some(good));
    assert!(!ok && !installed(&dir), "{err}");
    assert!(err.contains("prehashed"), "{err}");

    server.route(
        "/releases/download/v2.0.0/SHA256SUMS.minisig",
        404,
        &[],
        b"",
    );
    let (ok, err, dir) = run_install_sh(&server, &key, Some(good));
    assert!(!ok && !installed(&dir), "{err}");
    assert!(err.contains("SHA256SUMS.minisig"), "{err}");
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

/// The scratch folder and the downloaded binary are the owner's alone
/// until the binary has been checked, whatever the umask would give.
#[cfg(unix)]
#[test]
fn scratch_files_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let scratch = dir.path().join(".codesmith-upgrade-test");
    private_dir(&scratch).unwrap();
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&scratch), 0o700);
    // Only a new folder: one already there is an error.
    assert!(private_dir(&scratch).is_err());
    let file = scratch.join("codesmith");
    drop(private_file(&file).unwrap());
    assert_eq!(mode(&file), 0o700);
    assert!(private_file(&file).is_err(), "only a new file");
}

/// The CodeSmith release key (`~/.minisign/codesmith.pub` on Skeptic-Mac).
const CODESMITH_KEY: &str = "RWTFKe6qhvSs57s1pw5x6aJRK0jdZr42tg1NgNp3a9rAJ3HersFsRoMV";

#[test]
fn this_build_pins_the_codesmith_release_key_and_only_it() {
    let pinned: Vec<&str> = RELEASE_KEYS
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    assert_eq!(pinned, [CODESMITH_KEY], "release_key.pub changed");
    assert!(matches!(release_keys(), Ok(Trust::Keys(k)) if k.len() == 1));
}

#[test]
fn a_test_key_signature_verifies_only_where_that_key_is_listed() {
    let sums = b"0000  codesmith-2.0.0-aarch64-apple-darwin.tar.gz\n";
    let comment = "codesmith 2.0.0 SHA256SUMS timestamp:1";
    let signature = test_key().sign(sums, comment);
    let keys = |text: &str| match parse_keys(text).unwrap() {
        Trust::Keys(keys) => keys,
        Trust::Unsigned => unreachable!(),
    };

    // The compiled-in key refuses a signature by any other key.
    let Trust::Keys(real) = release_keys().unwrap() else {
        panic!("no release key compiled in")
    };
    let e = verify_sums(&real, sums, signature.as_bytes(), "2.0.0").unwrap_err();
    assert!(e.to_string().contains("not signed by"), "{e}");

    // Listed alongside the real key (as while rotating), it verifies.
    let both = keys(&format!("{RELEASE_KEYS}\n{}\n", test_key().public_line()));
    assert_eq!(both.len(), 2);
    verify_sums(&both, sums, signature.as_bytes(), "2.0.0").unwrap();
    // But not for another version, nor for changed checksums.
    assert!(verify_sums(&both, sums, signature.as_bytes(), "2.0.1").is_err());
    assert!(verify_sums(&both, b"1111", signature.as_bytes(), "2.0.0").is_err());

    // Another test key, listed instead, does not verify it either.
    let other = keys(&format!(
        "{CODESMITH_KEY}\n{}\n",
        TestKey::generate().public_line()
    ));
    let e = verify_sums(&other, sums, signature.as_bytes(), "2.0.0").unwrap_err();
    assert!(e.to_string().contains("not signed by"), "{e}");
}

// ------------------------------------------------------------ banner

/// Terminal columns of a banner line: the fire emoji is two wide, the
/// text-presentation selector after the hammer takes none.
fn columns(line: &str) -> usize {
    line.chars()
        .map(|c| match c {
            '\u{fe0e}' | '\u{fe0f}' => 0,
            '🔥' => 2,
            _ => 1,
        })
        .sum()
}

#[test]
fn the_banner_is_a_closed_box_every_line_the_same_width() {
    let lines: Vec<&str> = BANNER.lines().collect();
    assert_eq!(lines.len(), 23);
    for l in &lines {
        assert_eq!(columns(l), 62, "{l:?}");
    }
    assert!(lines[0].starts_with('╔') && lines[0].ends_with('╗'));
    assert!(lines[22].starts_with('╚') && lines[22].ends_with('╝'));
    for l in &lines[1..22] {
        assert!(l.starts_with('║') && l.ends_with('║'), "{l:?}");
    }
    assert!(BANNER.contains("CodeSmith forged successfully"));
    assert!(BANNER.contains("[ Press Enter to continue ]"));
    // The hammer is drawn as text, one column wide like the TUI's.
    assert!(BANNER.contains("⚒\u{fe0e}"));
    assert!(BANNER.ends_with("╝\n"));
}

#[test]
fn install_sh_carries_the_same_banner() {
    let script = install_sh();
    let start = script.find("<<'BANNER'\n").expect("banner heredoc") + "<<'BANNER'\n".len();
    let end = start + script[start..].find("\nBANNER\n").unwrap() + 1;
    assert_eq!(
        &script[start..end],
        BANNER,
        "copy packaging/banner.txt into install.sh"
    );
    // Shown last, after the install succeeded.
    let main = script.rfind("\nmain() {").unwrap();
    let body = &script[main..];
    assert!(body.find("say \"installed codesmith").unwrap() < body.find("    banner\n").unwrap());
}

#[test]
fn the_homebrew_formula_carries_the_banner_without_the_prompt() {
    let formula = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../packaging/homebrew/codesmith.rb"
    ))
    .unwrap();
    let expected: String = banner(false)
        .lines()
        .map(|l| format!("      {l}\n"))
        .collect();
    assert!(formula.contains("  def caveats\n    <<~EOS\n"), "{formula}");
    assert!(
        formula.contains(&expected),
        "copy packaging/banner.txt into the formula's caveats"
    );
    assert!(!formula.contains("Press Enter"));
    // Nothing Ruby would interpolate in the heredoc.
    assert!(!expected.contains("#{") && !expected.contains('\\'));
}

#[test]
fn the_plain_banner_drops_the_prompt_and_keeps_the_box() {
    let plain = banner(false);
    assert!(!plain.contains("Press Enter"));
    assert!(!plain.contains('\x1b'));
    assert_eq!(plain.lines().count(), BANNER.lines().count());
    for l in plain.lines() {
        assert_eq!(columns(l), 62, "{l:?}");
    }
}

#[test]
fn the_coloured_banner_is_the_plain_one_with_colour_codes() {
    let coloured = banner(true);
    assert!(coloured.contains("\x1b[1;38;5;226m"), "fire on CODE");
    assert!(coloured.contains("\x1b[1;38;5;117m"), "mithril on SMITH");
    let mut stripped = String::new();
    let mut chars = coloured.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for d in chars.by_ref() {
                if d == 'm' {
                    break;
                }
            }
        } else {
            stripped.push(c);
        }
    }
    assert_eq!(stripped, banner(false));
    // Every colour is reset on the line it starts on.
    for l in coloured.lines() {
        assert!(l.ends_with("\x1b[0m"), "{l:?}");
    }
}
