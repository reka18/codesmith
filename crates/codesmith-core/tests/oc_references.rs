//! Ports of OpenCode's reference tests (MIT): references listed to the
//! model (`reference-instructions.test.ts`), configured sources
//! (`reference.test.ts`), repository parsing (`repository.test.ts`) and the
//! checkout cache (`repository-cache.test.ts`), which runs against local
//! bare repositories and never reaches the network. See
//! `crates/codesmith-core/PARITY.md`.

use codesmith_core::instruction_state::{self as is, Values};
use codesmith_core::reference::{self, Entry, Reference, Source};
use codesmith_core::repository::{self as repo, Cache, CacheError, Refresh, Status};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn read_initial(list: &is::List) -> (Values, String) {
    let delta = is::diff(&is::read(list), None).unwrap();
    let v = is::apply(&Values::new(), &delta);
    let text = is::join(&is::render_initial(list, &v).unwrap());
    (v, text)
}

fn read_update(list: &is::List, previous: &Values) -> (Values, String) {
    let delta = is::diff(&is::read(list), Some(previous)).unwrap();
    let text = is::render_update(list, previous, &delta).unwrap();
    (is::apply(previous, &delta), text)
}

fn local(name: &str, description: Option<&str>) -> Reference {
    let path = PathBuf::from(format!("/{name}"));
    Reference {
        name: name.into(),
        path: path.clone(),
        description: description.map(String::from),
        hidden: None,
        source: Source::Local {
            path,
            description: description.map(String::from),
            hidden: None,
        },
    }
}

// --- reference-instructions.test.ts ---

// reference-instructions.test.ts: "lists available references in the instructions"
#[test]
fn oc_reference_instructions_list_available_references() {
    let (_, text) = read_initial(&reference::source(&[local(
        "docs",
        Some("Use for product documentation"),
    )]));
    assert!(text.contains("<available_references>"));
    assert!(text.contains("<name>docs</name>"));
    assert!(text.contains("<path>/docs</path>"));
    assert!(text.contains("<description>Use for product documentation</description>"));
}

// reference-instructions.test.ts: "omits instructions when no references are available"
#[test]
fn oc_reference_instructions_are_omitted_without_references() {
    assert_eq!(read_initial(&reference::source(&[])).1, "");
}

// reference-instructions.test.ts: "omits references without descriptions"
#[test]
fn oc_reference_instructions_omit_references_without_descriptions() {
    assert_eq!(
        read_initial(&reference::source(&[local("docs", None)])).1,
        ""
    );
}

// reference-instructions.test.ts: "announces added and removed references as deltas"
#[test]
fn oc_reference_instructions_announce_added_and_removed_references() {
    let docs = local("docs", Some("Use for product documentation"));
    let examples = local("examples", Some("Use for examples"));
    let (v, _) = read_initial(&reference::source(std::slice::from_ref(&docs)));
    let (v, added) = read_update(&reference::source(&[docs, examples.clone()]), &v);
    assert_eq!(
        added,
        [
            "New project references are available in addition to those previously listed:",
            "  <reference>",
            "    <name>examples</name>",
            "    <path>/examples</path>",
            "    <description>Use for examples</description>",
            "  </reference>",
        ]
        .join("\n")
    );
    assert_eq!(
        read_update(&reference::source(&[examples]), &v).1,
        "The following project references are no longer available and must not be used: docs."
    );
    assert_eq!(
        read_update(&reference::source(&[]), &v).1,
        "Project reference guidance is no longer available. Do not use previously listed references."
    );
}

// reference-instructions.test.ts: "announces added, changed, and removed references in one delta"
#[test]
fn oc_reference_instructions_announce_one_delta_for_several_changes() {
    let (v, _) = read_initial(&reference::source(&[
        local("docs", Some("Use for product documentation")),
        local("legacy", Some("Use for old code")),
    ]));
    let (_, text) = read_update(
        &reference::source(&[
            local("docs", Some("Use for API documentation")),
            local("examples", Some("Use for examples")),
        ]),
        &v,
    );
    assert_eq!(
        text,
        [
            "New project references are available in addition to those previously listed:",
            "  <reference>",
            "    <name>examples</name>",
            "    <path>/examples</path>",
            "    <description>Use for examples</description>",
            "  </reference>",
            "The following project references have changed. Each entry replaces the previously listed reference with the same name:",
            "  <reference>",
            "    <name>docs</name>",
            "    <path>/docs</path>",
            "    <description>Use for API documentation</description>",
            "  </reference>",
            "The following project references are no longer available and must not be used: legacy.",
        ]
        .join("\n")
    );
}

