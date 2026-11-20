//! The media blob store, ported from OpenCode's `session-projector.test.ts`
//! and `session-prompt.test.ts` media_blob cases (MIT). OpenCode keeps
//! refs in its message, inbox and part rows; CodeSmith keeps them in its
//! append-only records, so the ports drive `Store` (and a `Session` for the
//! config switch) and look at the raw rows.

use codesmith_ai::{Media, Message, Part, Role, Source};
use codesmith_core::blob;
use codesmith_core::store::{self, Info, Record, Store};
use serde_json::json;

const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

fn png_bytes() -> Vec<u8> {
    codesmith_core::media::unbase64(PNG).unwrap()
}

fn info(id: &str) -> Info {
    Info {
        id: id.into(),
        directory: "/w".into(),
        title: None,
        agent: "build".into(),
        model: "p/m".into(),
        parent_id: None,
        created: 1,
        updated: 1,
        usage: Default::default(),
    }
}

fn user_with(text: &str, media: Vec<Media>) -> Record {
    let mut parts = vec![Part::text(text)];
    parts.extend(media.into_iter().map(Part::Media));
    Record::Message {
        message: Message {
            role: Role::User,
            parts,
        },
    }
}

fn tool_with(id: &str, media: Vec<Media>) -> Record {
    Record::Message {
        message: Message {
            role: Role::User,
            parts: vec![Part::ToolResult {
                id: id.into(),
                name: "read".into(),
                content: "Image read successfully".into(),
                is_error: false,
                media,
            }],
        },
    }
}

fn shot(name: &str) -> Media {
    Media::base64("image/png", PNG).named(name)
}

