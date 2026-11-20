//! One-shot generation outside a session's history (after OpenCode's
//! `Generate`): text from a chat model, images from an image model, and the
//! image and video models a configuration can reach.
//!
//! Image and video models come from the models.dev catalog (`type: image`
//! or `video`) through the package of their type (`…/images`, `…/videos`),
//! for providers CodeSmith has a key for. A one-shot call is retried only
//! when the provider never ran it (see [`crate::retry::Policy::one_shot`]):
//! these calls can be billed per attempt.

use crate::config::Config;
use codesmith_ai::decision::{self, DecisionApi};
use codesmith_ai::generation::{self, ImageApi, Output, VideoApi};
use codesmith_ai::{Media, Message, Provider};
use serde_json::{Map, Value};
use std::time::Duration;

/// The most bytes one generated item may have (the media cap).
pub const MAX_BYTES: usize = crate::media::MAX_INGEST_BYTES as usize;

/// An image or video model a configuration can reach.
#[derive(Debug, Clone)]
pub struct MediaModel {
    pub provider_id: String,
    /// The catalog id.
    pub id: String,
    /// The id sent to the provider.
    pub model_id: String,
    pub name: String,
    /// `image` or `video`.
    pub kind: String,
    pub package: String,
    /// What it takes (`text`, `image`, `video`, …).
    pub input: Vec<String>,
    /// Its variants, by id, with their request fields.
    pub variants: Vec<(String, Map<String, Value>)>,
    /// Release time (ms), for "newest first".
    pub released: i64,
    pub provider: Provider,
    /// Where a decision host reads media (catalog `media_input`, or the
    /// model config's `compatibility.mediaInput`).
    pub media_input: Option<String>,
}

impl MediaModel {
    /// `provider/model`.
    pub fn spec(&self) -> String {
        format!("{}/{}", self.provider_id, self.id)
    }

    /// Whether it takes input images.
    pub fn accepts_images(&self) -> bool {
        self.input.iter().any(|i| i.starts_with("image"))
    }

    pub fn image_api(&self) -> Option<ImageApi> {
        generation::image_api(&self.package)
    }

    pub fn video_api(&self) -> Option<VideoApi> {
        generation::video_api(&self.package)
    }

    pub fn decision_api(&self) -> Option<DecisionApi> {
        decision::decision_api(&self.package)
    }
}

