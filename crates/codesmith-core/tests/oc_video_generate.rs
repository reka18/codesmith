//! Video generation, ported from OpenCode's `video-generation.test.ts` and
//! `tool-video-generate.test.ts` (MIT). The provider is a scripted Google
//! Interactions (or xAI) video endpoint whose job states the test sets.

mod media_harness;

use codesmith_core::config::Config;
use codesmith_core::generate;
use codesmith_core::permission::{Asker, Gate, Reply, Request};
use codesmith_core::store::Store;
use codesmith_core::tool::image_generate::Env;
use codesmith_core::tool::shell::Jobs;
use codesmith_core::tool::video_generate as tool;
use codesmith_core::tool::Ctx;
use codesmith_core::video::{self, Job, Videos};
use media_harness::*;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const GOOGLE: &str = "@opencode/ai/providers/google/videos";
const XAI: &str = "@opencode/ai/providers/xai/videos";

/// The provider's jobs: each submission gets the next remote id, whose
/// status the test sets; a finished job returns `videos` videos.
#[derive(Default)]
struct State {
    submitted: usize,
    done: HashMap<String, bool>,
    cancelled: Vec<String>,
    videos: usize,
}

fn mp4_with(i: u8) -> Vec<u8> {
    let mut v = mp4();
    v.push(i);
    v
}

/// A Google Interactions video endpoint over `state`.
async fn google(state: Arc<Mutex<State>>) -> Server {
    serve(Arc::new(move |s: &Seen| {
        let mut st = state.lock().unwrap();
        if s.method == "POST" && s.path.ends_with("/interactions") {
            st.submitted += 1;
            return json_reply(json!({"id": format!("remote_{}", st.submitted), "status": "in_progress"}));
        }
        if s.method == "POST" && s.path.ends_with("/cancel") {
            let id = s.path.trim_end_matches("/cancel").rsplit('/').next().unwrap().to_string();
            st.cancelled.push(id);
            return json_reply(json!({}));
        }
        let id = s.path.rsplit('/').next().unwrap().to_string();
        if st.done.get(&id).copied().unwrap_or(false) {
            let n = st.videos.max(1);
            let content: Vec<Value> = (0..n)
                .map(|i| json!({"type": "video", "mime_type": "video/mp4", "data": codesmith_core::media::base64(&mp4_with(i as u8))}))
                .collect();
            json_reply(json!({"id": id, "status": "completed", "steps": [{"type": "model_output", "content": content}]}))
        } else {
            json_reply(json!({"id": id, "status": "in_progress"}))
        }
    }))
    .await
}

fn setup_config(base: &str, package: &str, provider_id: &str) -> Config {
    let api_base = if package == GOOGLE {
        format!("{base}/v1beta")
    } else {
        format!("{base}/v1")
    };
    config(
        vec![provider(
            provider_id,
            "",
            vec![model(
                "director",
                "video",
                package,
                &api_base,
                &["text", "image"],
                10,
            )],
        )],
        &[provider_id],
        json!({}),
    )
}

fn input(prompt: &str) -> video::Input {
    video::Input {
        model: "test/director".into(),
        prompt: prompt.into(),
        ..Default::default()
    }
}

const FAST: Duration = Duration::from_millis(5);

