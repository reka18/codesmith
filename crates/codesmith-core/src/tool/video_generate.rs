//! `experimental_video_generate`: make a video with a video model, in the
//! background (after OpenCode's `VideoGenerateTool`).
//!
//! The call checks the model and the permissions, reads the frames and
//! references, and submits the job in the foreground (so a rejected
//! request fails the call); then a background job waits for it (see
//! [`crate::video`]) and copies each video from the blob store into place,
//! the first to `path`, the others next to it with `-1`, `-2`, ….

use super::image_generate::{self as image, Env};
use super::Ctx;
use crate::config::Config;
use crate::generate::{self, MediaModel};
use crate::permission::Request;
use crate::video::{self, Job, Videos};
use codesmith_ai::Media;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const NAME: &str = "experimental_video_generate";

/// The largest input file (the media cap).
pub const MAX_INPUT_BYTES: u64 = crate::media::MAX_INGEST_BYTES;

pub const DESCRIPTION: &str = "Generates a video from a prompt, optionally starting or ending on given images and guided by reference images, videos or audio.
Only call this when the user asks for a video: videos are slow and expensive.
It runs in the background, and you are notified with every saved file when it finishes. Never poll, sleep, or check for the file while you wait.";

pub fn spec() -> (String, Value) {
    let source = json!({"type": "string", "minLength": 1});
    (
        DESCRIPTION.into(),
        json!({
            "type": "object",
            "properties": {
                "prompt": {"type": "string", "minLength": 1, "description": "What happens in the video. Describe the subject, action, camera movement, style and, when sound is on, any dialogue or sound."},
                "start": {"type": "string", "minLength": 1, "description": "Image the video starts on exactly. A local file path (relative to the session directory or absolute) or an http(s) URL."},
                "end": {"type": "string", "minLength": 1, "description": "Image the video ends on exactly. A local file path or an http(s) URL."},
                "references": {"type": "array", "minItems": 1, "items": source, "description": "Images, videos or audio that guide the subject, style, motion or voice without being shown as-is. Local file paths or http(s) URLs."},
                "duration": {"type": "integer", "exclusiveMinimum": 0, "description": "Length in seconds."},
                "aspect": {"type": "string", "description": "Aspect ratio as \"width:height\", such as \"16:9\" or \"9:16\", or \"auto\" to follow the start image."},
                "resolution": {"type": "string", "description": "Output resolution, such as \"480p\", \"720p\", \"1080p\" or \"4k\"."},
                "sound": {"type": "boolean", "description": "Whether the video has generated sound. Unset uses the model's default."},
                "model": {"type": "string", "description": "NEVER set this unless the user explicitly asks for a particular video model. The value is written as \"providerID/modelID\". Do not guess the ID."},
                "path": {"type": "string", "description": "Where to save the video, relative to the session directory or absolute, usually ending in .mp4. An existing file is overwritten. Further videos from the same job go next to it with -1, -2, … before the extension. Defaults to a new file in the session's temp folder."}
            },
            "required": ["prompt"]
        }),
    )
}

/// Whether config turns the tool on: off unless `experimental.video_generate`
/// is `true` or a `{"model": …}` object. Video generation is slow and costly,
/// so (as in OpenCode's release builds) it is opt-in.
pub fn enabled(config: &Config) -> bool {
    match config.experimental.video_generate.as_ref() {
        Some(Value::Bool(on)) => *on,
        Some(Value::Object(_)) => true,
        _ => false,
    }
}

/// Offered while a video model is reachable and config leaves it on.
pub fn offered(config: &Config) -> bool {
    enabled(config) && !generate::models(config, "video").is_empty()
}

/// The model: the call's, else the configured one if available, else the
/// session provider's newest, else the newest.
pub fn resolve_model(
    config: &Config,
    available: &[MediaModel],
    input: Option<&str>,
    provider: Option<&str>,
) -> Result<MediaModel, String> {
    let Some(input) = input else {
        let configured = config
            .experimental
            .video_generate
            .as_ref()
            .and_then(image::model_setting);
        let parsed = configured.as_deref().and_then(generate::parse_ref);
        return parsed
            .and_then(|(p, m, _)| available.iter().find(|x| x.provider_id == p && x.id == m))
            .or_else(|| {
                available
                    .iter()
                    .find(|x| Some(x.provider_id.as_str()) == provider)
            })
            .or(available.first())
            .cloned()
            .ok_or_else(|| "No video model is available.".to_string());
    };
    let (p, m, _) = generate::parse_ref(input)
        .ok_or_else(|| format!("Invalid model \"{input}\". Use \"providerID/modelID\"."))?;
    available
        .iter()
        .find(|x| x.provider_id == p && x.id == m)
        .cloned()
        .ok_or_else(|| format!("Model \"{p}/{m}\" is not an available video model."))
}

