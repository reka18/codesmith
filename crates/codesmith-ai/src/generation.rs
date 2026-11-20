//! Image and video generation APIs (after OpenCode's `ImageClient` and
//! `VideoClient` with the `openai-images`, `xai-images`, `google-images`,
//! `xai-video` and `google-interactions-video` protocols).
//!
//! An image is one request and response. A video is a job: [`video_start`]
//! submits it and returns the provider's job id, which [`video_status`],
//! [`video_result`] and [`video_cancel`] take.
//!
//! The [`Provider`] gives the base URL, the key and the headers; which wire
//! API to speak comes from the model's catalog package ([`image_api`],
//! [`video_api`]).

use crate::{Error, Media, Provider, Source};
use base64::Engine;
use serde_json::{json, Map, Value};

/// How a provider generates images.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageApi {
    /// OpenAI's `/images/generations` and `/images/edits` (input images
    /// uploaded as multipart), and the hosts compatible with it.
    OpenAI,
    /// `/images/generations` with input images as `input_references`, for
    /// gateways without an edits endpoint (OpenRouter).
    OpenAIReferences,
    /// xAI's `/images/generations` and `/images/edits` (JSON).
    XAI,
    /// Gemini `generateContent` answering with images.
    Google,
}

/// How a provider generates videos.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoApi {
    /// xAI's `/videos/generations`, read back at `/videos/{id}`.
    XAI,
    /// Google's Interactions API (`/interactions`, in the background).
    Google,
}

fn entrypoint(package: &str) -> &str {
    package
        .trim_start_matches("@opencode/ai/providers/")
        .trim_end_matches('/')
}

/// The image API of a catalog package (`@opencode/ai/providers/<p>/images`).
pub fn image_api(package: &str) -> Option<ImageApi> {
    match entrypoint(package) {
        "openai/images" | "openai-compatible/images" => Some(ImageApi::OpenAI),
        "openrouter/images" => Some(ImageApi::OpenAIReferences),
        "xai/images" => Some(ImageApi::XAI),
        "google/images" => Some(ImageApi::Google),
        _ => None,
    }
}

/// The video API of a catalog package (`@opencode/ai/providers/<p>/videos`).
pub fn video_api(package: &str) -> Option<VideoApi> {
    match entrypoint(package) {
        "xai/videos" => Some(VideoApi::XAI),
        "google/videos" => Some(VideoApi::Google),
        _ => None,
    }
}

/// The base URL an API defaults to when the provider names none.
pub fn default_base_url(package: &str) -> Option<&'static str> {
    let p = entrypoint(package);
    Some(match p.split('/').next()? {
        "openai" => "https://api.openai.com/v1",
        "xai" => "https://api.x.ai/v1",
        "google" => "https://generativelanguage.googleapis.com/v1beta",
        "openrouter" => "https://openrouter.ai/api/v1",
        _ => return None,
    })
}

/// One image request.
#[derive(Debug, Clone, Default)]
pub struct ImageRequest {
    /// The provider's model id.
    pub model: String,
    pub prompt: String,
    /// Images to edit, or to use as references, inline or as URLs.
    pub images: Vec<Media>,
    /// How many images; one when unset.
    pub n: Option<u32>,
    /// `width:height`.
    pub aspect: Option<String>,
    /// A size tier (`1K`, `2K`, `4K`).
    pub resolution: Option<String>,
    /// Provider-native fields merged into the body (a variant's `body`).
    pub options: Map<String, Value>,
}

/// A generated image or video: inline bytes, or a URL to download.
#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    Bytes {
        /// The type the provider reported, if any.
        mime: Option<String>,
        data: Vec<u8>,
    },
    /// A URL to fetch; `authorized` URLs need the provider's key.
    Url {
        url: String,
        mime: Option<String>,
        authorized: bool,
    },
}

/// What an image request returned.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImageResponse {
    pub images: Vec<Output>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

fn merge(base: &mut Map<String, Value>, over: &Map<String, Value>) {
    for (k, v) in over {
        if v.is_null() {
            base.remove(k);
        } else {
            base.insert(k.clone(), v.clone());
        }
    }
}

/// `{"a": null}` fields left out.
fn compact(m: Map<String, Value>) -> Map<String, Value> {
    m.into_iter().filter(|(_, v)| !v.is_null()).collect()
}

