//! Image generation, ported from OpenCode's `generate.test.ts` (the image
//! entrypoint) and `tool-image-generate.test.ts` (MIT). The provider is a
//! scripted OpenAI-compatible images endpoint; the tool runs against a
//! `Ctx` whose permission requests are recorded.

mod media_harness;

use codesmith_core::config::Config;
use codesmith_core::generate::{self, GenerateError, ImageInput};
use codesmith_core::permission::{Asker, Gate, Reply, Request};
use codesmith_core::tool::image_generate::{self as tool, Env};
use codesmith_core::tool::shell::Jobs;
use codesmith_core::tool::Ctx;
use media_harness::*;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

const OPENAI: &str = "@opencode/ai/providers/openai/images";

/// Records each permission request; declines those for `deny`.
#[derive(Default)]
struct Recorder {
    asked: Mutex<Vec<Request>>,
    deny: Mutex<Option<String>>,
}

impl Asker for Recorder {
    fn ask(&self, req: &Request) -> Reply {
        self.asked.lock().unwrap().push(req.clone());
        if self.deny.lock().unwrap().as_deref() == Some(req.action.as_str()) {
            Reply::Reject(None)
        } else {
            Reply::Once
        }
    }
}

impl Recorder {
    fn asked(&self) -> Vec<(String, Vec<String>)> {
        self.asked
            .lock()
            .unwrap()
            .iter()
            .map(|r| (r.action.clone(), r.resources.clone()))
            .collect()
    }
}

/// Answers image requests with `image` (base64) and counts them.
async fn images(image: &'static str) -> Server {
    serve(Arc::new(move |s: &Seen| {
        if s.path.ends_with("/images/generations") || s.path.ends_with("/images/edits") {
            json_reply(json!({"data": [{"b64_json": image}], "usage": {"input_tokens": 9, "output_tokens": 272}}))
        } else {
            (404, "text/plain", b"no".to_vec())
        }
    }))
    .await
}

fn painter(id: &str, base: &str, input: &[&str]) -> Value {
    model(id, "image", OPENAI, &format!("{base}/v1"), input, 20)
}

struct Setup {
    /// `None` only once dropped (see `Drop`).
    dir: Option<tempfile::TempDir>,
    gate: Gate,
    rec: Arc<Recorder>,
    jobs: Jobs,
    config: Config,
    /// Notices of finished jobs not returned yet: `take_finished` hands
    /// over every finished job at once.
    pending: std::sync::Mutex<std::collections::VecDeque<String>>,
}

/// Image jobs run on in the background after a test's last check, and one
/// may still be writing the session store in the temporary folder. Deleting
/// the folder under SQLite's mapped WAL index crashes the test process
/// (SIGBUS), so wait for running jobs first, and keep the folder if one is
/// still going.
impl Drop for Setup {
    fn drop(&mut self) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while self.jobs.running() > 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        if self.jobs.running() > 0 {
            if let Some(dir) = self.dir.take() {
                let _ = dir.keep();
            }
        }
    }
}

impl Setup {
    fn new(config: Config) -> Setup {
        let rec = Arc::new(Recorder::default());
        Setup {
            dir: Some(tempfile::tempdir().unwrap()),
            gate: Gate::new(vec![], rec.clone()),
            rec,
            jobs: Jobs::default(),
            config,
            pending: Default::default(),
        }
    }

    fn root(&self) -> std::path::PathBuf {
        self.dir.as_ref().unwrap().path().canonicalize().unwrap()
    }