/// Where the job's `index`th video is saved: the path itself for the
/// first, then `cat-1.mp4`, `cat-2.mp4`, ….
pub fn video_path(file: &Path, index: usize) -> PathBuf {
    if index == 0 {
        return file.to_path_buf();
    }
    let ext = file.extension().map(|e| e.to_string_lossy().to_string());
    let stem = file.with_extension("");
    match ext {
        Some(e) => PathBuf::from(format!("{}-{index}.{e}", stem.display())),
        None => PathBuf::from(format!("{}-{index}", stem.display())),
    }
}

/// The completion notice for a background video job.
pub fn notice(id: &str, state: &str, body: &str) -> String {
    format!("<video id=\"{id}\" state=\"{state}\">\n{body}\n</video>")
}

/// The type a URL's extension names, when it names a known one.
fn url_type(url: &str) -> Option<&'static str> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "ogg" => "audio/ogg",
        "flac" => "audio/flac",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => return None,
    })
}

fn audio_type(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(b"ID3") || (b.len() > 1 && b[0] == 0xff && b[1] & 0xe0 == 0xe0) {
        Some("audio/mpeg")
    } else if b.starts_with(b"RIFF") && b.len() >= 12 && &b[8..12] == b"WAVE" {
        Some("audio/wav")
    } else if b.starts_with(b"OggS") {
        Some("audio/ogg")
    } else if b.starts_with(b"fLaC") {
        Some("audio/flac")
    } else {
        None
    }
}

