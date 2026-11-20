//! Ports of OpenCode's downloaded skill tests
//! (`packages/core/test/skill-discovery.test.ts` and
//! `config/skill.test.ts`, MIT) against `skill_remote::pull` and skill
//! discovery, with a local catalog server. Each test names the OpenCode
//! test it ports; see `crates/codesmith-core/PARITY.md`.

use codesmith_core::skill;
use codesmith_core::skill_remote::pull;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Default)]
struct State {
    skills: Value,
    files: BTreeMap<String, String>,
    requests: Vec<String>,
}

/// A catalog at `<base>` (`http://…/catalog/`).
struct Catalog {
    state: Arc<Mutex<State>>,
    base: String,
    cache: tempfile::TempDir,
}

async fn catalog() -> Catalog {
    let state: Arc<Mutex<State>> = Arc::default();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let st = state.clone();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            let st = st.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                while !String::from_utf8_lossy(&buf).contains("\r\n\r\n") {
                    let n = s.read(&mut tmp).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                let req = String::from_utf8_lossy(&buf).to_string();
                let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                let body = {
                    let mut st = st.lock().unwrap();
                    st.requests.push(format!("http://{addr}{path}"));
                    if path == "/catalog/index.json" {
                        Some(json!({"skills": st.skills}).to_string())
                    } else {
                        st.files.get(&path).cloned()
                    }
                };
                let (status, body) = match body {
                    Some(b) => ("200 OK", b),
                    None => ("404 Not Found", "Not Found".to_string()),
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    Catalog {
        state,
        base: format!("http://{addr}/catalog/"),
        cache: tempfile::tempdir().unwrap(),
    }
}

impl Catalog {
    async fn pull(&self, skills: Value, files: &[(&str, &str)]) -> (Vec<PathBuf>, Vec<String>) {
        {
            let mut st = self.state.lock().unwrap();
            st.skills = skills;
            st.files = files
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            st.requests.clear();
        }
        let dirs = pull(&self.base, self.cache.path()).await;
        let requests = self.state.lock().unwrap().requests.clone();
        (dirs, requests)
    }
    fn index(&self) -> String {
        format!("{}index.json", self.base)
    }
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap()
}

/// "rejects skill name traversal without fetching files", "rejects file
/// traversal without fetching files", "rejects absolute file paths without
/// fetching files", "rejects cross-origin file URLs without fetching
/// files" (4 cases)
#[tokio::test(flavor = "multi_thread")]
async fn oc_skill_discovery_rejects_unsafe_entries_without_fetching_files() {
    let c = catalog().await;
    for skills in [
        json!([{"name": "../outside", "files": ["SKILL.md"]}]),
        json!([{"name": "deploy", "files": ["SKILL.md", "../outside.md"]}]),
        json!([{"name": "deploy", "files": ["SKILL.md", "/tmp/outside.md"]}]),
        json!([{"name": "deploy", "files": ["SKILL.md", "https://evil.example.test/outside.md"]}]),
    ] {
        let (dirs, requests) = c.pull(skills.clone(), &[]).await;
        assert!(dirs.is_empty(), "{skills}");
        assert_eq!(requests, [c.index()], "{skills}");
        assert_eq!(
            std::fs::read_dir(c.cache.path()).unwrap().count(),
            0,
            "{skills}"
        );
    }
}

/// "downloads safe nested files under the skill root"
#[tokio::test(flavor = "multi_thread")]
async fn oc_skill_discovery_downloads_safe_nested_files_under_the_skill_root() {
    let c = catalog().await;
    let (dirs, mut requests) = c
        .pull(
            json!([{"name": "deploy", "files": ["SKILL.md", "references/guide.md"]}]),
            &[
                ("/catalog/deploy/SKILL.md", "# Deploy"),
                ("/catalog/deploy/references/guide.md", "# Guide"),
            ],
        )
        .await;
    assert_eq!(dirs.len(), 1);
    requests.sort();
    let mut want = vec![
        c.index(),
        format!("{}deploy/SKILL.md", c.base),
        format!("{}deploy/references/guide.md", c.base),
    ];
    want.sort();
    assert_eq!(requests, want);
    assert_eq!(read(&dirs[0].join("SKILL.md")), "# Deploy");
    assert_eq!(read(&dirs[0].join("references/guide.md")), "# Guide");
}

/// "refreshes cached files when the version changes"
#[tokio::test(flavor = "multi_thread")]
async fn oc_skill_discovery_refreshes_cached_files_when_the_version_changes() {
    let c = catalog().await;
    let (first, _) = c
        .pull(
            json!([{"name": "deploy", "version": "1", "files": ["SKILL.md"]}]),
            &[("/catalog/deploy/SKILL.md", "# Old")],
        )
        .await;
    let (_, second) = c
        .pull(
            json!([{"name": "deploy", "version": "2", "files": ["SKILL.md"]}]),
            &[("/catalog/deploy/SKILL.md", "# New")],
        )
        .await;
    assert_eq!(read(&first[0].join("SKILL.md")), "# New");
    assert!(second.contains(&format!("{}deploy/SKILL.md", c.base)));
    let (_, third) = c
        .pull(
            json!([{"name": "deploy", "version": "2", "files": ["SKILL.md"]}]),
            &[("/catalog/deploy/SKILL.md", "# Ignored")],
        )
        .await;
    assert_eq!(third, [c.index()], "an unchanged version is not fetched");
}

/// "publishes complete updates and removes stale files"
#[tokio::test(flavor = "multi_thread")]
async fn oc_skill_discovery_publishes_complete_updates_and_removes_stale_files() {
    let c = catalog().await;
    let (first, _) = c
        .pull(
            json!([{"name": "deploy", "version": "1", "files": ["SKILL.md", "old.md"]}]),
            &[
                ("/catalog/deploy/SKILL.md", "# Old"),
                ("/catalog/deploy/old.md", "old reference"),
            ],
        )
        .await;
    let root = first[0].clone();
    c.pull(
        json!([{"name": "deploy", "version": "2", "files": ["SKILL.md", "missing.md"]}]),
        &[("/catalog/deploy/SKILL.md", "# Partial")],
    )
    .await;
    assert_eq!(
        read(&root.join("SKILL.md")),
        "# Old",
        "a partial update keeps the old copy"
    );
    assert_eq!(read(&root.join("old.md")), "old reference");
    c.pull(
        json!([{"name": "deploy", "version": "3", "files": ["SKILL.md"]}]),
        &[("/catalog/deploy/SKILL.md", "# New")],
    )
    .await;
    assert_eq!(read(&root.join("SKILL.md")), "# New");
    assert!(!root.join("old.md").exists());
    let leftovers: Vec<String> = std::fs::read_dir(root.parent().unwrap())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(leftovers, ["deploy"], "no staging or backup folders left");
}

fn write_skill(dir: &Path, id: &str, description: &str) {
    std::fs::create_dir_all(dir.join(id)).unwrap();
    std::fs::write(
        dir.join(id).join("SKILL.md"),
        format!("---\nname: {id}\ndescription: {description}\n---\nBody"),
    )
    .unwrap();
}

/// "loads directory and individual downloaded skill roots with later-source
/// precedence": a folder of skills and then downloaded skill folders, a
/// later one replacing an earlier skill of the same id.
#[tokio::test(flavor = "multi_thread")]
async fn oc_skill_loads_directory_and_downloaded_skill_roots_with_later_source_precedence() {
    let c = catalog().await;
    let first = tempfile::tempdir().unwrap();
    write_skill(first.path(), "review", "First");
    let skill_md = |d: &str| format!("---\nname: x\ndescription: {d}\n---\nBody");
    let (deploy_md, review_md) = (skill_md("Deploy"), skill_md("Second"));
    let (downloaded, _) = c
        .pull(
            json!([
                {"name": "deploy", "files": ["SKILL.md"]},
                {"name": "review", "files": ["SKILL.md"]}
            ]),
            &[
                ("/catalog/deploy/SKILL.md", deploy_md.as_str()),
                ("/catalog/review/SKILL.md", review_md.as_str()),
            ],
        )
        .await;
    let mut roots = vec![first.path().to_path_buf()];
    roots.extend(downloaded);
    let skills = skill::discover(&roots);
    let ids: Vec<&str> = skills.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, ["deploy", "review"]);
    assert_eq!(skills[0].description, "Deploy");
    assert_eq!(skills[1].description, "Second");
}

/// "keeps directory skills when a URL source fails": a catalog that cannot
/// be reached adds nothing, and the folder's skills still load.
#[test]
fn oc_skill_keeps_directory_skills_when_a_url_source_fails() {
    let d = tempfile::tempdir().unwrap();
    write_skill(d.path(), "review", "Available");
    let roots = skill::roots(
        d.path(),
        d.path(),
        &d.path().join("config"),
        &d.path().join("home"),
        &[
            d.path().display().to_string(),
            "http://127.0.0.1:9/skills/".to_string(),
        ],
    );
    let skills = skill::discover(&roots);
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].description, "Available");
}