    fn ctx(&self) -> Ctx<'_> {
        Ctx {
            cwd: self.root(),
            root: self.root(),
            gate: &self.gate,
            shell: "/bin/sh".into(),
            session_id: "ses_test".into(),
            limits: Default::default(),
            client: reqwest::Client::new(),
            jobs: Some(self.jobs.clone()),
            websearch: None,
            format: None,
            lsp: None,
            progress: None,
        }
    }

    async fn run(&self, input: Value) -> Result<String, String> {
        self.run_on(input, Some("test")).await
    }

    async fn run_on(&self, input: Value, provider: Option<&str>) -> Result<String, String> {
        let env = Env {
            config: &self.config,
            provider,
            store: Some(self.root().join("s.db")),
            gate: self.gate.clone(),
            jobs: self.jobs.clone(),
            retry: generate::Options {
                retry_scale: 0.001,
                ..Default::default()
            },
        };
        tool::run(&self.ctx(), env, &input).await
    }

    /// The notice of the next job to finish.
    async fn notice(&self) -> String {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            {
                let mut pending = self.pending.lock().unwrap();
                pending.extend(
                    self.jobs
                        .take_finished()
                        .into_iter()
                        .map(|j| j.notice.unwrap()),
                );
                if let Some(n) = pending.pop_front() {
                    return n;
                }
            }
            // `wait_any` returns at once when nothing runs, so bound the loop
            // as a whole rather than each wait.
            tokio::time::timeout_at(deadline, self.jobs.wait_any())
                .await
                .expect("a job finishes");
            tokio::task::yield_now().await;
        }
    }
}

fn job_id(text: &str) -> String {
    text.split_whitespace().nth(2).unwrap().to_string()
}

fn model_of(text: &str) -> String {
    text.split(" with ")
        .nth(1)
        .unwrap()
        .split(". ")
        .next()
        .unwrap()
        .to_string()
}

// generate.test.ts