fn data_url(m: &Media) -> Result<String, Error> {
    match &m.source {
        Source::Base64(d) => Ok(format!("data:{};base64,{d}", m.mime)),
        Source::Url(u) => Ok(u.clone()),
    }
}

fn inline_bytes(m: &Media) -> Option<Vec<u8>> {
    match &m.source {
        Source::Base64(d) => base64::engine::general_purpose::STANDARD.decode(d).ok(),
        Source::Url(_) => None,
    }
}

fn decode_b64(what: &str, data: &str) -> Result<Vec<u8>, Error> {
    base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .map_err(|_| Error::Decode(format!("{what} returned an image that is not valid base64")))
}

/// The pixel size OpenAI's fixed-size models take nearest `aspect`.
fn openai_size(model: &str, aspect: Option<&str>) -> Option<String> {
    let (w, h) = aspect?.split_once(':')?;
    let ratio = w.trim().parse::<f64>().ok()? / h.trim().parse::<f64>().ok()?;
    let id = model.rsplit('/').next().unwrap_or(model);
    let sizes: &[(u32, u32)] = if id.starts_with("dall-e-3") {
        &[(1024, 1024), (1792, 1024), (1024, 1792)]
    } else {
        &[(1024, 1024), (1536, 1024), (1024, 1536)]
    };
    let best = sizes.iter().min_by(|a, b| {
        let da = (a.0 as f64 / a.1 as f64 - ratio).abs();
        let db = (b.0 as f64 / b.1 as f64 - ratio).abs();
        da.total_cmp(&db)
    })?;
    Some(format!("{}x{}", best.0, best.1))
}

/// A multipart body: text fields, then files.
fn multipart(
    fields: &Map<String, Value>,
    files: &[(String, String, String, Vec<u8>)],
) -> (String, Vec<u8>) {
    let boundary = format!(
        "codesmith-{:016x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    );
    let mut out = Vec::new();
    for (k, v) in fields {
        let text = match v {
            Value::String(s) => s.clone(),
            v => v.to_string(),
        };
        out.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{text}\r\n"
            )
            .as_bytes(),
        );
    }
    for (name, filename, mime, data) in files {
        out.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\nContent-Type: {mime}\r\n\r\n"
            )
            .as_bytes(),
        );
        out.extend_from_slice(data);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (boundary, out)
}

/// The JSON response of a 2xx reply.
async fn json_of(resp: reqwest::Response, what: &str) -> Result<Value, Error> {
    let text = resp.text().await?;
    serde_json::from_str(&text).map_err(|e| {
        Error::Decode(format!(
            "{what} returned an invalid response ({e}): {}",
            crate::quote(&text)
        ))
    })
}

async fn post_json(
    p: &Provider,
    client: &reqwest::Client,
    path: &str,
    body: &Value,
) -> Result<reqwest::Response, Error> {
    let rb = p.json_post(client, path, p.headers.clone(), body);
    crate::endpoint::send(p, rb).await
}

/// Generate images.
pub async fn generate_image(
    p: &Provider,
    client: &reqwest::Client,
    api: ImageApi,
    req: &ImageRequest,
) -> Result<ImageResponse, Error> {
    match api {
        ImageApi::OpenAI | ImageApi::OpenAIReferences => openai_image(p, client, api, req).await,
        ImageApi::XAI => xai_image(p, client, req).await,
        ImageApi::Google => google_image(p, client, req).await,
    }
}