/// The image (`kind` `image`), video (`video`) or decision (`decision`)
/// models with a supported API and a key, newest first. A provider the
/// config gives a base URL but no key (a local host like LM Studio) is
/// called without auth, as OpenCode does.
pub fn models(config: &Config, kind: &str) -> Vec<MediaModel> {
    let Some(catalog) = config.models_dev.as_ref().map(|c| c.catalog()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    // Only providers with a key are read: reading a provider normalizes
    // its part of the (multi-megabyte) catalog.
    let saved = crate::auth::providers_in(&crate::paths::auth_file());
    let keyed = |id: &str| {
        let user = config.providers.get(id);
        user.and_then(|u| u.settings.api_key.as_deref())
            .is_some_and(|k| !k.is_empty())
            || user.is_some_and(|u| u.env.iter().any(|v| std::env::var_os(v).is_some()))
            || catalog
                .environment(id)
                .iter()
                .any(|v| std::env::var_os(v).is_some_and(|k| !k.is_empty()))
            || saved.iter().any(|p| p == id)
            || keyless(config, id)
    };
    let ids: Vec<String> = catalog
        .ids()
        .filter(|id| keyed(id))
        .map(str::to_string)
        .collect();
    for snap in ids.iter().filter_map(|id| catalog.provider(id)) {
        let pid = snap.info.id.as_str();
        let supported = |m: &crate::catalog::Model| {
            let package = m.package.as_deref().unwrap_or(&snap.info.package);
            m.kind == kind
                && m.enabled
                && match kind {
                    "image" => generation::image_api(package).is_some(),
                    "video" => generation::video_api(package).is_some(),
                    "decision" => decision::decision_api(package).is_some(),
                    _ => false,
                }
        };
        if !snap.models.iter().any(supported) {
            continue;
        }
        // The key is looked up once per provider, and only for one with
        // models of this kind.
        let Some(p) = config.provider(pid) else {
            continue;
        };
        let key = config.provider_key(pid, &p);
        if key.is_none() && !keyless(config, pid) {
            continue;
        }
        for m in snap.models.iter().filter(|m| supported(m)) {
            let package = m
                .package
                .clone()
                .unwrap_or_else(|| snap.info.package.clone());
            let Some(provider) =
                media_provider(config, pid, &p, key.as_deref(), m, &snap.info, &package)
            else {
                continue;
            };
            let mut variants: Vec<(String, Map<String, Value>)> = m
                .variants
                .iter()
                .map(|v| {
                    let mut fields = v.settings.clone().unwrap_or_default();
                    fields.extend(v.body.clone().unwrap_or_default());
                    (v.id.clone(), fields)
                })
                .collect();
            if let Some(mc) = config.providers.get(pid).and_then(|p| p.models.get(&m.id)) {
                for (name, v) in mc.variants() {
                    let mut fields = v.options.as_map().clone();
                    fields.extend(v.body);
                    match variants.iter_mut().find(|(n, _)| *n == name) {
                        Some(slot) => slot.1.extend(fields),
                        None => variants.push((name, fields)),
                    }
                }
            }
            let media_input = config
                .providers
                .get(pid)
                .and_then(|p| p.models.get(&m.id))
                .and_then(|mc| mc.compatibility.get("mediaInput"))
                .and_then(|v| v.as_str().map(str::to_string))
                .or_else(|| m.compatibility.as_ref()?.media_input.clone());
            out.push(MediaModel {
                provider_id: pid.to_string(),
                id: m.id.clone(),
                model_id: m.model_id.clone(),
                name: m.name.clone(),
                kind: kind.to_string(),
                package,
                input: m.capabilities.input.clone(),
                variants,
                released: m.time.released,
                provider,
                media_input,
            });
        }
    }
    out.sort_by(|a, b| b.released.cmp(&a.released).then(a.spec().cmp(&b.spec())));
    out
}

/// The provider (URL, key, headers) a media model is called through: the
/// configured URL, else the catalog model's, else the provider's, else the
/// API's default; `None` without a key.
fn media_provider(
    config: &Config,
    pid: &str,
    p: &crate::config::ProviderConfig,
    key: Option<&str>,
    m: &crate::catalog::Model,
    info: &crate::catalog::ProviderInfo,
    package: &str,
) -> Option<Provider> {
    let configured = config
        .providers
        .get(pid)
        .and_then(|u| u.settings.base_url.clone())
        .filter(|u| !u.is_empty());
    let base_url = configured
        .or_else(|| m.settings.as_ref()?.base_url.clone())
        .or_else(|| info.settings.as_ref()?.base_url.clone())
        .or_else(|| generation::default_base_url(package).map(str::to_string))
        .filter(|u| !u.contains("${"))?;
    let protocol = if package.contains("/google/") {
        codesmith_ai::Protocol::GoogleGemini
    } else {
        codesmith_ai::Protocol::OpenaiChat
    };
    let auth = match p.settings.api_key_header.clone().filter(|h| !h.is_empty()) {
        Some(h) => codesmith_ai::Auth::Header(h),
        None => codesmith_ai::Auth::Default,
    };
    Some(Provider {
        protocol,
        name: pid.to_string(),
        base_url,
        api_key: key.map(str::to_string),
        headers: p.headers.clone().into_iter().collect(),
        query: p.settings.query_params.clone().into_iter().collect(),
        auth,
        encoding: codesmith_ai::Encoding::None,
        compat: Default::default(),
        hooks: None,
    })
}

/// Whether the config points `pid` at a host of its own without a key
/// (OpenCode's keyless rule: an enabled provider with no credential).
fn keyless(config: &Config, pid: &str) -> bool {
    config.providers.get(pid).is_some_and(|u| {
        u.settings
            .base_url
            .as_deref()
            .is_some_and(|b| !b.is_empty())
            && u.settings.api_key.as_deref().is_none_or(str::is_empty)
    })
}

/// `provider/model` or `provider/model#variant`, split.
pub fn parse_ref(spec: &str) -> Option<(&str, &str, Option<&str>)> {
    let (base, variant) = crate::config::split_variant(spec);
    let (p, m) = base.split_once('/')?;
    (!p.is_empty() && !m.is_empty() && !spec.ends_with('#')).then_some((p, m, variant))
}

/// Why a generation failed (OpenCode's `Generate.Error`).
#[derive(Debug, Clone, PartialEq)]
pub enum GenerateError {
    ModelNotFound {
        provider: String,
        model: String,
    },
    /// The model exists but cannot serve this request (a chat model asked
    /// for images).
    UnsupportedModel {
        provider: String,
        model: String,
        capability: String,
        message: String,
    },
    InvalidRequest {
        message: String,
    },
    /// The provider rejected or failed the request after it was sent.
    ProviderRequest {
        provider: String,
        reason: Failure,
        message: String,
        status: Option<u16>,
    },
    Unavailable {
        message: String,
    },
}

/// How a provider failed a generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    Authentication,
    RateLimit,
    ContentFilter,
    InvalidRequest,
    Overloaded,
    Provider,
}