// --- reference.test.ts ---

fn entries(v: serde_json::Value) -> BTreeMap<String, Entry> {
    serde_json::from_value(v).unwrap()
}

fn cache() -> Cache {
    Cache::new("/cache/repos", "/cache/state")
}

// reference.test.ts: "reads the current editor source by name"
// Adapted: config names each source; an invalid name is left out.
#[test]
fn oc_reference_reads_each_configured_source_by_name() {
    let e = entries(json!({
        "docs": "./docs",
        "sdk": "owner/repo",
        "notes": {"path": "~/notes"},
        "bad name": "./x",
        "a/b": "./y",
    }));
    let sources = reference::sources(&e, Path::new("/project"), Path::new("/home/me"));
    let get = |n: &str| sources.iter().find(|(k, _)| k == n).map(|(_, s)| s.clone());
    assert_eq!(
        get("docs"),
        Some(Source::Local {
            path: "/project/docs".into(),
            description: None,
            hidden: None
        })
    );
    assert_eq!(
        get("sdk"),
        Some(Source::Git {
            repository: "owner/repo".into(),
            branch: None,
            description: None,
            hidden: None
        })
    );
    assert!(
        matches!(get("notes"), Some(Source::Local { path, .. }) if path == Path::new("/home/me/notes"))
    );
    assert_eq!(get("bad name"), None);
    assert_eq!(get("a/b"), None);
}

// reference.test.ts: "registers normalized sources for the owning scope"
#[test]
fn oc_reference_lists_normalized_sources() {
    let e = entries(
        json!({"docs": {"path": "/docs/./api/..", "description": "Use for API documentation", "hidden": true}}),
    );
    let sources = reference::sources(&e, Path::new("/project"), Path::new("/home"));
    let source = Source::Local {
        path: "/docs/./api/..".into(),
        description: Some("Use for API documentation".into()),
        hidden: Some(true),
    };
    assert_eq!(sources, vec![("docs".to_string(), source.clone())]);
    assert_eq!(
        reference::list(&sources, &cache()),
        vec![Reference {
            name: "docs".into(),
            path: "/docs/./api/..".into(),
            description: Some("Use for API documentation".into()),
            hidden: Some(true),
            source,
        }]
    );
}

// reference.test.ts: "derives Git paths without exposing cache operations"
#[test]
fn oc_reference_derives_git_paths_without_cache_work() {
    let e = entries(json!({
        "sdk": {"repository": "owner/repo", "branch": "main"},
        "invalid": "invalid",
        "invalid-branch": {"repository": "owner/repo", "branch": "../escape"},
        "file": "file:///docs",
    }));
    let c = cache();
    let refs = reference::list(
        &reference::sources(&e, Path::new("/p"), Path::new("/h")),
        &c,
    );
    let r = repo::parse_remote("owner/repo").unwrap();
    assert_eq!(refs.len(), 1, "{refs:?}");
    assert_eq!(refs[0].name, "sdk");
    assert_eq!(refs[0].path, repo::cache_path(&c.root, &r, Some("main")));
    assert!(!Path::new("/cache").exists(), "nothing was cloned");
}

// reference.test.ts: "preserves configured Git descriptions"
#[test]
fn oc_reference_preserves_configured_git_descriptions() {
    let e = entries(
        json!({"sdk": {"repository": "owner/repo", "description": "Use for SDK implementation details"}}),
    );
    let c = cache();
    let refs = reference::list(
        &reference::sources(&e, Path::new("/p"), Path::new("/h")),
        &c,
    );
    assert_eq!(
        refs,
        vec![Reference {
            name: "sdk".into(),
            path: repo::cache_path(&c.root, &repo::parse_remote("owner/repo").unwrap(), None),
            description: Some("Use for SDK implementation details".into()),
            hidden: None,
            source: Source::Git {
                repository: "owner/repo".into(),
                branch: None,
                description: Some("Use for SDK implementation details".into()),
                hidden: None,
            },
        }]
    );
}

