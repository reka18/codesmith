//! Ports of the tool cases in PARITY.md's porting backlog
//! (`packages/core/test/tool-websearch.test.ts`, `file-mutation.test.ts`,
//! `tool-write.test.ts`, `formatter.test.ts`, `filesystem/search.test.ts`,
//! `mime.test.ts`, MIT), on the tools called directly.

use codesmith_core::config::{self, ToolOutputConfig};
use codesmith_core::format::{Formatter, Setting};
use codesmith_core::media;
use codesmith_core::permission::{Asker, Gate, Reply, Request};
use codesmith_core::tool::{self, websearch::WebSearchConfig, Ctx};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Recorder {
    reply: Reply,
    asked: Mutex<Vec<Request>>,
}

impl Asker for Recorder {
    fn ask(&self, req: &Request) -> Reply {
        self.asked.lock().unwrap().push(req.clone());
        self.reply.clone()
    }
}

struct Env {
    dir: tempfile::TempDir,
    gate: Gate,
    rec: Arc<Recorder>,
    websearch: Option<WebSearchConfig>,
    format: Option<Arc<Formatter>>,
}

impl Env {
    fn new(reply: Reply) -> Env {
        let rec = Arc::new(Recorder {
            reply,
            asked: Mutex::default(),
        });
        Env {
            dir: tempfile::tempdir().unwrap(),
            gate: Gate::new(vec![], rec.clone()),
            rec,
            websearch: None,
            format: None,
        }
    }
    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }
    fn ctx(&self) -> Ctx<'_> {
        Ctx {
            cwd: self.root(),
            root: self.root(),
            gate: &self.gate,
            shell: "/bin/sh".into(),
            session_id: "test".into(),
            limits: ToolOutputConfig::default(),
            client: reqwest::Client::new(),
            jobs: None,
            websearch: self.websearch.clone(),
            format: self.format.clone(),
            lsp: None,
            progress: None,
        }
    }
    async fn call(&self, name: &str, input: Value) -> (String, bool) {
        tool::execute(&self.ctx(), name, &input).await
    }
}

// --- tool-websearch.test.ts ---------------------------------------------

/// A SearXNG stand-in answering every search with `status` and `body`;
/// the queries it was sent.
async fn searxng(status: u16, body: Value) -> (String, Arc<Mutex<Vec<String>>>) {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                return;
            };
            let mut buf = vec![0u8; 8192];
            let n = s.read(&mut buf).await.unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]).to_string();
            log.lock()
                .unwrap()
                .push(head.lines().next().unwrap_or("").to_string());
            let b = body.to_string();
            let _ = s
                .write_all(
                    format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}",
                        b.len()
                    )
                    .as_bytes(),
                )
                .await;
        }
    });
    (format!("http://{addr}"), seen)
}

fn searching(reply: Reply, url: String) -> Env {
    let mut env = Env::new(reply);
    env.websearch = Some(WebSearchConfig {
        url: Some(url),
        ..Default::default()
    });
    env
}

/// "asserts permission before delegating to WebSearch": the query is
/// asked about (saving `*`) before it is sent, and a refusal sends nothing.
#[tokio::test(flavor = "multi_thread")]
async fn oc_websearch_asks_permission_before_searching() {
    let results = json!({"results": [{"url": "https://example.com", "title": "Search results", "content": "search results"}]});
    let (url, seen) = searxng(200, results.clone()).await;
    let env = searching(Reply::Reject(None), url);
    let (_, err) = env
        .call("websearch", json!({"query": "effect typescript"}))
        .await;
    assert!(err);
    assert!(seen.lock().unwrap().is_empty());
    let asked = env.rec.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    assert_eq!(
        (
            asked[0].action.as_str(),
            asked[0].resources.clone(),
            asked[0].save.clone()
        ),
        (
            "websearch",
            vec!["effect typescript".to_string()],
            vec!["*".to_string()]
        )
    );
    let (url, seen) = searxng(200, results).await;
    let env = searching(Reply::Once, url);
    let (out, err) = env
        .call("websearch", json!({"query": "effect typescript"}))
        .await;
    assert!(!err, "{out}");
    assert_eq!(
        out,
        "## [Search results](https://example.com)\n\nsearch results"
    );
    let sent = seen.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].contains("q=effect+typescript"), "{sent:?}");
}

/// "returns flat results and keeps the provider in metadata". Adapted: one
/// search service (SearXNG), so no provider to keep.
#[tokio::test(flavor = "multi_thread")]
async fn oc_websearch_returns_flat_results() {
    let (url, _) = searxng(
        200,
        json!({"results": [{"url": "https://effect.website", "title": "Effect", "content": "parallel results", "publishedDate": "2026-07-25T00:00:00.000Z"}]}),
    )
    .await;
    let env = searching(Reply::Once, url);
    let (out, err) = env
        .call("websearch", json!({"query": "effect layers"}))
        .await;
    assert!(!err, "{out}");
    assert_eq!(
        out,
        "## [Effect](https://effect.website) (published 2026-07-25T00:00:00.000Z)\n\nparallel results"
    );
}