impl std::fmt::Display for GenerateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GenerateError::ModelNotFound { provider, model } => {
                write!(f, "Model not found: {provider}/{model}")
            }
            GenerateError::UnsupportedModel { message, .. }
            | GenerateError::InvalidRequest { message }
            | GenerateError::ProviderRequest { message, .. }
            | GenerateError::Unavailable { message } => f.write_str(message),
        }
    }
}

impl std::error::Error for GenerateError {}

/// A provider error as a [`GenerateError`].
pub fn provider_error(provider: &str, e: &codesmith_ai::Error) -> GenerateError {
    use crate::retry::Class;
    if let codesmith_ai::Error::Invalid(m) = e {
        return GenerateError::InvalidRequest { message: m.clone() };
    }
    let status = match e {
        codesmith_ai::Error::Status { status, .. } => Some(*status),
        _ => None,
    };
    let reason = match crate::retry::classify(e) {
        Class::Authentication => Failure::Authentication,
        Class::RateLimit | Class::Quota => Failure::RateLimit,
        Class::ContentPolicy => Failure::ContentFilter,
        // The provider refused the request it got: a provider failure with
        // its status, as OpenCode reports it (a request CodeSmith refuses
        // before sending is `InvalidRequest`, above).
        Class::InvalidRequest | Class::ContextOverflow | Class::PayloadTooLarge => {
            Failure::InvalidRequest
        }
        Class::ProviderInternal if matches!(status, Some(503 | 529)) => Failure::Overloaded,
        _ if e.to_string().contains("content_filter") => Failure::ContentFilter,
        _ => Failure::Provider,
    };
    GenerateError::ProviderRequest {
        provider: provider.to_string(),
        reason,
        message: message_of(e),
        status,
    }
}

/// The provider's own message when its error body names one.
fn message_of(e: &codesmith_ai::Error) -> String {
    if let codesmith_ai::Error::Status { body, .. } = e {
        if let Ok(v) = serde_json::from_str::<Value>(body) {
            if let Some(m) = v["error"]["message"]
                .as_str()
                .or(v["message"].as_str())
                .or(v["error"].as_str())
            {
                return m.to_string();
            }
        }
    }
    e.to_string()
}

/// How one-shot calls wait between attempts.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Multiplies every retry wait (tests shrink it).
    pub retry_scale: f64,
    /// How often a waiting video job is checked.
    pub poll: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            retry_scale: 1.0,
            poll: crate::video::INTERVAL,
        }
    }
}

/// Run `call` under the one-shot retry rules: only a failure the provider
/// never ran (a refused connection, a throttle, an overloaded server) is
/// tried again, at most three times.
pub async fn retry_one_shot<T, F, Fut>(opts: Options, mut call: F) -> Result<T, codesmith_ai::Error>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, codesmith_ai::Error>>,
{
    let mut policy = crate::retry::Policy::one_shot();
    loop {
        match call().await {
            Ok(v) => return Ok(v),
            Err(e) => match policy.next(&e, std::time::Instant::now()) {
                Some(wait) => tokio::time::sleep(wait.mul_f64(opts.retry_scale)).await,
                None => return Err(e),
            },
        }
    }
}

/// The models of `kind` a ref may name, to tell "not this type" from
/// "no such model".
fn missing(config: &Config, provider: &str, model: &str, kind: &str) -> GenerateError {
    let catalog = config.models_dev.as_ref().map(|c| c.catalog());
    let other = catalog
        .as_ref()
        .and_then(|c| c.provider(provider))
        .and_then(|p| {
            p.models
                .iter()
                .find(|m| m.id == model)
                .map(|m| m.kind.clone())
        })
        .or_else(|| {
            config
                .providers
                .get(provider)
                .and_then(|p| p.models.get(model))
                .map(|_| "chat".to_string())
        });
    match other {
        Some(t) if t != kind => GenerateError::UnsupportedModel {
            provider: provider.into(),
            model: model.into(),
            capability: kind.into(),
            message: format!("{provider}/{model} is a {t} model, not a {kind} model"),
        },
        Some(_) => GenerateError::UnsupportedModel {
            provider: provider.into(),
            model: model.into(),
            capability: kind.into(),
            message: format!("{provider}/{model} has no {kind} API CodeSmith can call, or no key"),
        },
        None => GenerateError::ModelNotFound {
            provider: provider.into(),
            model: model.into(),
        },
    }
}