#[tokio::test(flavor = "multi_thread")]
async fn oc_video_keeps_jobs_across_restarts_and_forgets_them_a_day_after_they_finish() {
    let state = Arc::new(Mutex::new(State {
        videos: 2,
        ..Default::default()
    }));
    let server = google(state.clone()).await;
    let config = setup_config(&server.url, GOOGLE, "test");
    let client = reqwest::Client::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.db");
    let (done, slow, first) = {
        let store = Store::open(&path).unwrap();
        let videos = Videos {
            store: &store,
            config: &config,
            client: &client,
        };
        let done = videos.start(&input("a cat")).await.unwrap();
        let slow = videos.start(&input("a snail")).await.unwrap();
        assert!(done.starts_with("video_"));
        state.lock().unwrap().done.insert("remote_1".into(), true);
        let job = videos.wait(&done, FAST).await.unwrap().unwrap();
        let Job::Completed { blobs } = &job else {
            panic!("{job:?}")
        };
        let sizes: Vec<(String, u64)> = blobs.iter().map(|b| (b.mime.clone(), b.bytes)).collect();
        assert_eq!(
            sizes,
            [
                ("video/mp4".to_string(), mp4().len() as u64 + 1),
                ("video/mp4".to_string(), mp4().len() as u64 + 1)
            ]
        );
        assert_eq!(store.blob(&blobs[0].id).unwrap().unwrap().1, mp4_with(0));
        assert_eq!(videos.get(&slow).unwrap(), Some(Job::Running));
        (done, slow, job)
    };
    // A new process reads the same rows and picks the polling up.
    let store = Store::open(&path).unwrap();
    let videos = Videos {
        store: &store,
        config: &config,
        client: &client,
    };
    assert_eq!(videos.get(&done).unwrap(), Some(first));
    state.lock().unwrap().done.insert("remote_2".into(), true);
    assert!(matches!(
        videos.wait(&slow, FAST).await.unwrap(),
        Some(Job::Completed { .. })
    ));
    let day_later = codesmith_core::store::now() + 25 * 60 * 60 * 1000;
    assert_eq!(videos.sweep_at(day_later).unwrap(), 2);
    assert_eq!(videos.get(&done).unwrap(), None);
    // The blobs only the jobs held went with them.
    let left: i64 = store
        .with_conn(|c| Ok(c.query_row("SELECT COUNT(*) FROM blob", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(left, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_video_waits_for_a_job_to_finish_and_cancels_a_running_one() {
    let state = Arc::new(Mutex::new(State {
        videos: 2,
        ..Default::default()
    }));
    let server = google(state.clone()).await;
    let config = setup_config(&server.url, GOOGLE, "test");
    let client = reqwest::Client::new();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("s.db")).unwrap();
    let videos = Videos {
        store: &store,
        config: &config,
        client: &client,
    };
    let done = videos.start(&input("a cat")).await.unwrap();
    let slow = videos.start(&input("a snail")).await.unwrap();
    let flip = {
        let state = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            state.lock().unwrap().done.insert("remote_1".into(), true);
        })
    };
    match videos.wait(&done, FAST).await.unwrap() {
        Some(Job::Completed { blobs }) => assert_eq!(blobs.len(), 2),
        other => panic!("{other:?}"),
    }
    flip.await.unwrap();
    assert!(videos.cancel(&slow).await.unwrap());
    assert_eq!(
        videos.wait(&slow, FAST).await.unwrap(),
        Some(Job::Error {
            reason: "cancelled".into(),
            error: "The video generation was cancelled".into()
        })
    );
    assert_eq!(state.lock().unwrap().cancelled, ["remote_2"]);
    assert_eq!(videos.wait("video_missing", FAST).await.unwrap(), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_video_xai_submits_a_job_and_downloads_its_video() {
    let files = serve(Arc::new(|_: &Seen| (200, "video/mp4", mp4()))).await;
    let file_url = format!("{}/video.mp4", files.url);
    let polls = Arc::new(Mutex::new(0));
    let p = polls.clone();
    let server = serve(Arc::new(move |s: &Seen| {
        if s.method == "POST" {
            return json_reply(json!({"request_id": "req_1"}));
        }
        let mut n = p.lock().unwrap();
        *n += 1;
        if *n < 2 {
            json_reply(json!({"status": "pending", "progress": 10}))
        } else {
            json_reply(json!({"status": "done", "video": {"url": file_url, "duration": 6}}))
        }
    }))
    .await;
    let config = setup_config(&server.url, XAI, "test");
    let client = reqwest::Client::new();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("s.db")).unwrap();
    let videos = Videos {
        store: &store,
        config: &config,
        client: &client,
    };
    let id = videos
        .start(&video::Input {
            duration: Some(6),
            aspect: Some("16:9".into()),
            sound: Some(true),
            ..input("a kite")
        })
        .await
        .unwrap();
    let Some(Job::Completed { blobs }) = videos.wait(&id, FAST).await.unwrap() else {
        panic!()
    };
    assert_eq!(store.blob(&blobs[0].id).unwrap().unwrap().1, mp4());
    let start = &server.requests()[0];
    assert_eq!(start.path, "/v1/videos/generations");
    assert_eq!(start.header("authorization"), Some("Bearer test-key"));
    assert_eq!(
        start.json(),
        json!({"model": "director", "prompt": "a kite", "duration": 6, "aspect_ratio": "16:9", "generate_audio": true})
    );
    assert!(server.requests()[1..]
        .iter()
        .all(|r| r.path == "/v1/videos/req_1"));
    // The download is a public URL: no provider key.
    assert_eq!(files.requests()[0].header("authorization"), None);
}

#[derive(Default)]
struct Recorder {
    asked: Mutex<Vec<Request>>,
}

impl Asker for Recorder {
    fn ask(&self, req: &Request) -> Reply {
        self.asked.lock().unwrap().push(req.clone());
        Reply::Once
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_video_tool_starts_the_job_saves_every_video_it_returns_and_lists_them_all_in_the_notice(
) {
    let state = Arc::new(Mutex::new(State {
        videos: 2,
        ..Default::default()
    }));
    state.lock().unwrap().done.insert("remote_1".into(), true);
    let server = google(state.clone()).await;
    let config = setup_config(&server.url, GOOGLE, "test");
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("first.png"), png_bytes()).unwrap();
    let rec = Arc::new(Recorder::default());
    let gate = Gate::new(vec![], rec.clone());
    let jobs = Jobs::default();
    let ctx = Ctx {
        cwd: root.clone(),
        root: root.clone(),
        gate: &gate,
        shell: "/bin/sh".into(),
        session_id: "ses_test".into(),
        limits: Default::default(),
        client: reqwest::Client::new(),
        jobs: Some(jobs.clone()),
        websearch: None,
        format: None,
        lsp: None,
        progress: None,
    };
    let env = Env {
        config: &config,
        provider: Some("test"),
        store: Some(root.join("s.db")),
        gate: gate.clone(),
        jobs: jobs.clone(),
        retry: generate::Options {
            retry_scale: 0.001,
            poll: FAST,
        },
    };
    let result = tool::run(
        &ctx,
        env,
        &json!({
            "prompt": "the kite takes off",
            "start": "first.png",
            "end": "https://example.test/last.png",
            "references": ["https://example.test/voice.mp3?sig=1", "https://example.test/motion.mp4"],
            "duration": 6,
            "aspect": "16:9",
            "resolution": "720p",
            "sound": true,
            "path": "kite.mp4",
        }),
    )
    .await
    .unwrap();
    let file = root.join("kite.mp4");
    let second = root.join("kite-1.mp4");
    let id = result.split_whitespace().nth(2).unwrap().to_string();
    assert!(id.starts_with("video_"), "{result}");
    assert!(result.contains(&format!(
        "started in the background with test/director. It will be saved to {};",
        file.display()
    )));
    let notice = loop {
        if let Some(j) = jobs.take_finished().into_iter().next() {
            break j.notice.unwrap();
        }
        tokio::time::timeout(Duration::from_secs(10), jobs.wait_any())
            .await
            .unwrap();
    };
    assert_eq!(
        notice,
        format!(
            "<video id=\"{id}\" state=\"completed\">\nVideo request finished. Saved 2 videos: {}, {}.\n</video>",
            file.display(),
            second.display()
        )
    );
    assert_eq!(std::fs::read(&file).unwrap(), mp4_with(0));
    assert_eq!(std::fs::read(&second).unwrap(), mp4_with(1));
    let start = server.requests()[0].json();
    assert_eq!(start["model"], "director");
    assert_eq!(
        start["response_format"],
        json!({"type": "video", "aspect_ratio": "16:9", "resolution": "720p", "duration": "6s"})
    );
    let input = start["input"].as_array().unwrap();
    assert_eq!(input[0]["type"], "image");
    assert_eq!(input[0]["mime_type"], "image/png");
    assert!(input[0]["data"].is_string());
    assert_eq!(
        input[1],
        json!({"type": "image", "uri": "https://example.test/last.png", "mime_type": "image/png"})
    );
    let kinds: Vec<&str> = input[2..4]
        .iter()
        .map(|c| c["mime_type"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["audio/mpeg", "video/mp4"]);
    assert_eq!(
        input[4],
        json!({"type": "text", "text": "the kite takes off"})
    );
    // The job polled is the provider's id that submission returned.
    assert!(server.requests()[1..]
        .iter()
        .all(|r| r.path == "/v1beta/interactions/remote_1"));
    let asked: Vec<(String, Vec<String>)> = rec
        .asked
        .lock()
        .unwrap()
        .iter()
        .map(|r| (r.action.clone(), r.resources.clone()))
        .collect();
    let s = |a: &str, r: &str| (a.to_string(), vec![r.to_string()]);
    assert_eq!(
        asked,
        [
            s("experimental_video_generate", "test/director"),
            s("edit", "kite.mp4"),
            s("read", "first.png"),
            s("webfetch", "https://example.test/last.png"),
            s("webfetch", "https://example.test/voice.mp3?sig=1"),
            s("webfetch", "https://example.test/motion.mp4"),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_video_tool_is_offered_only_with_a_video_model_and_off_with_experimental_video_generate_false(
) {
    let state = Arc::new(Mutex::new(State::default()));
    let server = google(state).await;
    let on = setup_config(&server.url, GOOGLE, "test");
    assert!(tool::offered(&on));
    let mut off = on.clone();
    off.experimental.video_generate = Some(json!(false));
    assert!(!tool::offered(&off));
    assert!(!tool::offered(&config(vec![], &[], json!({}))));
    assert_eq!(
        tool::video_path(std::path::Path::new("/w/cat.mp4"), 0),
        std::path::Path::new("/w/cat.mp4")
    );
    assert_eq!(
        tool::video_path(std::path::Path::new("/w/cat.mp4"), 2),
        std::path::Path::new("/w/cat-2.mp4")
    );
}