fn bodies(s: &Store, id: &str) -> Vec<String> {
    s.with_conn(|c| {
        let mut st = c.prepare("SELECT body FROM record WHERE session_id = ?1 ORDER BY seq")?;
        let rows = st.query_map([id], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    })
    .unwrap()
}

fn count(s: &Store, table: &str) -> i64 {
    s.with_conn(|c| Ok(c.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?))
        .unwrap()
}

fn refs(s: &Store) -> Vec<(String, i64)> {
    s.with_conn(|c| {
        let mut st = c.prepare("SELECT owner, seq FROM blob_ref ORDER BY owner, seq")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    })
    .unwrap()
}

fn media_of(s: &Store, id: &str) -> Vec<Media> {
    let mut out = Vec::new();
    for m in s.history(id).unwrap() {
        for p in m.parts {
            match p {
                Part::Media(m) => out.push(m),
                Part::ToolResult { media, .. } => out.extend(media),
                _ => {}
            }
        }
    }
    out
}

fn put_raw_blob(s: &Store, bytes: &[u8], mime: &str) -> String {
    let h = blob::hash(bytes);
    s.with_conn(|c| {
        c.execute(
            "INSERT INTO blob (hash, mime, bytes, data, created) VALUES (?1, ?2, ?3, ?4, 0)",
            rusqlite::params![h, mime, bytes.len() as i64, bytes],
        )?;
        Ok(())
    })
    .unwrap();
    h
}

fn put_raw_record(s: &Store, id: &str, seq: i64, body: &serde_json::Value) {
    s.with_conn(|c| {
        c.execute(
            "INSERT INTO record (session_id, seq, created, body) VALUES (?1, ?2, 1, ?3)",
            rusqlite::params![id, seq, body.to_string()],
        )?;
        Ok(())
    })
    .unwrap();
}

#[test]
fn oc_blob_hydrates_media_refs_on_reads() {
    let s = Store::memory().unwrap();
    s.create(&info("s")).unwrap();
    let raw = b"externalized-media-payload";
    let h = put_raw_blob(&s, raw, "application/pdf");
    // Only media:<hash> is a ref; a path that looks like a media URL is left alone.
    let path = format!("/api/media/{h}");
    put_raw_record(
        &s,
        "s",
        1,
        &serde_json::to_value(user_with(
            "user attachment",
            vec![
                Media::url("application/pdf", blob::reference(&h)).named("spec.pdf"),
                Media::url("application/pdf", path.clone()),
            ],
        ))
        .unwrap(),
    );
    let media = media_of(&s, "s");
    assert_eq!(
        media[0].source,
        Source::Base64(codesmith_core::media::base64(raw))
    );
    assert_eq!(media[0].filename.as_deref(), Some("spec.pdf"));
    assert_eq!(media[1].source, Source::Url(path));
}

#[test]
fn oc_blob_replaces_refs_to_missing_blobs_with_an_unavailable_placeholder() {
    let s = Store::memory().unwrap();
    s.create(&info("s")).unwrap();
    let gone = blob::reference(&"b".repeat(64));
    put_raw_record(
        &s,
        "s",
        1,
        &serde_json::to_value(Record::Message {
            message: Message {
                role: Role::User,
                parts: vec![
                    Part::Media(Media::url("image/png", gone.clone()).named("shot.png")),
                    Part::ToolResult {
                        id: "c1".into(),
                        name: "read".into(),
                        content: "Image read successfully".into(),
                        is_error: false,
                        media: vec![Media::url("image/png", gone.clone())],
                    },
                ],
            },
        })
        .unwrap(),
    );
    let history = s.history("s").unwrap();
    assert_eq!(
        history[0].parts[0],
        Part::text("[media unavailable: shot.png (image/png)]")
    );
    match &history[0].parts[1] {
        Part::ToolResult { content, media, .. } => {
            assert!(media.is_empty());
            assert_eq!(
                content,
                "Image read successfully\n[media unavailable: image/png]"
            );
        }
        p => panic!("{p:?}"),
    }
}

#[test]
fn oc_blob_keeps_media_inline_when_media_blob_is_off() {
    let s = Store::memory().unwrap();
    s.set_media_blob(false);
    s.create(&info("s")).unwrap();
    s.append("s", &user_with("inline", vec![shot("shot.png")]))
        .unwrap();
    assert!(bodies(&s, "s")[0].contains(PNG));
    assert_eq!(count(&s, "blob"), 0);
    assert_eq!(count(&s, "blob_ref"), 0);
}

#[test]
fn oc_blob_stores_media_once_and_refs_it_from_messages_and_tool_results() {
    let s = Store::memory().unwrap();
    s.create(&info("s")).unwrap();
    s.append("s", &user_with("first", vec![shot("a.png")]))
        .unwrap();
    s.append("s", &user_with("again", vec![shot("b.png")]))
        .unwrap();
    s.append("s", &tool_with("c1", vec![Media::base64("image/png", PNG)]))
        .unwrap();
    let h = blob::hash(&png_bytes());
    for body in bodies(&s, "s") {
        assert!(!body.contains(PNG), "{body}");
        assert!(body.contains(&blob::reference(&h)), "{body}");
    }
    assert_eq!(count(&s, "blob"), 1);
    assert_eq!(
        refs(&s),
        [("s".into(), 1), ("s".into(), 2), ("s".into(), 3)]
    );
    // Reads see the media inline again.
    let media = media_of(&s, "s");
    assert_eq!(media.len(), 3);
    assert!(media.iter().all(|m| m.source == Source::Base64(PNG.into())));
}

#[test]
fn oc_blob_media_blob_is_on_by_default() {
    // OpenCode turns it on by default only on its dev channel; CodeSmith
    // has one channel and stores media as blobs unless config says no.
    let s = Store::memory().unwrap();
    assert!(s.media_blob());
    s.create(&info("s")).unwrap();
    s.append("s", &user_with("x", vec![shot("a.png")])).unwrap();
    assert_eq!(count(&s, "blob"), 1);
}

#[test]
fn oc_blob_deletes_undone_turns_refs_and_the_blobs_only_they_referenced() {
    let s = Store::memory().unwrap();
    s.create(&info("s")).unwrap();
    s.append("s", &user_with("kept", vec![shot("shot.png")]))
        .unwrap();
    let from = s
        .append("s", &Record::Snapshot { tree: "t".into() })
        .unwrap();
    let notes = Media::base64("text/plain", codesmith_core::media::base64(b"reverted"));
    s.append("s", &user_with("reverted", vec![notes])).unwrap();
    s.append("s", &user_with("shared", vec![shot("again.png")]))
        .unwrap();
    assert_eq!(count(&s, "blob_ref"), 3);
    assert_eq!(count(&s, "blob"), 2);
    let revert = s
        .append(
            "s",
            &Record::Revert {
                from_seq: from,
                after: "t2".into(),
            },
        )
        .unwrap();
    // Redo is still possible: nothing is let go.
    assert_eq!(count(&s, "blob_ref"), 3);
    assert!(store::redo_target(&s.entries("s").unwrap()).is_some());
    s.append("s", &user_with("next", vec![])).unwrap();
    assert!(revert > from);
    assert_eq!(refs(&s), [("s".into(), 1)]);
    let blobs: Vec<String> = s
        .with_conn(|c| {
            let mut st = c.prepare("SELECT hash FROM blob")?;
            let rows = st.query_map([], |r| r.get(0))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .unwrap();
    assert_eq!(blobs, [blob::hash(&png_bytes())]);
}

#[test]
fn oc_blob_keeps_a_blob_while_a_fork_references_it_and_deletes_it_with_the_last_session() {
    let mut s = Store::memory().unwrap();
    s.create(&info("parent")).unwrap();
    s.append("parent", &user_with("fork me", vec![shot("shot.png")]))
        .unwrap();
    s.fork("parent", "child").unwrap();
    let owners: Vec<String> = refs(&s).into_iter().map(|r| r.0).collect();
    assert_eq!(owners, ["child", "parent"]);
    s.delete("parent").unwrap();
    assert_eq!(refs(&s), [("child".into(), 1)]);
    assert_eq!(count(&s, "blob"), 1);
    assert_eq!(media_of(&s, "child")[0].source, Source::Base64(PNG.into()));
    s.delete("child").unwrap();
    assert_eq!(count(&s, "blob_ref"), 0);
    assert_eq!(count(&s, "blob"), 0);
}

#[test]
fn oc_blob_keeps_a_tool_results_blob_while_a_fork_references_it() {
    let mut s = Store::memory().unwrap();
    s.create(&info("parent")).unwrap();
    s.append(
        "parent",
        &tool_with("shot", vec![Media::base64("image/png", PNG)]),
    )
    .unwrap();
    s.fork("parent", "child").unwrap();
    s.delete("parent").unwrap();
    assert_eq!(refs(&s), [("child".into(), 1)]);
    assert_eq!(count(&s, "blob"), 1);
    s.delete("child").unwrap();
    assert_eq!(count(&s, "blob"), 0);
}

#[test]
fn oc_blob_splits_inline_media_out_of_stored_records_for_offline_backfills() {
    let s = Store::memory().unwrap();
    s.set_media_blob(false);
    s.create(&info("s")).unwrap();
    s.append(
        "s",
        &tool_with("shot", vec![Media::base64("image/png", PNG)]),
    )
    .unwrap();
    s.append("s", &user_with("no media", vec![])).unwrap();
    // The backfill runs whatever the setting says.
    assert_eq!(s.migrate_media("s").unwrap(), 1);
    assert_eq!(s.migrate_media("s").unwrap(), 0);
    let body = &bodies(&s, "s")[0];
    assert!(!body.contains(PNG));
    assert!(body.contains(&blob::reference(&blob::hash(&png_bytes()))));
    assert_eq!(refs(&s), [("s".into(), 1)]);
    assert_eq!(media_of(&s, "s")[0].source, Source::Base64(PNG.into()));
}

#[test]
fn oc_blob_moves_a_legacy_inline_media_message_into_a_blob() {
    // A record from before the blob store: inline base64 media, no
    // `filename` or `meta` fields. It reads unchanged, and the backfill
    // keeps its content while the media moves out.
    let s = Store::memory().unwrap();
    s.create(&info("s")).unwrap();
    put_raw_record(
        &s,
        "s",
        1,
        &json!({"kind": "message", "message": {"role": "user", "parts": [
            {"type": "text", "data": {"text": "legacy"}},
            {"type": "media", "data": {"mime": "image/png", "source": {"base64": PNG}}},
        ]}}),
    );
    let before = s.history("s").unwrap();
    assert_eq!(s.migrate_media("s").unwrap(), 1);
    assert_eq!(s.history("s").unwrap(), before);
    assert_eq!(count(&s, "blob"), 1);
}

#[test]
fn oc_blob_stores_a_forks_copies_of_inline_parent_media_as_blobs_and_leaves_the_parent_inline() {
    let mut s = Store::memory().unwrap();
    s.set_media_blob(false);
    s.create(&info("parent")).unwrap();
    s.append("parent", &user_with("inline", vec![shot("shot.png")]))
        .unwrap();
    s.set_media_blob(true);
    s.fork("parent", "child").unwrap();
    assert!(bodies(&s, "parent")[0].contains(PNG));
    assert!(!bodies(&s, "child")[0].contains(PNG));
    assert_eq!(refs(&s), [("child".into(), 1)]);
}

#[test]
fn oc_blob_records_refs_on_import_only_for_the_blobs_it_took_out_of_its_own_inline_media() {
    let mut s = Store::memory().unwrap();
    s.create(&info("other")).unwrap();
    let secret = b"another session's blob";
    s.append(
        "other",
        &user_with(
            "mine",
            vec![Media::base64(
                "text/plain",
                codesmith_core::media::base64(secret),
            )],
        ),
    )
    .unwrap();
    let foreign = blob::reference(&blob::hash(secret));
    let export = store::Export {
        info: info("imported"),
        entries: vec![
            store::Entry {
                seq: 1,
                created: 1,
                record: user_with(
                    "carried",
                    vec![Media::url("text/plain", foreign.clone()).named("x.txt")],
                ),
            },
            store::Entry {
                seq: 2,
                created: 1,
                record: user_with("own", vec![shot("own.png")]),
            },
        ],
    };
    s.import(&export).unwrap();
    let imported: Vec<(String, i64)> = refs(&s).into_iter().filter(|r| r.0 == "imported").collect();
    assert_eq!(imported, [("imported".into(), 2)]);
    // The ref it carried in never exposes the other session's blob.
    let history = s.history("imported").unwrap();
    assert_eq!(
        history[0].parts[1],
        Part::text("[media unavailable: x.txt (text/plain)]")
    );
    assert!(!bodies(&s, "imported")[0].contains(&foreign));
}

#[test]
fn oc_blob_imports_media_into_blobs_only_where_enabled() {
    let mut s = Store::memory().unwrap();
    s.set_media_blob(false);
    let export = store::Export {
        info: info("imported"),
        entries: vec![store::Entry {
            seq: 1,
            created: 1,
            record: user_with("own", vec![shot("own.png")]),
        }],
    };
    s.import(&export).unwrap();
    assert!(bodies(&s, "imported")[0].contains(PNG));
    assert_eq!(count(&s, "blob"), 0);
    s.set_media_blob(true);
    let mut e2 = export.clone();
    e2.info.id = "imported2".into();
    s.import(&e2).unwrap();
    assert!(!bodies(&s, "imported2")[0].contains(PNG));
    assert_eq!(count(&s, "blob"), 1);
}

#[test]
fn oc_blob_caps_one_item_at_twenty_mebibytes() {
    let s = Store::memory().unwrap();
    s.create(&info("s")).unwrap();
    assert_eq!(blob::MAX_BYTES, 20 * 1024 * 1024);
    let big = vec![7u8; blob::MAX_BYTES + 1];
    assert!(s.put_blob("job", &big, "video/mp4").unwrap().is_none());
    let h = s.put_blob("job", b"small", "video/mp4").unwrap().unwrap();
    assert_eq!(s.blob(&h).unwrap().unwrap().1, b"small");
    s.release_blobs("job").unwrap();
    assert!(s.blob(&h).unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_blob_stores_media_refs_at_admission_when_config_enables_media_blob() {
    use codesmith_core::config::{Config, Loaded};
    use codesmith_core::permission::{Fixed, Reply};
    use codesmith_core::session::{Options, Session};
    use std::sync::Arc;
    struct Quiet;
    impl codesmith_core::session::Sink for Quiet {
        fn text(&mut self, _: &str) {}
    }
    let base = answer_server().await;
    for (setting, inline) in [(json!(null), false), (json!(false), true)] {
        let dir = tempfile::tempdir().unwrap();
        let config: Config = serde_json::from_value(json!({
            "model": "fake/m", "snapshots": false, "media": {"blob": setting},
            "providers": {"fake": {"protocol": "openai-chat",
                "settings": {"baseURL": base, "apiKey": "x"}, "models": {"m": {}}}},
        }))
        .unwrap();
        let loaded = Loaded {
            config,
            files: vec![],
            root: dir.path().to_path_buf(),
        };
        let mut s = Session::new(
            &loaded,
            dir.path().to_path_buf(),
            Options {
                agent: None,
                model: None,
                resume: None,
                title: Some("t".into()),
                parent: None,
                questioner: None,
                mcp: None,
            },
            Arc::new(Fixed(Reply::Once)),
            Some(Store::open(&dir.path().join("s.db")).unwrap()),
        )
        .unwrap();
        s.prompt_with("look", vec![shot("shot.png")], &mut Quiet)
            .await
            .unwrap();
        let db = Store::open(&dir.path().join("s.db")).unwrap();
        let all = bodies(&db, &s.id).join("\n");
        assert_eq!(all.contains(PNG), inline, "media.blob = {setting}");
    }
}

/// A Chat Completions model that answers "ok" to anything.
async fn answer_server() -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 65536];
                loop {
                    let n = s.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(i) = text.find("\r\n\r\n") {
                        let len: usize = text[..i]
                            .lines()
                            .find_map(|l| {
                                l.to_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse().unwrap())
                            })
                            .unwrap_or(0);
                        if buf.len() >= i + 4 + len {
                            break;
                        }
                    }
                }
                let chunk =
                    json!({"choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}]});
                let sse = format!("data: {chunk}\n\ndata: [DONE]\n\n");
                let _ = s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}", sse.len()).as_bytes()).await;
            });
        }
    });
    format!("http://{addr}/v1")
}