#[tokio::test(flavor = "multi_thread")]
async fn oc_generate_generates_images_through_the_models_image_entrypoint_with_the_provider_key() {
    let server = images(PNG).await;
    let config = config(
        vec![provider(
            "acme",
            "@opencode/ai/providers/openai/responses",
            vec![painter("gpt-image-1-mini", &server.url, &["text"])],
        )],
        &["acme"],
        json!({}),
    );
    let result = generate::image(
        &config,
        &reqwest::Client::new(),
        &ImageInput {
            model: "acme/gpt-image-1-mini".into(),
            prompt: "A black circle".into(),
            ..Default::default()
        },
        Default::default(),
    )
    .await
    .unwrap();
    let req = &server.requests()[0];
    assert_eq!(req.path, "/v1/images/generations");
    assert_eq!(req.header("authorization"), Some("Bearer acme-key"));
    assert_eq!(
        req.json(),
        json!({"model": "gpt-image-1-mini", "prompt": "A black circle"})
    );
    assert_eq!(result.images[0].mime, "image/png");
    assert_eq!(result.images[0].data, png_bytes());
    assert_eq!(
        (result.input_tokens, result.output_tokens),
        (Some(9), Some(272))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_generate_keeps_credential_metadata_out_of_image_requests() {
    let server = images(PNG).await;
    let config = config(
        vec![provider(
            "acme",
            "@opencode/ai/providers/openai/responses",
            vec![painter("gpt-image-2", &server.url, &["text"])],
        )],
        &["acme"],
        json!({}),
    );
    generate::image(
        &config,
        &reqwest::Client::new(),
        &ImageInput {
            model: "acme/gpt-image-2".into(),
            prompt: "A black circle".into(),
            ..Default::default()
        },
        Default::default(),
    )
    .await
    .unwrap();
    let req = &server.requests()[0];
    assert_eq!(req.header("authorization"), Some("Bearer acme-key"));
    // Only the request's own fields: no key, account or session metadata.
    assert_eq!(
        req.json(),
        json!({"model": "gpt-image-2", "prompt": "A black circle"})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_generate_downloads_url_images_and_detects_their_type_from_the_bytes() {
    let files = serve(Arc::new(|_: &Seen| {
        (200, "application/octet-stream", png_bytes())
    }))
    .await;
    let url = format!("{}/fox", files.url);
    let server = serve(Arc::new(move |_: &Seen| {
        json_reply(json!({"data": [{"url": url}]}))
    }))
    .await;
    let config = config(
        vec![provider(
            "zai",
            "@opencode/ai/providers/zai/chat",
            vec![model(
                "glm-image",
                "image",
                "@opencode/ai/providers/openai-compatible/images",
                &server.url,
                &["text"],
                1,
            )],
        )],
        &["zai"],
        json!({}),
    );
    let result = generate::image(
        &config,
        &reqwest::Client::new(),
        &ImageInput {
            model: "zai/glm-image".into(),
            prompt: "A fox".into(),
            ..Default::default()
        },
        Default::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        result.images,
        [generate::Image {
            mime: "image/png".into(),
            data: png_bytes()
        }]
    );
    // The download carries no provider key.
    assert_eq!(files.requests()[0].header("authorization"), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_generate_reports_a_chat_model_asked_for_images_as_unsupported() {
    let server = images(PNG).await;
    let chat = model(
        "chat",
        "chat",
        "@opencode/ai/providers/openai-compatible",
        &server.url,
        &["text"],
        1,
    );
    let config = config(
        vec![provider(
            "acme",
            "@opencode/ai/providers/openai/responses",
            vec![chat],
        )],
        &["acme"],
        json!({}),
    );
    let input = |m: &str| ImageInput {
        model: m.into(),
        prompt: "x".into(),
        ..Default::default()
    };
    let client = reqwest::Client::new();
    match generate::image(&config, &client, &input("acme/chat"), Default::default()).await {
        Err(GenerateError::UnsupportedModel { message, .. }) => {
            assert_eq!(message, "acme/chat is a chat model, not a image model")
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        generate::image(&config, &client, &input("acme/none"), Default::default()).await,
        Err(GenerateError::ModelNotFound { .. })
    ));
}

// tool-image-generate.test.ts

#[tokio::test(flavor = "multi_thread")]
async fn oc_image_tool_is_registered_only_while_an_image_model_is_available() {
    let server = images(PNG).await;
    let models = vec![provider(
        "test",
        "@opencode/ai/providers/openai/responses",
        vec![painter("painter", &server.url, &["text"])],
    )];
    // No key: the model is not reachable.
    assert!(!tool::offered(&config(models.clone(), &[], json!({}))));
    assert!(tool::offered(&config(models, &["test"], json!({}))));
    assert!(!tool::offered(&config(vec![], &["test"], json!({}))));
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_image_tool_is_on_by_default_and_off_with_experimental_image_generate_false() {
    let server = images(PNG).await;
    let models = vec![provider(
        "test",
        "@opencode/ai/providers/openai/responses",
        vec![painter("painter", &server.url, &["text"])],
    )];
    for (setting, on) in [
        (json!(null), true),
        (json!(false), false),
        (json!(true), true),
        (json!({}), true),
    ] {
        let c = config(
            models.clone(),
            &["test"],
            json!({"experimental": {"image_generate": setting}}),
        );
        assert_eq!(tool::offered(&c), on, "{setting}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_image_tool_uses_the_configured_model_unless_the_agent_picks_one_and_falls_back_with_a_note(
) {
    let server = images(PNG).await;
    let models = vec![
        provider(
            "test",
            "@opencode/ai/providers/openai/responses",
            vec![painter("painter", &server.url, &["text", "image"])],
        ),
        provider(
            "other",
            "@opencode/ai/providers/openai/responses",
            vec![painter("text-only", &server.url, &["text"])],
        ),
    ];
    let s = Setup::new(config(
        models.clone(),
        &["test", "other"],
        json!({"experimental": {"image_generate": {"model": {"providerID": "other", "model": "text-only"}}}}),
    ));
    std::fs::write(s.root().join("in.png"), png_bytes()).unwrap();
    // The configured model beats the session's provider, and an explicit model beats both.
    let configured = s.run(json!({"prompt": "x"})).await.unwrap();
    assert_eq!(model_of(&configured), "other/text-only");
    assert!(!configured.contains("used"));
    let explicit = s
        .run(json!({"prompt": "x", "model": "test/painter"}))
        .await
        .unwrap();
    assert_eq!(model_of(&explicit), "test/painter");
    // A configured model that can't serve the call is skipped, and the result says why.
    let edit = s
        .run(json!({"prompt": "x", "images": ["in.png"]}))
        .await
        .unwrap();
    assert_eq!(model_of(&edit), "test/painter");
    assert!(
        edit.contains("other/text-only doesn't accept input images; used test/painter."),
        "{edit}"
    );

    let s = Setup::new(config(
        models,
        &["test", "other"],
        json!({"experimental": {"image_generate": {"model": "gone/model"}}}),
    ));
    let missing = s.run(json!({"prompt": "x"})).await.unwrap();
    assert_eq!(model_of(&missing), "test/painter");
    assert!(
        missing.contains("gone/model isn't available; used test/painter."),
        "{missing}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_image_tool_reads_input_images_asks_permission_and_saves_the_result_overwriting_an_existing_file(
) {
    let server = images(PNG).await;
    let s = Setup::new(config(
        vec![provider(
            "test",
            "@opencode/ai/providers/openai/responses",
            vec![painter("painter", &server.url, &["text", "image"])],
        )],
        &["test"],
        json!({}),
    ));
    std::fs::write(s.root().join("in.png"), png_bytes()).unwrap();
    let result = s
        .run(json!({"prompt": "make it blue", "images": ["in.png"], "path": "out.png"}))
        .await
        .unwrap();
    let file = s.root().join("out.png");
    let stem = s.root().join("out");
    let id = job_id(&result);
    assert!(id.starts_with("image_"));
    assert_eq!(
        result,
        [
            format!("Image job {id} started in the background with test/painter. It will be saved as {} with the extension of its real format (.png, .jpg, .webp or .gif); the notice gives the exact path.", stem.display()),
            "You will be notified automatically when it finishes or fails.".into(),
            "DO NOT sleep, poll, check whether the file exists, or request the same image again while it runs.".into(),
            "Work on other tasks, or briefly tell the user what you started and end your response.".into(),
        ]
        .join("\n")
    );
    // The notice carries the job ID from the start result, like shell notices.
    assert_eq!(
        s.notice().await,
        format!("<image id=\"{id}\" state=\"completed\">\nImage request finished. Saved image: {} (test/painter).\nUse the read tool on this path to view the image.\n</image>", file.display())
    );
    assert_eq!(std::fs::read(&file).unwrap(), png_bytes());
    // An edit: the input image goes up as multipart.
    let req = &server.requests()[0];
    assert_eq!(req.path, "/v1/images/edits");
    let body = String::from_utf8_lossy(&req.body);
    assert!(
        body.contains("name=\"prompt\"\r\n\r\nmake it blue"),
        "{body}"
    );
    assert!(body.contains("name=\"n\"\r\n\r\n1"), "{body}");
    assert!(
        body.contains("name=\"image[]\"; filename=\"image-0\"\r\nContent-Type: image/png"),
        "{body}"
    );
    assert_eq!(
        s.rec.asked(),
        [
            (
                "experimental_image_generate".into(),
                vec!["test/painter".into()]
            ),
            ("edit".into(), vec!["out.png".into()]),
            ("read".into(), vec!["in.png".into()]),
        ]
    );
    // The image is kept once in the session's blob store too.
    let db = codesmith_core::store::Store::open(&s.root().join("s.db")).unwrap();
    let h = codesmith_core::blob::hash(&png_bytes());
    assert_eq!(db.blob(&h).unwrap().unwrap().1, png_bytes());

    // Running again with the same path replaces the file, like the write tool.
    s.run(json!({"prompt": "again", "path": "out.png"}))
        .await
        .unwrap();
    assert!(s.notice().await.contains("state=\"completed\""));
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_image_tool_saves_with_the_extension_of_the_images_real_format() {
    let server = images(JPEG).await;
    let s = Setup::new(config(
        vec![provider(
            "test",
            "@opencode/ai/providers/openai/responses",
            vec![painter("painter", &server.url, &["text"])],
        )],
        &["test"],
        json!({}),
    ));
    // The default path lives in the session temp folder and takes the real extension.
    let result = s.run(json!({"prompt": "a cat"})).await.unwrap();
    let notice = s.notice().await;
    let saved = notice
        .split("Saved image: ")
        .nth(1)
        .unwrap()
        .split(" (")
        .next()
        .unwrap()
        .to_string();
    assert!(saved.ends_with(".jpg"), "{saved}");
    assert!(saved.contains("/images/ses_test/image_"), "{saved}");
    assert!(
        saved.starts_with(&isolate().display().to_string()),
        "{saved}"
    );
    assert!(result.contains(saved.trim_end_matches(".jpg")));
    assert!(std::path::Path::new(&saved).exists());
    let _ = std::fs::remove_file(&saved);

    // An explicit path's extension is replaced too, and the notice gives the real path.
    s.run(json!({"prompt": "a cat", "path": "cat.png"}))
        .await
        .unwrap();
    assert!(s.notice().await.contains(&format!(
        "Saved image: {} (test/painter).",
        s.root().join("cat.jpg").display()
    )));
    assert!(s.root().join("cat.jpg").exists());
    // The edit prompt covered cat.png; a new cat.jpg only had to pass the deny rules.
    let edits: Vec<Vec<String>> = s
        .rec
        .asked()
        .into_iter()
        .filter(|(a, _)| a == "edit")
        .map(|(_, r)| r)
        .collect();
    assert_eq!(edits, [vec!["cat.png".to_string()]]);

    // Replacing an existing file under the real extension asks for its own approval.
    s.rec.asked.lock().unwrap().clear();
    s.run(json!({"prompt": "a cat", "path": "cat.png"}))
        .await
        .unwrap();
    s.notice().await;
    let edits: Vec<Vec<String>> = s
        .rec
        .asked()
        .into_iter()
        .filter(|(a, _)| a == "edit")
        .map(|(_, r)| r)
        .collect();
    assert_eq!(
        edits,
        [vec!["cat.png".to_string()], vec!["cat.jpg".to_string()]]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_renamed_image_is_not_written_outside_the_project_through_a_symlink() {
    let server = images(JPEG).await;
    let s = Setup::new(config(
        vec![provider(
            "test",
            "@opencode/ai/providers/openai/responses",
            vec![painter("painter", &server.url, &["text"])],
        )],
        &["test"],
        json!({}),
    ));
    let outside = tempfile::tempdir().unwrap();
    let victim = outside.path().canonicalize().unwrap().join("victim.jpg");
    std::fs::write(&victim, "keep").unwrap();
    // cat.png was approved; the real format renames it to cat.jpg, a
    // symlink out of the project.
    std::os::unix::fs::symlink(&victim, s.root().join("cat.jpg")).unwrap();
    s.run(json!({"prompt": "a cat", "path": "cat.png"}))
        .await
        .unwrap();
    let notice = s.notice().await;
    assert!(notice.contains("state=\"error\""), "{notice}");
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
    // The renamed path got the outside-the-project check.
    let external: Vec<Vec<String>> = s
        .rec
        .asked()
        .into_iter()
        .filter(|(a, _)| a == "external_directory")
        .map(|(_, r)| r)
        .collect();
    assert!(
        external
            .iter()
            .any(|r| r.iter().any(|x| x.ends_with("victim.jpg"))),
        "{external:?}"
    );

    // Declining that check stops the job before anything is written.
    *s.rec.deny.lock().unwrap() = Some("external_directory".into());
    std::fs::remove_file(s.root().join("cat.jpg")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("new.jpg"), s.root().join("cat.jpg")).unwrap();
    s.run(json!({"prompt": "a cat", "path": "cat.png"}))
        .await
        .unwrap();
    let notice = s.notice().await;
    assert!(notice.contains("state=\"error\""), "{notice}");
    assert!(!outside.path().join("new.jpg").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_image_tool_reports_a_failed_generation_in_the_completion_notice() {
    let server = serve(Arc::new(|_: &Seen| {
        (
            400,
            "application/json",
            json!({"error": {"message": "The prompt was rejected", "code": "content_policy_violation"}})
                .to_string()
                .into_bytes(),
        )
    }))
    .await;
    let s = Setup::new(config(
        vec![provider(
            "test",
            "@opencode/ai/providers/openai/responses",
            vec![painter("painter", &server.url, &["text"])],
        )],
        &["test"],
        json!({}),
    ));
    let result = s
        .run(json!({"prompt": "x", "path": "out.png"}))
        .await
        .unwrap();
    assert_eq!(
        s.notice().await,
        format!(
            "<image id=\"{}\" state=\"error\">\nImage request failed for {}: The prompt was rejected\n</image>",
            job_id(&result),
            s.root().join("out").display()
        )
    );
    assert!(!s.root().join("out.png").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_image_tool_cancels_a_running_generation_through_its_job() {
    // A provider that never answers.
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((s, _)) = l.accept().await {
            held.push(s);
        }
    });
    let s = Setup::new(config(
        vec![provider(
            "test",
            "@opencode/ai/providers/openai/responses",
            vec![painter("painter", &url, &["text"])],
        )],
        &["test"],
        json!({}),
    ));
    let result = s
        .run(json!({"prompt": "x", "path": "out.png"}))
        .await
        .unwrap();
    let id = job_id(&result);
    assert_eq!(s.jobs.running(), 1);
    assert!(s.jobs.cancel(&id));
    assert_eq!(
        s.notice().await,
        format!(
            "<image id=\"{id}\" state=\"cancelled\">\nImage request for {} was cancelled.\n</image>",
            s.root().join("out").display()
        )
    );
    assert!(!s.root().join("out.png").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_image_tool_starts_no_job_when_a_permission_is_denied() {
    let server = images(PNG).await;
    let s = Setup::new(config(
        vec![provider(
            "test",
            "@opencode/ai/providers/openai/responses",
            vec![painter("painter", &server.url, &["text"])],
        )],
        &["test"],
        json!({}),
    ));
    *s.rec.deny.lock().unwrap() = Some("edit".into());
    assert!(s
        .run(json!({"prompt": "x", "path": "out.png"}))
        .await
        .is_err());
    let actions: Vec<String> = s.rec.asked().into_iter().map(|(a, _)| a).collect();
    assert_eq!(actions, ["experimental_image_generate", "edit"]);
    assert_eq!(s.jobs.running(), 0);
    assert!(server.requests().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_image_tool_validates_an_explicit_model_and_its_support_for_input_images() {
    let server = images(PNG).await;
    let s = Setup::new(config(
        vec![
            provider(
                "test",
                "@opencode/ai/providers/openai/responses",
                vec![painter("text-only", &server.url, &["text"])],
            ),
            provider(
                "other",
                "@opencode/ai/providers/openai/responses",
                vec![painter("editor", &server.url, &["text", "image"])],
            ),
        ],
        &["test", "other"],
        json!({}),
    ));
    std::fs::write(s.root().join("in.png"), png_bytes()).unwrap();
    assert_eq!(
        s.run(json!({"prompt": "x", "model": "test/missing"}))
            .await
            .unwrap_err(),
        "Unable to generate image: Model \"test/missing\" is not an available image model."
    );
    assert_eq!(
        s.run(json!({"prompt": "x", "model": "test/text-only", "images": ["in.png"]}))
            .await
            .unwrap_err(),
        "Unable to generate image: Model \"test/text-only\" does not accept input images. Image models that do: other/editor."
    );
    // Without an explicit model, input images steer the default to a model that takes them.
    let steered = s
        .run(json!({"prompt": "x", "images": ["in.png"]}))
        .await
        .unwrap();
    assert_eq!(model_of(&steered), "other/editor");
    let explicit = s
        .run(json!({"prompt": "x", "model": "test/text-only"}))
        .await
        .unwrap();
    assert_eq!(model_of(&explicit), "test/text-only");
    s.notice().await;
    s.notice().await;
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_image_tool_passes_a_model_variant_through_from_the_input_or_from_config() {
    let server = images(PNG).await;
    let hd = painter("painter", &server.url, &["text"]);
    let mut plain = painter("plain", &server.url, &["text"]);
    plain["release_date"] = json!("2026-01-01");
    let models = vec![provider(
        "test",
        "@opencode/ai/providers/openai/responses",
        vec![hd, plain],
    )];
    let variants = json!({"providers": {"test": {"models": {"painter": {"variants": {"hd": {"quality": "high"}}}}}}});
    let s = Setup::new(config(models.clone(), &["test"], variants.clone()));
    let explicit = s
        .run(json!({"prompt": "x", "model": "test/painter#hd"}))
        .await
        .unwrap();
    assert_eq!(model_of(&explicit), "test/painter#hd");
    s.notice().await;
    assert_eq!(server.requests().last().unwrap().json()["quality"], "high");
    assert_eq!(
        s.run(json!({"prompt": "x", "model": "test/painter#bogus"})).await.unwrap_err(),
        "Unable to generate image: Variant \"bogus\" is not available for \"test/painter\". Available: hd."
    );
    assert_eq!(
        s.run(json!({"prompt": "x", "model": "test/plain#hd"}))
            .await
            .unwrap_err(),
        "Unable to generate image: Model \"test/plain\" has no variants. Omit the variant."
    );

    let mut extra = variants.clone();
    extra["experimental"] = json!({"image_generate": {"model": {"providerID": "test", "model": "painter", "variant": "hd"}}});
    let s = Setup::new(config(models.clone(), &["test"], extra));
    let configured = s.run(json!({"prompt": "x"})).await.unwrap();
    assert_eq!(model_of(&configured), "test/painter#hd");
    s.notice().await;
    assert_eq!(server.requests().last().unwrap().json()["quality"], "high");

    let mut extra = variants;
    extra["experimental"] = json!({"image_generate": {"model": "test/painter#bogus"}});
    let s = Setup::new(config(models, &["test"], extra));
    let fallback = s.run(json!({"prompt": "x"})).await.unwrap();
    assert!(
        fallback.contains("test/painter#bogus has no variant \"bogus\"; used "),
        "{fallback}"
    );
    s.notice().await;
    assert!(server
        .requests()
        .last()
        .unwrap()
        .json()
        .get("quality")
        .is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_image_tool_asks_webfetch_permission_before_downloading_an_image_url() {
    let server = images(PNG).await;
    let s = Setup::new(config(
        vec![provider(
            "test",
            "@opencode/ai/providers/openai/responses",
            vec![painter("painter", &server.url, &["text", "image"])],
        )],
        &["test"],
        json!({}),
    ));
    *s.rec.deny.lock().unwrap() = Some("webfetch".into());
    assert!(s
        .run(json!({"prompt": "x", "images": ["https://example.invalid/cat.png"]}))
        .await
        .is_err());
    assert_eq!(
        s.rec.asked(),
        [
            (
                "experimental_image_generate".into(),
                vec!["test/painter".into()]
            ),
            (
                "webfetch".into(),
                vec!["https://example.invalid/cat.png".into()]
            ),
        ]
    );
    assert!(server.requests().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_image_tool_rejects_input_files_that_are_not_images() {
    let server = images(PNG).await;
    let s = Setup::new(config(
        vec![provider(
            "test",
            "@opencode/ai/providers/openai/responses",
            vec![painter("painter", &server.url, &["text", "image"])],
        )],
        &["test"],
        json!({}),
    ));
    std::fs::write(s.root().join("notes.txt"), "hello").unwrap();
    assert_eq!(
        s.run(json!({"prompt": "x", "images": ["notes.txt"]}))
            .await
            .unwrap_err(),
        "Unable to generate image: notes.txt is not a PNG, JPEG, WebP or GIF image."
    );
    assert_eq!(
        s.run(json!({"prompt": "x", "images": ["missing.png"]}))
            .await
            .unwrap_err(),
        "Unable to generate image: File not found: missing.png"
    );
    assert!(server.requests().is_empty());
}

fn info(provider: &str, id: &str, input: &[&str], released: i64) -> generate::MediaModel {
    generate::MediaModel {
        provider_id: provider.into(),
        id: id.into(),
        model_id: id.into(),
        name: id.into(),
        kind: "image".into(),
        package: OPENAI.into(),
        input: input.iter().map(|s| s.to_string()).collect(),
        variants: vec![],
        released,
        provider: codesmith_ai::Provider {
            protocol: codesmith_ai::Protocol::OpenaiChat,
            name: provider.into(),
            base_url: "http://x".into(),
            api_key: None,
            headers: vec![],
            query: vec![],
            auth: codesmith_ai::Auth::Default,
            encoding: codesmith_ai::Encoding::None,
            compat: Default::default(),
            hooks: None,
        },
        media_input: None,
    }
}

fn pick(models: &[generate::MediaModel], provider: Option<&str>, images: bool) -> Option<String> {
    tool::default_model(models, provider, images).map(|m| m.spec())
}

#[test]
fn oc_image_default_model_prefers_the_sessions_provider_then_the_newest() {
    // `models` lists newest first.
    let all = [
        info("fresh", "newest", &["text", "image"], 3),
        info("openai", "gpt-image-2", &["text", "image"], 2),
        info("mine", "own-image", &["text", "image"], 1),
    ];
    assert_eq!(
        pick(&all, Some("mine"), false).as_deref(),
        Some("mine/own-image")
    );
    assert_eq!(
        pick(&all, Some("elsewhere"), false).as_deref(),
        Some("fresh/newest")
    );
    assert_eq!(pick(&all, None, false).as_deref(), Some("fresh/newest"));
    assert_eq!(pick(&[], None, false), None);
}

#[test]
fn oc_image_default_model_only_considers_models_that_accept_input_images_when_images_are_passed() {
    let text_only = info("mine", "text-only", &["text"], 2);
    let editor = info("other", "editor", &["text", "image"], 1);
    let both = [text_only.clone(), editor];
    assert_eq!(
        pick(&both, Some("mine"), true).as_deref(),
        Some("other/editor")
    );
    assert_eq!(pick(&[text_only], Some("mine"), true), None);
    assert_eq!(
        pick(&both, Some("mine"), false).as_deref(),
        Some("mine/text-only")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_image_models_are_listed_newest_first_and_only_with_a_key() {
    let server = images(PNG).await;
    let mut old = painter("old", &server.url, &["text"]);
    old["release_date"] = json!("2026-01-01");
    let config = config(
        vec![
            provider(
                "test",
                "@opencode/ai/providers/openai/responses",
                vec![old, painter("new", &server.url, &["text"])],
            ),
            provider(
                "nokey",
                "@opencode/ai/providers/openai/responses",
                vec![painter("x", &server.url, &["text"])],
            ),
        ],
        &["test"],
        json!({}),
    );
    let specs: Vec<String> = generate::models(&config, "image")
        .iter()
        .map(|m| m.spec())
        .collect();
    assert_eq!(specs, ["test/new", "test/old"]);
    assert!(generate::models(&config, "video").is_empty());
}

/// A Chat Completions model that asks for an image on "draw", then
/// answers; it records each request body.
async fn drawing_model(log: Arc<Mutex<Vec<Value>>>) -> Server {
    serve(Arc::new(move |s: &Seen| {
        let body = s.json();
        log.lock().unwrap().push(body.clone());
        let msgs = body["messages"].as_array().cloned().unwrap_or_default();
        let draw = msgs.last().is_some_and(|m| m["role"] == "user" && m["content"] == "draw")
            && !msgs.iter().any(|m| m["role"] == "tool");
        let chunk = if draw {
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1", "type": "function", "function": {"name": "experimental_image_generate", "arguments": "{\"prompt\": \"a circle\", \"path\": \"circle.png\"}"}}]}, "finish_reason": "tool_calls"}]})
        } else {
            json!({"choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}]})
        };
        (200, "text/event-stream", format!("data: {chunk}\n\ndata: [DONE]\n\n").into_bytes())
    }))
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_image_tool_runs_in_a_session_and_reports_its_notice_to_the_model() {
    use codesmith_core::config::Loaded;
    use codesmith_core::permission::Fixed;
    use codesmith_core::session::{Options, Session, Sink};
    struct Quiet;
    impl Sink for Quiet {
        fn text(&mut self, _: &str) {}
    }
    let images = images(PNG).await;
    let log = Arc::new(Mutex::new(Vec::new()));
    let chat = drawing_model(log.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let config = config(
        vec![provider(
            "test",
            "",
            vec![painter("painter", &images.url, &["text"])],
        )],
        &["test"],
        json!({
            "model": "fake/m",
            "providers": {"fake": {"protocol": "openai-chat",
                "settings": {"baseURL": format!("{}/v1", chat.url), "apiKey": "x"}, "models": {"m": {}}}},
        }),
    );
    let loaded = Loaded {
        config,
        files: vec![],
        root: root.clone(),
    };
    let mut s = Session::new(
        &loaded,
        root.clone(),
        Options {
            agent: None,
            model: None,
            resume: None,
            title: Some("t".into()),
            parent: None,
            id: None,
            questioner: None,
            mcp: None,
        },
        Arc::new(Fixed(Reply::Once)),
        Some(codesmith_core::store::Store::open(&root.join("s.db")).unwrap()),
    )
    .unwrap();
    s.set_retry_scale(0.001);
    s.prompt("draw", &mut Quiet).await.unwrap();
    s.settle_background(&mut Quiet).await.unwrap();
    let first = log.lock().unwrap()[0].clone();
    assert!(first["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["function"]["name"] == "experimental_image_generate"));
    let last = log.lock().unwrap().last().unwrap().clone();
    let text = last.to_string();
    assert!(text.contains("state=\\\"completed\\\""), "{text}");
    assert!(text.contains("circle.png (test/painter)"), "{text}");
    assert_eq!(std::fs::read(root.join("circle.png")).unwrap(), png_bytes());
}