async fn openai_image(
    p: &Provider,
    client: &reqwest::Client,
    api: ImageApi,
    req: &ImageRequest,
) -> Result<ImageResponse, Error> {
    let what = "OpenAI Images";
    if req.options.contains_key("size") && req.aspect.is_some() {
        return Err(Error::Invalid(
            "OpenAI size replaces aspect and resolution; set one or the other".into(),
        ));
    }
    let mut fields = Map::new();
    fields.insert("model".into(), json!(req.model));
    fields.insert("prompt".into(), json!(req.prompt));
    if let Some(n) = req.n {
        fields.insert("n".into(), json!(n));
    }
    if let Some(size) = openai_size(&req.model, req.aspect.as_deref()) {
        fields.insert("size".into(), json!(size));
    }
    let mut overlay = req.options.clone();
    for (from, to) in [
        ("outputFormat", "output_format"),
        ("outputCompression", "output_compression"),
    ] {
        if let Some(v) = overlay.remove(from) {
            overlay.insert(to.into(), v);
        }
    }
    let resp = if req.images.is_empty() {
        merge(&mut fields, &overlay);
        post_json(p, client, "images/generations", &Value::Object(fields)).await?
    } else if api == ImageApi::OpenAIReferences {
        let refs: Vec<Value> = req
            .images
            .iter()
            .map(|m| Ok(json!({"type": "image_url", "image_url": {"url": data_url(m)?}})))
            .collect::<Result<_, Error>>()?;
        fields.insert("input_references".into(), Value::Array(refs));
        merge(&mut fields, &overlay);
        post_json(p, client, "images/generations", &Value::Object(fields)).await?
    } else if let Some(bytes) = req
        .images
        .iter()
        .map(inline_bytes)
        .collect::<Option<Vec<Vec<u8>>>>()
    {
        // Owned bytes upload as multipart edits.
        for k in ["model", "prompt", "image", "image[]", "images", "mask"] {
            overlay.remove(k);
        }
        merge(&mut fields, &overlay);
        let files: Vec<(String, String, String, Vec<u8>)> = bytes
            .into_iter()
            .enumerate()
            .map(|(i, data)| {
                (
                    "image[]".to_string(),
                    format!("image-{i}"),
                    req.images[i].mime.clone(),
                    data,
                )
            })
            .collect();
        let (boundary, body) = multipart(&fields, &files);
        let mut rb = client.post(p.url("images/edits"));
        if !p.query.is_empty() {
            rb = rb.query(&p.query);
        }
        for (k, v) in p.auth_headers(p.headers.clone()) {
            rb = rb.header(k, v);
        }
        let rb = rb
            .header(
                reqwest::header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(body);
        crate::endpoint::send(p, rb).await?
    } else {
        // Remote images go as JSON edits.
        let refs: Vec<Value> = req
            .images
            .iter()
            .map(|m| Ok(json!({"image_url": data_url(m)?})))
            .collect::<Result<_, Error>>()?;
        fields.insert("images".into(), Value::Array(refs));
        merge(&mut fields, &overlay);
        post_json(p, client, "images/edits", &Value::Object(fields)).await?
    };
    let v = json_of(resp, what).await?;
    let format = v["output_format"].as_str().map(|f| format!("image/{f}"));
    let mut images = Vec::new();
    for item in v["data"].as_array().into_iter().flatten() {
        if let Some(b) = item["b64_json"].as_str() {
            images.push(Output::Bytes {
                mime: format.clone(),
                data: decode_b64(what, b)?,
            });
        } else if let Some(u) = item["url"].as_str() {
            images.push(Output::Url {
                url: u.to_string(),
                mime: format.clone(),
                authorized: false,
            });
        }
    }
    if images.is_empty() {
        return Err(Error::Decode(format!("{what} returned no images")));
    }
    Ok(ImageResponse {
        images,
        input_tokens: v["usage"]["input_tokens"].as_u64(),
        output_tokens: v["usage"]["output_tokens"].as_u64(),
    })
}

/// xAI renders `1k` or `2k`; other tiers use the nearest.
fn xai_resolution(r: &str) -> &'static str {
    let tier = r
        .trim()
        .to_ascii_lowercase()
        .trim_end_matches('k')
        .parse::<f64>()
        .unwrap_or(1.0);
    if tier < 1.5 {
        "1k"
    } else {
        "2k"
    }
}