/// The media model `spec` names, with its variant's fields.
pub fn resolve(
    config: &Config,
    kind: &str,
    spec: &str,
) -> Result<(MediaModel, Map<String, Value>), GenerateError> {
    let (pid, mid, variant) = parse_ref(spec).ok_or_else(|| GenerateError::InvalidRequest {
        message: format!("Invalid model \"{spec}\". Use \"providerID/modelID\"."),
    })?;
    let model = models(config, kind)
        .into_iter()
        .find(|m| m.provider_id == pid && m.id == mid)
        .ok_or_else(|| missing(config, pid, mid, kind))?;
    let fields = match variant {
        None => Map::new(),
        Some(v) => model
            .variants
            .iter()
            .find(|(n, _)| n == v)
            .map(|(_, f)| f.clone())
            .ok_or_else(|| GenerateError::InvalidRequest {
                message: format!("Variant \"{v}\" is not available for \"{pid}/{mid}\"."),
            })?,
    };
    Ok((model, fields))
}

/// An image request: the model as `provider/model[#variant]`.
#[derive(Debug, Clone, Default)]
pub struct ImageInput {
    pub model: String,
    pub prompt: String,
    pub images: Vec<Media>,
    pub n: Option<u32>,
    pub aspect: Option<String>,
    pub resolution: Option<String>,
}

/// A generated image.
#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    pub mime: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImageResult {
    pub images: Vec<Image>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

/// The bytes of a generated output: downloaded when it is a URL (at most
/// [`MAX_BYTES`]), with its type read from the bytes when they tell.
pub async fn materialize(
    p: &Provider,
    out: Output,
) -> Result<(String, Vec<u8>), codesmith_ai::Error> {
    let (mime, data) = match out {
        Output::Bytes { mime, data } => (mime, data),
        Output::Url {
            url,
            mime,
            authorized,
        } => {
            let data = if let Some(m) = Media::from_data_url(&url) {
                let codesmith_ai::Source::Base64(d) = m.source else {
                    unreachable!()
                };
                crate::media::unbase64(&d).ok_or_else(|| {
                    codesmith_ai::Error::Decode("a generated data URL is not valid base64".into())
                })?
            } else {
                {
                    // The URL is the provider's choice: one that is not
                    // itself local may not point downloads at this machine
                    // or its network.
                    let public_only = !crate::config::is_local_url(&p.base_url);
                    generation::download(p, &url, authorized, public_only, MAX_BYTES).await?
                }
            };
            (mime, data)
        }
    };
    if data.len() > MAX_BYTES {
        return Err(codesmith_ai::Error::Invalid(format!(
            "the generated file is larger than {} MiB",
            MAX_BYTES / (1024 * 1024)
        )));
    }
    let detected = crate::media::detect(&data[..data.len().min(crate::media::SNIFF_BYTES)]);
    let mime = detected
        .map(str::to_string)
        .or(mime)
        .unwrap_or_else(|| "application/octet-stream".into());
    Ok((mime, data))
}

/// Generate images (OpenCode's `Generate.image`): URL results are
/// downloaded, so every image comes back as bytes.
pub async fn image(
    config: &Config,
    client: &reqwest::Client,
    input: &ImageInput,
    opts: Options,
) -> Result<ImageResult, GenerateError> {
    let (model, fields) = resolve(config, "image", &input.model)?;
    let api = model
        .image_api()
        .ok_or_else(|| missing(config, &model.provider_id, &model.id, "image"))?;
    let req = generation::ImageRequest {
        model: model.model_id.clone(),
        prompt: input.prompt.clone(),
        images: input.images.clone(),
        n: input.n,
        aspect: input.aspect.clone(),
        resolution: input.resolution.clone(),
        options: fields,
    };
    let p = &model.provider;
    let response = retry_one_shot(opts, || generation::generate_image(p, client, api, &req))
        .await
        .map_err(|e| provider_error(&model.provider_id, &e))?;
    let mut images = Vec::new();
    for out in response.images {
        let (mime, data) = materialize(p, out)
            .await
            .map_err(|e| provider_error(&model.provider_id, &e))?;
        images.push(Image { mime, data });
    }
    Ok(ImageResult {
        images,
        input_tokens: response.input_tokens,
        output_tokens: response.output_tokens,
    })
}