#[test]
fn references_are_read_from_config() {
    let c: codesmith_core::config::Config = serde_json::from_value(json!({
        "references": {"docs": "../docs", "sdk": {"repository": "gitlab.com/team/sdk", "branch": "v2"}}
    }))
    .unwrap();
    assert_eq!(c.references["docs"], Entry::Short("../docs".into()));
    assert!(matches!(&c.references["sdk"], Entry::Git { branch: Some(b), .. } if b == "v2"));
}

// --- repository.test.ts ---

// repository.test.ts: "parses github shorthand and builds an explicit-root cache path"
#[test]
fn oc_repository_parses_github_shorthand_and_builds_a_cache_path() {
    let r = repo::parse_remote("owner/repo").unwrap();
    assert_eq!(r.host, "github.com");
    assert_eq!(r.path, "owner/repo");
    assert_eq!(r.segments, ["owner", "repo"]);
    assert_eq!(r.owner.as_deref(), Some("owner"));
    assert_eq!(r.repo, "repo");
    assert_eq!(r.remote, "https://github.com/owner/repo.git");
    assert_eq!(r.label, "owner/repo");
    let root = Path::new("/cache");
    assert_eq!(
        repo::cache_path(root, &r, None),
        root.join("github.com/owner/repo")
    );
    assert_eq!(
        repo::cache_path(root, &r, Some("main")),
        root.join("github.com/owner/repo@main")
    );
    assert_eq!(
        repo::cache_path(root, &r, Some("feature/x")),
        root.join("github.com/owner/repo@feature%2Fx")
    );
    assert_eq!(repo::cache_identity(&r), "github.com/owner/repo");
}

// repository.test.ts: "parses host path and scp remote references"
#[test]
fn oc_repository_parses_host_path_and_scp_references() {
    let r = repo::parse_remote("gitlab.com/group/repo").unwrap();
    assert_eq!(
        (
            r.host.as_str(),
            r.path.as_str(),
            r.remote.as_str(),
            r.label.as_str()
        ),
        (
            "gitlab.com",
            "group/repo",
            "https://gitlab.com/group/repo.git",
            "gitlab.com/group/repo"
        )
    );
    let r = repo::parse_remote("git@github.com:owner/repo.git").unwrap();
    assert_eq!(
        (
            r.host.as_str(),
            r.path.as_str(),
            r.remote.as_str(),
            r.label.as_str()
        ),
        (
            "github.com",
            "owner/repo",
            "git@github.com:owner/repo.git",
            "owner/repo"
        )
    );
}

// repository.test.ts: "caches a host with a port under a directory per host part"
#[test]
fn oc_repository_caches_a_host_with_a_port_per_host_part() {
    let r = repo::parse_remote("ssh://git@example.com:2222/owner/repo").unwrap();
    assert_eq!(
        repo::cache_path(Path::new("/cache"), &r, None),
        Path::new("/cache/example.com/2222/owner/repo")
    );
}

// repository.test.ts: "keeps local file repositories distinct from remote repositories"
#[test]
fn oc_repository_keeps_local_file_repositories_apart() {
    let d = tempfile::tempdir().unwrap();
    let local = d.path().join("repo.git");
    let url = reqwest::Url::from_file_path(&local).unwrap().to_string();
    let r = repo::parse(&url).unwrap();
    assert_eq!(r.host, "file");
    assert_eq!(r.protocol.as_deref(), Some("file:"));
    assert_eq!(r.label, local.display().to_string());
    assert!(repo::is_file(&r) && !repo::is_remote(&r));
    assert!(matches!(
        repo::parse_remote(&url),
        Err(repo::Error::UnsupportedLocal { .. })
    ));
}

// repository.test.ts: "rejects unsafe remote references and branches with typed errors"
#[test]
fn oc_repository_rejects_unsafe_references_and_branches() {
    let invalid = |s: &str| {
        matches!(
            repo::parse_remote(s),
            Err(repo::Error::InvalidReference { .. })
        )
    };
    assert!(invalid("not-a-repo"));
    assert!(invalid("git@github.com:../../../etc/passwd"));
    assert!(repo::validate_branch("feature/docs.v1").is_ok());
    for b in ["-bad", "bad..branch", "bad branch"] {
        assert!(
            matches!(
                repo::validate_branch(b),
                Err(repo::Error::InvalidBranch { .. })
            ),
            "{b}"
        );
    }
}