async fn xai_image(
    p: &Provider,
    client: &reqwest::Client,
    req: &ImageRequest,
) -> Result<ImageResponse, Error> {
    let what = "xAI Images";
    let refs: Vec<Value> = req
        .images
        .iter()
        .map(|m| Ok(json!({"url": data_url(m)?, "type": "image_url"})))
        .collect::<Result<_, Error>>()?;
    let mut body = compact(Map::from_iter([
        ("model".to_string(), json!(req.model)),
        ("prompt".to_string(), json!(req.prompt)),
        (
            "image".to_string(),
            if refs.len() == 1 {
                refs[0].clone()
            } else {
                Value::Null
            },
        ),
        (
            "images".to_string(),
            if refs.len() > 1 {
                Value::Array(refs.clone())
            } else {
                Value::Null
            },
        ),
        ("n".to_string(), json!(req.n)),
        ("aspect_ratio".to_string(), json!(req.aspect)),
        (
            "resolution".to_string(),
            json!(req.resolution.as_deref().map(xai_resolution)),
        ),
        ("response_format".to_string(), json!("b64_json")),
    ]));
    let mut overlay = req.options.clone();
    if let Some(f) = overlay.remove("responseFormat") {
        overlay.insert("response_format".into(), f);
    }
    merge(&mut body, &overlay);
    let path = if refs.is_empty() {
        "images/generations"
    } else {
        "images/edits"
    };
    let v = json_of(
        post_json(p, client, path, &Value::Object(body)).await?,
        what,
    )
    .await?;
    let mut images = Vec::new();
    for item in v["data"].as_array().into_iter().flatten() {
        let mime = item["mime_type"].as_str().map(str::to_string);
        if let Some(b) = item["b64_json"].as_str() {
            images.push(Output::Bytes {
                mime,
                data: decode_b64(what, b)?,
            });
        } else if let Some(u) = item["url"].as_str() {
            images.push(Output::Url {
                url: u.to_string(),
                mime,
                authorized: false,
            });
        }
    }
    if images.is_empty() {
        return Err(Error::Decode(format!("{what} returned no images")));
    }
    Ok(ImageResponse {
        images,
        ..Default::default()
    })
}

fn google_part(m: &Media) -> Value {
    match &m.source {
        Source::Base64(d) => json!({"inlineData": {"mimeType": m.mime, "data": d}}),
        Source::Url(u) => json!({"fileData": {"mimeType": m.mime, "fileUri": u}}),
    }
}

async fn google_image(
    p: &Provider,
    client: &reqwest::Client,
    req: &ImageRequest,
) -> Result<ImageResponse, Error> {
    let what = "Google Images";
    if req.n.is_some_and(|n| n > 1) {
        return Err(Error::Invalid(format!(
            "{what} generates one image per request; call it once per image"
        )));
    }
    let mut parts = vec![json!({"text": req.prompt})];
    parts.extend(req.images.iter().map(google_part));
    let mut config = Map::new();
    config.insert("responseModalities".into(), json!(["IMAGE"]));
    let size = req.resolution.as_deref().map(|r| {
        let r = r.to_ascii_uppercase();
        if r == "0.5K" {
            "512".to_string()
        } else {
            r
        }
    });
    let image_config = compact(Map::from_iter([
        ("aspectRatio".to_string(), json!(req.aspect)),
        ("imageSize".to_string(), json!(size)),
    ]));
    if !image_config.is_empty() {
        config.insert("imageConfig".into(), Value::Object(image_config));
    }
    merge(&mut config, &req.options);
    let body = json!({
        "contents": [{"role": "user", "parts": parts}],
        "generationConfig": config,
    });
    let path = format!("models/{}:generateContent", req.model);
    let v = json_of(post_json(p, client, &path, &body).await?, what).await?;
    let mut images = Vec::new();
    let mut reasons = Vec::new();
    for c in v["candidates"].as_array().into_iter().flatten() {
        if let Some(r) = c["finishReason"].as_str() {
            reasons.push(r.to_string());
        }
        for part in c["content"]["parts"].as_array().into_iter().flatten() {
            // Thought parts are drafts; only other inline data is a final image.
            if part["thought"] == true {
                continue;
            }
            if let Some(d) = part["inlineData"]["data"].as_str() {
                images.push(Output::Bytes {
                    mime: part["inlineData"]["mimeType"].as_str().map(str::to_string),
                    data: decode_b64(what, d)?,
                });
            }
        }
    }
    if images.is_empty() {
        let filtered = reasons.iter().any(|r| {
            matches!(
                r.as_str(),
                "SAFETY"
                    | "RECITATION"
                    | "BLOCKLIST"
                    | "PROHIBITED_CONTENT"
                    | "SPII"
                    | "IMAGE_SAFETY"
            )
        });
        let detail = if reasons.is_empty() {
            String::new()
        } else {
            format!(" (finish reasons: {})", reasons.join(", "))
        };
        return Err(match filtered {
            true => Error::Provider {
                message: format!("{what} withheld its images{detail}: content_filter"),
                retryable: false,
            },
            false => Error::Decode(format!("{what} returned no final images{detail}")),
        });
    }
    let u = &v["usageMetadata"];
    let output = u["candidatesTokenCount"]
        .as_u64()
        .map(|c| c + u["thoughtsTokenCount"].as_u64().unwrap_or(0));
    Ok(ImageResponse {
        images,
        input_tokens: u["promptTokenCount"].as_u64(),
        output_tokens: output,
    })
}