/// "uses the concise no-results fallback"
#[tokio::test(flavor = "multi_thread")]
async fn oc_websearch_uses_the_concise_no_results_fallback() {
    let (url, _) = searxng(200, json!({"results": []})).await;
    let env = searching(Reply::Once, url);
    let (out, err) = env.call("websearch", json!({"query": "nothing"})).await;
    assert!(!err);
    assert_eq!(out, tool::websearch::NO_RESULTS);
}

/// "reports safe HTTP failures with the attempted provider". Adapted: the
/// status, without the service's reply.
#[tokio::test(flavor = "multi_thread")]
async fn oc_websearch_reports_safe_http_failures() {
    let (url, _) = searxng(500, json!({"secret": "internal detail"})).await;
    let env = searching(Reply::Once, url);
    let (out, err) = env.call("websearch", json!({"query": "effect"})).await;
    assert!(err);
    assert!(out.contains("HTTP 500"), "{out}");
    assert!(!out.contains("internal detail"), "{out}");
}

// --- file-mutation.test.ts and tool-write.test.ts -----------------------

/// "preserves exactly one BOM for text writes and normalizes created text"
/// (the created half; the overwrite is `tests/oc_tools.rs`'s
/// `oc_write_preserves_exactly_one_bom`).
#[tokio::test(flavor = "multi_thread")]
async fn oc_write_normalizes_the_bom_of_created_text() {
    let env = Env::new(Reply::Once);
    let (out, err) = env
        .call(
            "write",
            json!({"path": "created.txt", "content": "\u{feff}\u{feff}\u{feff}created"}),
        )
        .await;
    assert!(!err, "{out}");
    assert_eq!(
        std::fs::read_to_string(env.root().join("created.txt")).unwrap(),
        "\u{feff}created"
    );
}

/// A formatter command appending `text` to the file.
fn append(text: &str) -> Value {
    json!([
        "/bin/sh",
        "-c",
        format!("printf '%s' '{text}' >> \"$0\""),
        "$FILE"
    ])
}

/// "formats the committed file"
#[tokio::test(flavor = "multi_thread")]
async fn oc_write_formats_the_committed_file() {
    let mut env = Env::new(Reply::Once);
    let setting: Setting = serde_json::from_value(json!({"upper": {
        "command": ["/bin/sh", "-c", "tr a-z A-Z < \"$0\" > \"$0.tmp\" && mv \"$0.tmp\" \"$0\"", "$FILE"],
        "extensions": [".txt"],
    }}))
    .unwrap();
    env.format = Some(Arc::new(Formatter::new(
        Some(&setting),
        env.root(),
        env.root(),
    )));
    let (out, err) = env
        .call(
            "write",
            json!({"path": "formatted.txt", "content": "format me"}),
        )
        .await;
    assert!(!err, "{out}");
    assert_eq!(
        std::fs::read_to_string(env.root().join("formatted.txt")).unwrap(),
        "FORMAT ME"
    );
}

// --- formatter.test.ts --------------------------------------------------