// repository.test.ts: "rejects %s because its host contains a relative path segment" (7 cases)
#[test]
fn oc_repository_rejects_hosts_with_relative_path_segments() {
    for input in [
        "..:repo",
        "git@..:repo",
        "../owner/repo",
        "ssh://../repo",
        "ssh://..:22/repo",
        "https://%2e%2e/owner/repo",
        ".:repo",
    ] {
        assert!(
            matches!(
                repo::parse_remote(input),
                Err(repo::Error::InvalidReference { .. })
            ),
            "{input}: {:?}",
            repo::parse(input)
        );
    }
}

// repository.test.ts: "compares cache identity independent of input spelling"
#[test]
fn oc_repository_compares_identity_independent_of_spelling() {
    let short = repo::parse_remote("owner/repo").unwrap();
    assert!(repo::same(
        &short,
        &repo::parse_remote("https://github.com/owner/repo.git").unwrap()
    ));
    assert!(repo::same(
        &short,
        &repo::parse_remote("github.com/owner/repo").unwrap()
    ));
}

// --- repository-cache.test.ts, against local bare repositories ---

fn git(cwd: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@codesmith.test",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn has_git() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    source: PathBuf,
    remote: String,
    reference: repo::Reference,
}

impl Fixture {
    /// `origin.git` (a bare repository whose `main` says "one") and a
    /// clone of it to commit from.
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let origin = root.join("origin.git");
        let source = root.join("source");
        git(&root, &["init", "--bare", origin.to_str().unwrap()]);
        git(&root, &["init", source.to_str().unwrap()]);
        std::fs::write(source.join("README.md"), "one\n").unwrap();
        git(&source, &["add", "README.md"]);
        git(&source, &["commit", "-m", "initial"]);
        git(&source, &["branch", "-M", "main"]);
        let remote = reqwest::Url::from_file_path(&origin).unwrap().to_string();
        git(&source, &["remote", "add", "origin", &remote]);
        git(&source, &["push", "-u", "origin", "main"]);
        git(
            &root,
            &[
                "--git-dir",
                origin.to_str().unwrap(),
                "symbolic-ref",
                "HEAD",
                "refs/heads/main",
            ],
        );
        let reference = repo::Reference {
            remote: remote.clone(),
            ..repo::parse_remote("owner/repo").unwrap()
        };
        Fixture {
            _dir: dir,
            root,
            source,
            remote,
            reference,
        }
    }

    fn cache(&self) -> Cache {
        Cache::new(self.root.join("repos"), self.root.join("state"))
    }

    fn commit(&self, content: &str, message: &str) {
        std::fs::write(self.source.join("README.md"), content).unwrap();
        git(&self.source, &["add", "README.md"]);
        git(&self.source, &["commit", "-m", message]);
        git(&self.source, &["push"]);
    }

    fn branch(&self, name: &str, content: &str) {
        git(&self.source, &["checkout", "-b", name]);
        std::fs::write(self.source.join("README.md"), content).unwrap();
        git(&self.source, &["add", "README.md"]);
        git(&self.source, &["commit", "-m", name]);
        git(&self.source, &["push", "-u", "origin", name]);
    }

    fn ensure(&self, branch: Option<&str>, refresh: Refresh) -> Result<repo::Checkout, CacheError> {
        self.cache().ensure(&self.reference, branch, refresh)
    }
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p.join("README.md")).unwrap()
}

fn yesterday() -> serde_json::Value {
    json!({"attemptedAt": codesmith_core::store::now() - repo::REFRESH_INTERVAL_MS})
}