/// One video request.
#[derive(Debug, Clone, Default)]
pub struct VideoRequest {
    pub model: String,
    pub prompt: String,
    /// The image the video starts on.
    pub start: Option<Media>,
    /// The image the video ends on.
    pub end: Option<Media>,
    /// Images, videos or audio that guide it.
    pub references: Vec<Media>,
    /// Seconds.
    pub duration: Option<u32>,
    /// `width:height` or `auto`.
    pub aspect: Option<String>,
    pub resolution: Option<String>,
    pub sound: Option<bool>,
    pub options: Map<String, Value>,
}

/// A video job's state at one read.
#[derive(Debug, Clone, PartialEq)]
pub enum VideoState {
    /// Still running; `progress` is 0..1 when the provider says.
    Running {
        progress: Option<f64>,
    },
    Completed,
    /// It failed: `failed`, `expired` or `cancelled`, with the provider's
    /// message.
    Failed {
        reason: String,
        message: String,
    },
}

/// Submit a video job; returns the provider's job id. Submitting is paid
/// and not idempotent: callers never retry it.
pub async fn video_start(
    p: &Provider,
    client: &reqwest::Client,
    api: VideoApi,
    req: &VideoRequest,
) -> Result<String, Error> {
    match api {
        VideoApi::XAI => {
            if req.references.iter().any(|m| m.mime.starts_with("video/")) {
                return Err(Error::Invalid(
                    "xAI Video has no reference video input".into(),
                ));
            }
            let url = |m: &Media| data_url(m).map(|u| json!({"url": u}));
            let images: Vec<Value> = req
                .references
                .iter()
                .filter(|m| !m.mime.starts_with("audio/"))
                .map(url)
                .collect::<Result<_, _>>()?;
            let audios: Vec<Value> = req
                .references
                .iter()
                .filter(|m| m.mime.starts_with("audio/"))
                .map(url)
                .collect::<Result<_, _>>()?;
            let aspect = match req.aspect.as_deref() {
                Some("auto") if req.start.is_some() => None,
                a => a,
            };
            let mut body = compact(Map::from_iter([
                ("model".to_string(), json!(req.model)),
                ("prompt".to_string(), json!(req.prompt)),
                (
                    "image".to_string(),
                    req.start
                        .as_ref()
                        .map(url)
                        .transpose()?
                        .unwrap_or(Value::Null),
                ),
                (
                    "last_frame".to_string(),
                    req.end
                        .as_ref()
                        .map(url)
                        .transpose()?
                        .unwrap_or(Value::Null),
                ),
                (
                    "reference_images".to_string(),
                    if images.is_empty() {
                        Value::Null
                    } else {
                        Value::Array(images)
                    },
                ),
                (
                    "reference_audios".to_string(),
                    if audios.is_empty() {
                        Value::Null
                    } else {
                        Value::Array(audios)
                    },
                ),
                ("duration".to_string(), json!(req.duration)),
                ("aspect_ratio".to_string(), json!(aspect)),
                ("resolution".to_string(), json!(req.resolution)),
                ("generate_audio".to_string(), json!(req.sound)),
            ]));
            let mut overlay = req.options.clone();
            let mode = overlay.remove("mode");
            merge(&mut body, &overlay);
            let path = match mode.as_ref().and_then(Value::as_str) {
                Some("edit") => "videos/edits",
                Some("extend") => "videos/extensions",
                _ => "videos/generations",
            };
            let v = json_of(
                post_json(p, client, path, &Value::Object(body)).await?,
                "xAI Video",
            )
            .await?;
            v["request_id"]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| Error::Decode("xAI Video returned no request_id".into()))
        }
        VideoApi::Google => {
            if req.end.is_some() && req.start.is_none() {
                return Err(Error::Invalid(
                    "Google Interactions reads a lone image as the start frame; send start too"
                        .into(),
                ));
            }
            let mut input: Vec<Value> = req
                .start
                .iter()
                .chain(req.end.iter())
                .chain(req.references.iter())
                .map(google_content)
                .collect();
            input.push(json!({"type": "text", "text": req.prompt}));
            let format = compact(Map::from_iter([
                (
                    "aspect_ratio".to_string(),
                    json!(req.aspect.as_deref().filter(|a| *a != "auto")),
                ),
                (
                    "resolution".to_string(),
                    json!(req.resolution.as_deref().map(str::to_lowercase)),
                ),
                (
                    "duration".to_string(),
                    json!(req.duration.map(|d| format!("{d}s"))),
                ),
            ]));
            let frames = req.start.is_some() || req.end.is_some();
            let task = match (frames, !req.references.is_empty()) {
                (true, false) => Some("image_to_video"),
                (false, true) => Some("reference_to_video"),
                _ => None,
            };
            let mut body = Map::new();
            body.insert("model".into(), json!(req.model));
            body.insert("input".into(), Value::Array(input));
            body.insert("background".into(), json!(true));
            if !format.is_empty() {
                let mut f = Map::new();
                f.insert("type".into(), json!("video"));
                f.extend(format);
                body.insert("response_format".into(), Value::Object(f));
            }
            if let Some(t) = task {
                body.insert(
                    "generation_config".into(),
                    json!({"video_config": {"task": t}}),
                );
            }
            merge(&mut body, &req.options);
            let v = json_of(
                post_json(p, client, "interactions", &Value::Object(body)).await?,
                "Google Interactions Video",
            )
            .await?;
            v["id"].as_str().map(str::to_string).ok_or_else(|| {
                Error::Decode("Google Interactions Video returned no interaction id".into())
            })
        }
    }
}