/// The `{entry.name}` table: formatter settings layered from the global
/// config up, as the config layers merge. Adapted: the extensions are
/// `.mdtest` and `.project`, so no built-in formatter on this machine can
/// take the file first.
#[tokio::test(flavor = "multi_thread")]
async fn oc_formatter_layers_merge_as_configured() {
    let md = || json!({"command": append("md"), "extensions": [".mdtest"]});
    let project = || json!({"command": append("project"), "extensions": [".project"]});
    type Case<'a> = (&'a str, Vec<Option<Value>>, Vec<(&'a str, Option<&'a str>)>);
    let cases: Vec<Case> = vec![
        (
            "keeps lower formatters when a higher config adds another",
            vec![
                Some(json!({"markdown": md()})),
                None,
                Some(json!({"project": project()})),
            ],
            vec![
                ("test.mdtest", Some("md")),
                ("test.project", Some("project")),
            ],
        ),
        (
            "inherits omitted fields and replaces supplied arrays",
            vec![
                Some(
                    json!({"markdown": {"command": append("A"), "extensions": [".mdtest", ".project"]}}),
                ),
                Some(json!({"markdown": {"command": append("B"), "extensions": [".project"]}})),
            ],
            vec![("test.mdtest", None), ("test.project", Some("B"))],
        ),
        (
            "merges environment variables by key",
            vec![
                Some(json!({"markdown": {
                    "command": ["/bin/sh", "-c", "printf '%s%s' \"$FIRST\" \"$SECOND\" >> \"$0\"", "$FILE"],
                    "extensions": [".mdtest"],
                    "environment": {"FIRST": "global", "SECOND": "global"},
                }})),
                Some(json!({"markdown": {"environment": {"SECOND": "project"}}})),
            ],
            vec![("test.mdtest", Some("globalproject"))],
        ),
        (
            "keeps a lower disable through overrides that omit it",
            vec![
                Some(
                    json!({"markdown": {"command": append("md"), "extensions": [".mdtest"], "disabled": true}}),
                ),
                Some(json!({"markdown": {"command": append("override")}, "project": project()})),
            ],
            vec![("test.mdtest", None), ("test.project", Some("project"))],
        ),
        (
            "re-enables a lower disabled formatter",
            vec![
                Some(
                    json!({"markdown": {"command": append("md"), "extensions": [".mdtest"], "disabled": true}}),
                ),
                Some(json!({"markdown": {"disabled": false}})),
            ],
            vec![("test.mdtest", Some("md"))],
        ),
        (
            "keeps lower formatters through an empty object",
            vec![Some(json!({"markdown": md()})), Some(json!({}))],
            vec![("test.mdtest", Some("md"))],
        ),
        (
            "clears lower formatters when a higher config is false",
            vec![Some(json!({"markdown": md()})), Some(json!(false))],
            vec![("test.mdtest", None)],
        ),
        (
            "clears lower formatters before a later object when a higher config is true",
            vec![
                Some(json!({"markdown": md()})),
                Some(json!(true)),
                Some(json!({"project": project()})),
            ],
            vec![("test.mdtest", None), ("test.project", Some("project"))],
        ),
        (
            "enables only formatters configured after false",
            vec![
                Some(json!({"markdown": md()})),
                Some(json!(false)),
                Some(json!({"project": project()})),
            ],
            vec![("test.mdtest", None), ("test.project", Some("project"))],
        ),
    ];
    for (name, layers, expected) in cases {
        let mut merged = json!({});
        for layer in layers.into_iter().flatten() {
            config::merge_layer(&mut merged, json!({"formatter": layer}));
        }
        let setting: Setting = serde_json::from_value(merged["formatter"].clone()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let f = Formatter::new(Some(&setting), root.clone(), root.clone());
        for (file, want) in expected {
            let path = root.join(file);
            std::fs::write(&path, "").unwrap();
            let ran = f.file(&path).await;
            let got = ran.map(|_| std::fs::read_to_string(&path).unwrap());
            assert_eq!(got.as_deref(), want, "{name}: {file}");
        }
    }
}

// --- filesystem/search.test.ts ------------------------------------------

/// "honors wildcard directory rules from .gitignore". Adapted: the glob
/// tool, in a Git repository.
#[tokio::test(flavor = "multi_thread")]
async fn oc_glob_honors_wildcard_directory_rules_from_gitignore() {
    let env = Env::new(Reply::Once);
    let root = env.root();
    std::fs::create_dir_all(root.join("rust/target/debug/deps")).unwrap();
    std::fs::write(root.join(".gitignore"), "**/target/\n").unwrap();
    std::fs::write(root.join("rust/target/debug/deps/ignored.rs"), "ignored").unwrap();
    std::fs::write(root.join("rust/kept.rs"), "kept").unwrap();
    let git = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&root)
        .status();
    if !git.is_ok_and(|s| s.success()) {
        return;
    }
    let (out, err) = env.call("glob", json!({"pattern": "**/*.rs"})).await;
    assert!(!err, "{out}");
    assert!(out.contains("kept.rs"), "{out}");
    assert!(!out.contains("target"), "{out}");
}

// --- mime.test.ts -------------------------------------------------------

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// "leaves samples without media headers to the text fallback: %j" and
/// "keeps ordinary text starting with BM as text"
#[test]
fn oc_mime_leaves_samples_without_media_headers_to_the_text_fallback() {
    for sample in [
        &[65u8][..],
        &[0xff],
        &[0xe2, 0x82],
        b"BM",
        b"BMW service schedule\n",
    ] {
        assert_eq!(media::detect(sample), None, "{sample:?}");
    }
}

/// "tells ISO-BMFF brands apart". Adapted: AVIF, HEIC and M4A are not
/// media here; MP4 and QuickTime are video.
#[test]
fn oc_mime_tells_iso_bmff_brands_apart() {
    for (sample, want) in [
        (
            "000000206674797061766966000000004d695072617669666d6961666d696631",
            None,
        ),
        ("000000186674797061766973000000006d73663161766973", None),
        (
            "00000018667479706d69663100000000617669666d69663100000000",
            None,
        ),
        (
            "000000246674797068656963000000006d6966314d6950726d6961664d694842",
            None,
        ),
        (
            "00000018667479706d69663100000000686569636d69663100000000",
            None,
        ),
        (
            "000000206674797069736f6d0000020069736f6d69736f32617663316d703431",
            Some("video/mp4"),
        ),
        (
            "00000014667479707174202000000200717420200000000877696465",
            Some("video/quicktime"),
        ),
        (
            "0000001c667479704d344120000002004d34412069736f6d69736f3200000008",
            None,
        ),
    ] {
        assert_eq!(media::detect(&hex(sample)), want, "{sample}");
    }
    let _: &Path = Path::new("");
}