// repository-cache.test.ts: "persists the daily throttle across cache recreation and serializes competing refreshes"
#[test]
fn oc_repository_cache_persists_the_daily_throttle_and_serializes_refreshes() {
    if !has_git() {
        return;
    }
    let f = Fixture::new();
    let initial = f.ensure(None, Refresh::Daily).unwrap();
    assert_eq!(initial.status, Status::Cloned);
    f.commit("two\n", "advance main");
    // A new cache over the same folders keeps the throttle.
    assert_eq!(
        f.ensure(None, Refresh::Daily).unwrap().status,
        Status::Cached
    );
    assert_eq!(read(&initial.local_path), "one\n");
    f.cache()
        .set_attempt(&initial.local_path, Some(&yesterday()));
    let mut statuses: Vec<Status> = std::thread::scope(|s| {
        let a = s.spawn(|| f.ensure(None, Refresh::Daily).unwrap().status);
        let b = s.spawn(|| f.ensure(None, Refresh::Daily).unwrap().status);
        vec![a.join().unwrap(), b.join().unwrap()]
    });
    statuses.sort();
    assert_eq!(statuses, [Status::Cached, Status::Refreshed]);
    assert_eq!(read(&initial.local_path), "two\n");
    // A missing checkout is cloned again even when the attempt is recent.
    std::fs::remove_dir_all(&initial.local_path).unwrap();
    assert_eq!(
        f.ensure(None, Refresh::Daily).unwrap().status,
        Status::Cloned
    );
    assert_eq!(read(&initial.local_path), "two\n");
}

// repository-cache.test.ts: "honors legacy attempt records while allowing forced refreshes"
#[test]
fn oc_repository_cache_honors_older_attempt_records_and_forced_refreshes() {
    if !has_git() {
        return;
    }
    let f = Fixture::new();
    let initial = f.ensure(None, Refresh::Daily).unwrap();
    f.commit("two\n", "advance main");
    let now = codesmith_core::store::now();
    f.cache().set_attempt(
        &initial.local_path,
        Some(&json!({"attemptedAt": now, "refreshedAt": 0})),
    );
    assert_eq!(
        f.ensure(None, Refresh::Daily).unwrap().status,
        Status::Cached
    );
    assert_eq!(read(&initial.local_path), "one\n");
    assert_eq!(
        f.ensure(None, Refresh::Force).unwrap().status,
        Status::Refreshed
    );
    assert_eq!(read(&initial.local_path), "two\n");
    assert_eq!(
        f.ensure(None, Refresh::Daily).unwrap().status,
        Status::Cached
    );
}

// repository-cache.test.ts: "throttles failed refresh attempts until the next interval"
#[test]
fn oc_repository_cache_throttles_failed_refreshes() {
    if !has_git() {
        return;
    }
    let f = Fixture::new();
    let initial = f.ensure(None, Refresh::Daily).unwrap();
    f.cache()
        .set_attempt(&initial.local_path, Some(&yesterday()));
    std::fs::rename(f.root.join("origin.git"), f.root.join("offline.git")).unwrap();
    assert!(matches!(
        f.ensure(None, Refresh::Daily),
        Err(CacheError::FetchFailed { .. })
    ));
    assert_eq!(
        f.ensure(None, Refresh::Daily).unwrap().status,
        Status::Cached
    );
    assert_eq!(read(&initial.local_path), "one\n");
    std::fs::rename(f.root.join("offline.git"), f.root.join("origin.git")).unwrap();
    f.commit("two\n", "advance main");
    f.cache()
        .set_attempt(&initial.local_path, Some(&yesterday()));
    assert_eq!(
        f.ensure(None, Refresh::Daily).unwrap().status,
        Status::Refreshed
    );
    assert_eq!(read(&initial.local_path), "two\n");
}

// repository-cache.test.ts: "refreshes existing untracked checkouts and keeps branch freshness independent"
#[test]
fn oc_repository_cache_refreshes_untracked_checkouts_per_branch() {
    if !has_git() {
        return;
    }
    let f = Fixture::new();
    f.branch("feature", "feature\n");
    let main = f.ensure(None, Refresh::Daily).unwrap();
    let feature = f.ensure(Some("feature"), Refresh::Daily).unwrap();
    f.cache().set_attempt(&feature.local_path, None);
    f.commit("new feature\n", "advance feature");
    assert_eq!(
        f.ensure(None, Refresh::Daily).unwrap().status,
        Status::Cached
    );
    assert_eq!(
        f.ensure(Some("feature"), Refresh::Daily).unwrap().status,
        Status::Refreshed
    );
    assert_eq!(read(&main.local_path), "one\n");
    assert_eq!(read(&feature.local_path), "new feature\n");
}