/// Text from a chat model (`model`, else the configured default), with no
/// session stored (OpenCode's `Generate.text`).
pub async fn text(
    config: &Config,
    client: &reqwest::Client,
    model: Option<&str>,
    prompt: &str,
    opts: Options,
) -> Result<(String, codesmith_ai::Usage), GenerateError> {
    let spec = model
        .map(str::to_string)
        .or_else(|| config.model.clone())
        .ok_or_else(|| GenerateError::Unavailable {
            message: "No model specified and no supported model is available".into(),
        })?;
    if let Some((pid, mid, _)) = parse_ref(&spec) {
        let kind = config.models_dev.as_ref().and_then(|c| {
            c.catalog()
                .provider(pid)?
                .models
                .iter()
                .find(|x| x.id == mid)
                .map(|x| x.kind.clone())
        });
        if kind.as_deref().is_some_and(|k| k != "chat") {
            return Err(missing(config, pid, mid, "chat"));
        }
        // A model the catalog's provider does not list, and the config does
        // not add, is unknown (a live-listed local provider has no catalog
        // entry, so it is left to resolve).
        let listed = config
            .models_dev
            .as_ref()
            .is_some_and(|c| c.catalog().provider(pid).is_some());
        let configured = config
            .providers
            .get(pid)
            .is_some_and(|p| p.models.contains_key(mid));
        if listed && kind.is_none() && !configured {
            return Err(GenerateError::ModelNotFound {
                provider: pid.into(),
                model: mid.into(),
            });
        }
    }
    let m = config.resolve_model(&spec).map_err(|e| {
        let (pid, mid, _) = parse_ref(&spec).unwrap_or(("", "", None));
        if config.provider(pid).is_none() {
            GenerateError::ModelNotFound {
                provider: pid.into(),
                model: mid.into(),
            }
        } else {
            GenerateError::InvalidRequest {
                message: format!("{e:#}"),
            }
        }
    })?;
    // Gateways want a session for attribution even on a stateless call;
    // a fresh id is sent each time and nothing is stored.
    let session = crate::session::new_id();
    let req = codesmith_ai::Request {
        model: m.model_id.clone(),
        messages: vec![Message::user(prompt)],
        max_tokens: m.max_output,
        temperature: m.temperature,
        options: m.options.clone(),
        body: m.body.clone(),
        cache: m.cache.clone(),
        prompt_cache_key: Some(session),
        interaction: codesmith_ai::Interaction::Background,
        ..Default::default()
    };
    let response = retry_one_shot(opts, || async {
        let stream = m.provider.stream(client, &req).await?;
        codesmith_ai::assemble(stream, |_| {})
            .await
            .map_err(|f| f.error)
    })
    .await
    .map_err(|e| provider_error(&m.provider_id, &e))?;
    let text = response
        .parts
        .iter()
        .filter_map(|p| match p {
            codesmith_ai::Part::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<String>();
    Ok((text, response.usage))
}

/// A decision request (OpenCode's `Generate.decisions`): the model as
/// `provider/model[#variant]`.
#[derive(Debug, Clone, Default)]
pub struct DecisionInput {
    pub model: String,
    pub input: Vec<decision::Part>,
    pub questions: Vec<decision::Question>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DecisionResult {
    /// One per question, in question order.
    pub answers: Vec<decision::Answer>,
    /// USD the provider reported.
    pub cost: Option<f64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

/// Ask a decision model `input`'s questions.
pub async fn decisions(
    config: &Config,
    client: &reqwest::Client,
    input: &DecisionInput,
    opts: Options,
) -> Result<DecisionResult, GenerateError> {
    let (model, fields) = resolve(config, "decision", &input.model)?;
    let api = model
        .decision_api()
        .ok_or_else(|| missing(config, &model.provider_id, &model.id, "decision"))?;
    let req = decision::DecisionRequest {
        model: model.model_id.clone(),
        input: input.input.clone(),
        questions: input.questions.clone(),
        options: fields,
    };
    let media = model
        .media_input
        .as_deref()
        .and_then(decision::MediaInput::parse);
    let p = &model.provider;
    let response = retry_one_shot(opts, || {
        decision::decide(p, client, api, &model.package, media, &req)
    })
    .await
    .map_err(|e| provider_error(&model.provider_id, &e))?;
    Ok(DecisionResult {
        answers: response.answers,
        cost: response.cost,
        input_tokens: response.input_tokens,
        output_tokens: response.output_tokens,
    })
}

/// How long an image may take before it is given up.
pub const IMAGE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