/// One input: a URL goes to the provider as it is (under the `webfetch`
/// rule, since the agent chose it); a local file is read and sent inline.
async fn read_media(ctx: &Ctx<'_>, value: &str, kinds: &[&str]) -> Result<Media, String> {
    let m = if image::is_url(value) {
        // The provider fetches it, so its redirects are out of reach; the
        // URL itself gets webfetch's checks, and "always" saves its origin.
        // The provider is sent the URL as the rules saw it.
        let parsed = super::webfetch::authorize(ctx, value)?;
        let value = parsed.as_str();
        Media::url(url_type(value).unwrap_or("application/octet-stream"), value)
    } else {
        let path = super::read::authorize(ctx, value)?;
        let meta = std::fs::metadata(&path).map_err(|_| format!("File not found: {value}"))?;
        if meta.len() > MAX_INPUT_BYTES {
            return Err(format!("{value} is larger than 20 MiB."));
        }
        let bytes = std::fs::read(&path).map_err(|e| format!("Unable to read {value}: {e}"))?;
        let head = &bytes[..bytes.len().min(crate::media::SNIFF_BYTES)];
        let mime = crate::media::detect(head)
            .or_else(|| audio_type(head))
            .ok_or_else(|| format!("{value} is not a supported image, video or audio file."))?;
        Media::base64(mime, crate::media::base64(&bytes))
    };
    // A URL without a known extension has no type yet; the provider checks it.
    let kind = m.mime.split('/').next().unwrap_or("");
    if m.mime != "application/octet-stream" && !kinds.contains(&kind) {
        let list = match kinds {
            [one] => one.to_string(),
            [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
            [] => String::new(),
        };
        return Err(format!("{value} must be {list}."));
    }
    Ok(m)
}

/// Start a video job; returns the call's text.
pub async fn run(ctx: &Ctx<'_>, env: Env<'_>, input: &Value) -> Result<String, String> {
    start(ctx, env, input)
        .await
        .map_err(|e| format!("Unable to generate video: {e}"))
}

fn valid_aspect(a: &str) -> bool {
    a == "auto"
        || a.split_once(':').is_some_and(|(w, h)| {
            w.parse::<u32>().is_ok_and(|n| n > 0) && h.parse::<u32>().is_ok_and(|n| n > 0)
        })
}

async fn start(ctx: &Ctx<'_>, env: Env<'_>, input: &Value) -> Result<String, String> {
    let prompt = input["prompt"]
        .as_str()
        .filter(|p| !p.is_empty())
        .ok_or("Missing required parameter `prompt`")?
        .to_string();
    let available = generate::models(env.config, "video");
    let model = resolve_model(
        env.config,
        &available,
        input["model"].as_str(),
        env.provider,
    )?;
    let reference = model.spec();
    let aspect = input["aspect"].as_str().map(str::to_string);
    if let Some(a) = &aspect {
        if !valid_aspect(a) {
            return Err(format!(
                "Invalid aspect \"{a}\". Use \"width:height\", such as \"16:9\", or \"auto\"."
            ));
        }
    }
    let target = input["path"].as_str().map(|p| ctx.resolve(p));
    ctx.ask(Request {
        action: NAME.into(),
        resources: vec![reference.clone()],
        save: vec!["*".into()],
        title: format!("Generate a video with {reference}"),
        access: None,
    })?;
    if let Some(t) = &target {
        ctx.authorize_external(t, false, super::Access::Write)?;
        ctx.ask(Request {
            action: "edit".into(),
            resources: t.resources.clone(),
            save: vec!["*".into()],
            title: format!("Write {}", t.resource),
            access: None,
        })?;
    }
    let frame = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(str::to_string);
    let start = match frame(&input["start"]) {
        Some(s) => Some(read_media(ctx, &s, &["image"]).await?),
        None => None,
    };
    let end = match frame(&input["end"]) {
        Some(s) => Some(read_media(ctx, &s, &["image"]).await?),
        None => None,
    };
    let mut references = Vec::new();
    for r in input["references"].as_array().into_iter().flatten() {
        let r = r.as_str().ok_or("`references` must list paths or URLs")?;
        references.push(read_media(ctx, r, &["image", "video", "audio"]).await?);
    }
    let store_path = env
        .store
        .clone()
        .ok_or("Video generation needs the session store.")?;
    let store = crate::store::Store::open(&store_path).map_err(|e| format!("{e:#}"))?;
    let videos = Videos {
        store: &store,
        config: env.config,
        client: &ctx.client,
    };
    // Submitting runs in the foreground, so a rejected request fails the call.
    let id = videos
        .start(&video::Input {
            model: reference.clone(),
            prompt: prompt.clone(),
            start,
            end,
            references,
            duration: input["duration"]
                .as_u64()
                .filter(|d| *d > 0)
                .map(|d| d as u32),
            aspect,
            resolution: input["resolution"].as_str().map(str::to_string),
            sound: input["sound"].as_bool(),
        })
        .await
        .map_err(|e| e.to_string())?;
    let file = match &target {
        Some(t) => t.absolute.clone(),
        None => crate::paths::tmp_dir()
            .join("videos")
            .join(&ctx.session_id)
            .join(format!("{id}.mp4")),
    };
    track(Track {
        jobs: env.jobs,
        config: env.config.clone(),
        client: ctx.client.clone(),
        store: store_path,
        id: id.clone(),
        file: file.clone(),
        title: prompt,
        interval: env.retry.poll,
    });
    // The model is named only when the tool picked it.
    let with = if input["model"].is_string() {
        String::new()
    } else {
        format!(" with {reference}")
    };
    Ok([
        format!("Video job {id} started in the background{with}. It will be saved to {}; if the model returns more than one video, the others are saved next to it with -1, -2, … added to the name.", file.display()),
        "You will be notified automatically when it finishes or fails. Videos usually take a few minutes.".into(),
        "DO NOT sleep, poll, check whether the file exists, or request the same video again while it runs.".into(),
        "Work on other tasks, or briefly tell the user what you started and end your response.".into(),
    ]
    .join("\n"))
}

/// What the background half needs.
pub struct Track {
    pub jobs: super::shell::Jobs,
    pub config: Config,
    pub client: reqwest::Client,
    pub store: PathBuf,
    pub id: String,
    pub file: PathBuf,
    pub title: String,
    pub interval: std::time::Duration,
}

/// Wait for video job `id` as a background job of `jobs`, then copy each
/// of its videos from the blob store into place; the notice lists them.
/// Cancelling the background job cancels the video job.
pub fn track(t: Track) {
    let (done_id, cancel_id) = (t.id.clone(), t.id.clone());
    let shown = t.file.display().to_string();
    let cancel = (t.config.clone(), t.client.clone(), t.store.clone());
    let (jobs, id, title) = (t.jobs.clone(), t.id.clone(), t.title.clone());
    let task = async move {
        let failed = |e: String| format!("Video request failed for {}: {e}", t.file.display());
        let result: Result<String, String> = async {
            let store = crate::store::Store::open(&t.store).map_err(|e| format!("{e:#}"))?;
            let videos = Videos {
                store: &store,
                config: &t.config,
                client: &t.client,
            };
            let job = videos
                .wait(&t.id, t.interval)
                .await
                .map_err(|e| format!("{e:#}"))?;
            let blobs = match job {
                Some(Job::Completed { blobs }) => blobs,
                Some(Job::Error { error, .. }) => return Err(error),
                Some(Job::Running) | None => return Err(format!("Video not found: {}", t.id)),
            };
            let mut files = Vec::new();
            for (i, b) in blobs.iter().enumerate() {
                let file = video_path(&t.file, i);
                let (_, data) = store
                    .blob(&b.id)
                    .map_err(|e| format!("{e:#}"))?
                    .ok_or_else(|| format!("Video not found: {}", t.id))?;
                if let Some(dir) = file.parent() {
                    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
                }
                std::fs::write(&file, data).map_err(|e| e.to_string())?;
                files.push(file.display().to_string());
            }
            Ok(if files.len() == 1 {
                format!("Video request finished. Saved video: {}.", files[0])
            } else {
                format!(
                    "Video request finished. Saved {} videos: {}.",
                    files.len(),
                    files.join(", ")
                )
            })
        }
        .await;
        match result {
            Ok(text) => notice(&done_id, "completed", &text),
            Err(e) => notice(&done_id, "error", &failed(e)),
        }
    };
    let cancelled = async move {
        if let Ok(store) = crate::store::Store::open(&cancel.2) {
            let videos = Videos {
                store: &store,
                config: &cancel.0,
                client: &cancel.1,
            };
            let _ = videos.cancel(&cancel_id).await;
        }
        notice(
            &cancel_id,
            "cancelled",
            &format!("Video request for {shown} was cancelled."),
        )
    };
    jobs.spawn_task(&id, &title, task, cancelled);
}

#[cfg(test)]
mod default_tests {
    use super::*;

    #[test]
    fn video_generation_is_opt_in() {
        let mut c = Config::default();
        assert!(!enabled(&c));
        c.experimental.video_generate = Some(Value::Bool(true));
        assert!(enabled(&c));
        c.experimental.video_generate = Some(serde_json::json!({"model": "xai/grok-video"}));
        assert!(enabled(&c));
        c.experimental.video_generate = Some(Value::Bool(false));
        assert!(!enabled(&c));
    }
}

#[cfg(test)]
mod url_tests {
    use super::*;
    use crate::permission::Reply;
    use crate::tool::testing::Env;

    #[tokio::test(flavor = "multi_thread")]
    async fn input_urls_get_webfetchs_checks_and_always_saves_their_origin() {
        let (env, rec) = Env::recording(vec![]);
        let err = read_media(&env.ctx(), "https://user@example.test/a.png", &["image"])
            .await
            .unwrap_err();
        assert!(err.contains("credentials"), "{err}");
        assert!(rec.actions().is_empty());

        let env = Env::with_reply(Reply::Always);
        read_media(&env.ctx(), "https://example.test/a.png", &["image"])
            .await
            .unwrap();
        let saved: Vec<String> = env.gate.saved().into_iter().map(|r| r.resource).collect();
        assert_eq!(
            saved,
            vec![
                "https://example.test".to_string(),
                "https://example.test/*".to_string()
            ]
        );
    }

    /// Rules see an input URL as it is requested, and the provider is sent
    /// that URL: case, a default port, tabs or percent-encoding do not get
    /// a URL past a deny rule.
    #[tokio::test(flavor = "multi_thread")]
    async fn input_urls_match_the_rules_as_requested() {
        use crate::permission::{Effect, Rule};
        let rules = vec![
            Rule::new("webfetch", "*", Effect::Allow),
            Rule::new("webfetch", "https://internal.test/secret*", Effect::Deny),
        ];
        let (env, _) = Env::recording(rules);
        for url in [
            "HTTPS://INTERNAL.TEST/secret.png",
            "https://internal.test:443/secret.png",
            "https://inter\tnal.te\nst/secret.png",
            "https://internal.test/%73ecret.png",
            "https://internal.test/a/../secret.png",
        ] {
            let err = read_media(&env.ctx(), url, &["image"]).await.unwrap_err();
            assert!(err.starts_with("Permission denied"), "{url}: {err}");
        }
        let m = read_media(&env.ctx(), "HTTPS://Internal.Test:443/ok.png#x", &["image"])
            .await
            .unwrap();
        assert_eq!(
            m.source,
            codesmith_ai::Source::Url("https://internal.test/ok.png".into())
        );
    }
}