// repository-cache.test.ts: "replaces a stale cache directory before cloning"
#[test]
fn oc_repository_cache_replaces_a_stale_directory_before_cloning() {
    if !has_git() {
        return;
    }
    let f = Fixture::new();
    let local = f.cache().path(&f.reference, None);
    std::fs::create_dir_all(&local).unwrap();
    std::fs::write(local.join("stale.txt"), "stale").unwrap();
    let r = f.ensure(None, Refresh::No).unwrap();
    assert_eq!(r.status, Status::Cloned);
    assert!(!local.join("stale.txt").exists());
    assert_eq!(read(&local), "one\n");
}

// repository-cache.test.ts: "serializes concurrent materialization for the same checkout"
#[test]
fn oc_repository_cache_serializes_concurrent_clones() {
    if !has_git() {
        return;
    }
    let f = Fixture::new();
    let (a, b) = std::thread::scope(|s| {
        let a = s.spawn(|| f.ensure(None, Refresh::No).unwrap());
        let b = s.spawn(|| f.ensure(None, Refresh::No).unwrap());
        (a.join().unwrap(), b.join().unwrap())
    });
    let mut statuses = [a.status, b.status];
    statuses.sort();
    assert_eq!(statuses, [Status::Cached, Status::Cloned]);
    assert_eq!(a.local_path, b.local_path);
}

// repository-cache.test.ts: "replaces an existing checkout whose origin does not match"
#[test]
fn oc_repository_cache_replaces_a_checkout_with_another_origin() {
    if !has_git() {
        return;
    }
    let f = Fixture::new();
    let initial = f.ensure(None, Refresh::Daily).unwrap();
    git(
        &initial.local_path,
        &[
            "config",
            "remote.origin.url",
            "https://github.com/other/repo.git",
        ],
    );
    std::fs::write(initial.local_path.join("stale.txt"), "stale").unwrap();
    let replaced = f.ensure(None, Refresh::Daily).unwrap();
    assert_eq!(replaced.status, Status::Cloned);
    assert!(!replaced.local_path.join("stale.txt").exists());
}

// repository-cache.test.ts: "keeps branch checkouts isolated from branchless refreshes"
#[test]
fn oc_repository_cache_keeps_branch_checkouts_apart() {
    if !has_git() {
        return;
    }
    let f = Fixture::new();
    f.branch("feature", "two\n");
    let featured = f.ensure(Some("feature"), Refresh::No).unwrap();
    assert_eq!(featured.branch.as_deref(), Some("feature"));
    assert!(featured
        .local_path
        .to_string_lossy()
        .ends_with("repo@feature"));
    assert_eq!(read(&featured.local_path), "two\n");
    let refreshed = f.ensure(None, Refresh::Force).unwrap();
    assert_ne!(refreshed.local_path, featured.local_path);
    assert_eq!(read(&refreshed.local_path), "one\n");
    let cached = f.ensure(Some("feature"), Refresh::No).unwrap();
    assert_eq!(cached.status, Status::Cached);
    assert_eq!(read(&cached.local_path), "two\n");
}

// repository-cache.test.ts: "does not mistake an enclosing repository for the cache checkout"
#[test]
fn oc_repository_cache_does_not_take_an_enclosing_repository_for_the_checkout() {
    if !has_git() {
        return;
    }
    let f = Fixture::new();
    git(
        &f.root,
        &["clone", &f.remote, f.root.join("repos").to_str().unwrap()],
    );
    let r = f.ensure(None, Refresh::No).unwrap();
    assert_eq!(r.status, Status::Cloned);
    assert_eq!(read(&r.local_path), "one\n");
}

// repository-cache.test.ts: "returns typed branch validation and clone failures"
#[test]
fn oc_repository_cache_returns_typed_branch_and_clone_failures() {
    if !has_git() {
        return;
    }
    let f = Fixture::new();
    assert!(matches!(
        f.ensure(Some("../unsafe"), Refresh::No),
        Err(CacheError::InvalidBranch { .. })
    ));
    let missing = repo::Reference {
        remote: reqwest::Url::from_file_path(f.root.join("missing.git"))
            .unwrap()
            .to_string(),
        ..f.reference.clone()
    };
    assert!(matches!(
        f.cache().ensure(&missing, None, Refresh::No),
        Err(CacheError::CloneFailed { .. })
    ));
}