fn google_content(m: &Media) -> Value {
    let kind = m.mime.split('/').next().unwrap_or("");
    let kind = match kind {
        "image" | "audio" | "video" => kind,
        _ => "document",
    };
    match &m.source {
        Source::Base64(d) => json!({"type": kind, "data": d, "mime_type": m.mime}),
        Source::Url(u) => json!({"type": kind, "uri": u, "mime_type": m.mime}),
    }
}

fn job_path(api: VideoApi, id: &str) -> String {
    let id: String = url_escape(id);
    match api {
        VideoApi::XAI => format!("videos/{id}"),
        VideoApi::Google => format!("interactions/{id}"),
    }
}

fn url_escape(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

async fn read_job(
    p: &Provider,
    client: &reqwest::Client,
    api: VideoApi,
    id: &str,
) -> Result<Value, Error> {
    let rb = p.get(client, &job_path(api, id));
    json_of(crate::endpoint::send(p, rb).await?, "video status").await
}

fn state_of(api: VideoApi, v: &Value) -> VideoState {
    let status = v["status"].as_str().unwrap_or("");
    match (api, status) {
        (VideoApi::XAI, "done") | (VideoApi::Google, "completed" | "incomplete") => {
            VideoState::Completed
        }
        (VideoApi::XAI, "failed" | "expired") | (VideoApi::Google, "failed" | "cancelled") => {
            let message = match api {
                VideoApi::XAI => v["error"]["message"].as_str().unwrap_or("").to_string(),
                VideoApi::Google => v["errors"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e["message"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
            };
            VideoState::Failed {
                reason: status.to_string(),
                message,
            }
        }
        _ => VideoState::Running {
            progress: v["progress"]
                .as_f64()
                .filter(|p| (0.0..=100.0).contains(p))
                .map(|p| p / 100.0),
        },
    }
}

/// One status read of a job.
pub async fn video_status(
    p: &Provider,
    client: &reqwest::Client,
    api: VideoApi,
    id: &str,
) -> Result<VideoState, Error> {
    Ok(state_of(api, &read_job(p, client, api, id).await?))
}

/// The finished job's videos (at least one).
pub async fn video_result(
    p: &Provider,
    client: &reqwest::Client,
    api: VideoApi,
    id: &str,
) -> Result<Vec<Output>, Error> {
    let v = read_job(p, client, api, id).await?;
    match state_of(api, &v) {
        VideoState::Completed => {}
        VideoState::Running { .. } => {
            return Err(Error::Invalid(format!("video job {id} is still running")))
        }
        VideoState::Failed { reason, message } => {
            return Err(Error::Provider {
                message: format!(
                    "video generation {reason}{}",
                    if message.is_empty() {
                        String::new()
                    } else {
                        format!(": {message}")
                    }
                ),
                retryable: false,
            })
        }
    }
    let mut out = Vec::new();
    match api {
        VideoApi::XAI => {
            if let Some(u) = v["video"]["url"].as_str() {
                out.push(Output::Url {
                    url: u.to_string(),
                    mime: Some("video/mp4".into()),
                    authorized: false,
                });
            } else if v["video"]["respect_moderation"] == false {
                return Err(Error::Provider {
                    message: "xAI Video withheld the video for moderation: content_filter".into(),
                    retryable: false,
                });
            }
        }
        VideoApi::Google => {
            for step in v["steps"].as_array().into_iter().flatten() {
                if step["type"] != "model_output" {
                    continue;
                }
                for c in step["content"].as_array().into_iter().flatten() {
                    if c["type"] != "video" {
                        continue;
                    }
                    let mime = c["mime_type"].as_str().map(str::to_string);
                    if let Some(d) = c["data"].as_str() {
                        out.push(Output::Bytes {
                            mime,
                            data: decode_b64("Google Interactions Video", d)?,
                        });
                    } else if let Some(u) = c["uri"].as_str() {
                        out.push(Output::Url {
                            url: u.to_string(),
                            mime,
                            authorized: true,
                        });
                    }
                }
            }
        }
    }
    if out.is_empty() {
        return Err(Error::Decode(format!(
            "video job {id} completed without a video"
        )));
    }
    Ok(out)
}

/// Ask the provider to stop a job; fails when it has no way to.
pub async fn video_cancel(
    p: &Provider,
    client: &reqwest::Client,
    api: VideoApi,
    id: &str,
) -> Result<(), Error> {
    match api {
        VideoApi::XAI => Err(Error::Invalid("xAI Video cannot cancel a job".into())),
        VideoApi::Google => {
            let path = format!("{}/cancel", job_path(api, id));
            post_json(p, client, &path, &json!({})).await?;
            Ok(())
        }
    }
}

/// Download a generated file, up to `max` bytes; `authorized` sends the
/// provider's key (a Google Files API URI).
pub async fn download(
    p: &Provider,
    client: &reqwest::Client,
    url: &str,
    authorized: bool,
    max: usize,
) -> Result<Vec<u8>, Error> {
    let mut resp = if authorized {
        let mut rb = client.get(url);
        for (k, v) in p.auth_headers(p.headers.clone()) {
            rb = rb.header(k, v);
        }
        crate::endpoint::send(p, rb).await?
    } else {
        // A public URL never gets the provider's credentials.
        let resp = client.get(url).send().await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::Status {
                status,
                body: crate::redact_urls(&text.chars().take(2000).collect::<String>()),
            });
        }
        resp
    };
    if resp.content_length().is_some_and(|n| n as usize > max) {
        return Err(Error::Invalid(format!(
            "the generated file is larger than {} MiB",
            max / (1024 * 1024)
        )));
    }
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        out.extend_from_slice(&chunk);
        if out.len() > max {
            return Err(Error::Invalid(format!(
                "the generated file is larger than {} MiB",
                max / (1024 * 1024)
            )));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packages_name_their_apis() {
        assert_eq!(
            image_api("@opencode/ai/providers/openai/images"),
            Some(ImageApi::OpenAI)
        );
        assert_eq!(
            image_api("@opencode/ai/providers/openrouter/images"),
            Some(ImageApi::OpenAIReferences)
        );
        assert_eq!(
            image_api("@opencode/ai/providers/google/images"),
            Some(ImageApi::Google)
        );
        assert_eq!(image_api("@opencode/ai/providers/azure/images"), None);
        assert_eq!(
            video_api("@opencode/ai/providers/xai/videos"),
            Some(VideoApi::XAI)
        );
        assert_eq!(
            default_base_url("@opencode/ai/providers/xai/videos"),
            Some("https://api.x.ai/v1")
        );
    }

    #[test]
    fn openai_sizes_follow_the_aspect() {
        assert_eq!(
            openai_size("gpt-image-1", Some("16:9")).as_deref(),
            Some("1536x1024")
        );
        assert_eq!(
            openai_size("dall-e-3", Some("9:16")).as_deref(),
            Some("1024x1792")
        );
        assert_eq!(openai_size("gpt-image-1", None), None);
    }

    #[test]
    fn xai_resolutions_round_to_its_tiers() {
        assert_eq!(xai_resolution("1K"), "1k");
        assert_eq!(xai_resolution("4k"), "2k");
        assert_eq!(xai_resolution("0.5k"), "1k");
    }
}
