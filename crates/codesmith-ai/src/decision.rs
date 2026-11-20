//! Decision (classifier) models: a request asks named questions about some
//! input and gets one probability-backed answer per question (after
//! OpenCode's `Decision` with the `system-one` and `google-decisions`
//! protocols).
//!
//! Three kinds of question: a `predicate` (how likely a statement is true),
//! a `choice` among values and a `score` on ordered levels. System One hosts
//! (TypeSafe, OpenRouter, OpenCode Zen, llama.cpp-compatible servers) take
//! `POST {base}/systemone`; Google takes `POST {base}/decisions`. Where media
//! goes in a System One body depends on the host ([`MediaInput`]).

use crate::generation::{json_of, post_json};
use crate::{Error, Media, Provider, Source};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// One piece of a decision's input.
#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Text(String),
    Json(Value),
    Media(Media),
}

/// A choice's value: a string or a boolean (`true` and `"true"` differ).
pub type ChoiceValue = Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Choice {
    pub value: ChoiceValue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Level {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// What counts as true and false for a predicate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Criteria {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#true: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#false: Option<String>,
}

/// A question, by `type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Question {
    Predicate {
        name: String,
        instructions: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        criteria: Option<Criteria>,
    },
    Choice {
        name: String,
        instructions: String,
        choices: Vec<Choice>,
    },
    Score {
        name: String,
        instructions: String,
        levels: Vec<Level>,
    },
}

impl Question {
    pub fn name(&self) -> &str {
        match self {
            Question::Predicate { name, .. }
            | Question::Choice { name, .. }
            | Question::Score { name, .. } => name,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChoiceProbability {
    pub value: ChoiceValue,
    pub probability: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LevelProbability {
    pub value: i64,
    pub label: String,
    pub probability: f64,
}

/// One question's answer, in question order.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Answer {
    Predicate {
        name: String,
        probability: f64,
    },
    Choice {
        name: String,
        choice: ChoiceValue,
        probabilities: Vec<ChoiceProbability>,
        confidence: f64,
    },
    /// `score` is the probability-weighted level index.
    Score {
        name: String,
        score: f64,
        probabilities: Vec<LevelProbability>,
        confidence: f64,
    },
    /// This question has no answer (the host declined it); the others
    /// still came back.
    Error {
        name: String,
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
}

impl Answer {
    pub fn name(&self) -> &str {
        match self {
            Answer::Predicate { name, .. }
            | Answer::Choice { name, .. }
            | Answer::Score { name, .. }
            | Answer::Error { name, .. } => name,
        }
    }
}

/// Where a System One host reads media (models.dev
/// `compatibility.media_input`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaInput {
    /// TypeSafe's `assets` map, referenced from the state as `<image-N>`.
    Assets,
    /// Chat Completions content parts inside a state list.
    State,
    /// One top-level list per kind: `images`, `audio`, `videos`.
    Root,
    /// Parts in a user message inside `state.messages` (Baseten).
    StateMessages,
}

impl MediaInput {
    /// A catalog or config `media_input` value.
    pub fn parse(s: &str) -> Option<MediaInput> {
        match s {
            "assets" => Some(MediaInput::Assets),
            "state" => Some(MediaInput::State),
            "root" => Some(MediaInput::Root),
            _ => None,
        }
    }
}

/// How a host answers decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionApi {
    /// System One, with where this host places media by default.
    SystemOne(MediaInput),
    /// Google's `/decisions`.
    Google,
}

/// The decision API of a catalog package
/// (`@opencode/ai/providers/<p>/decisions`).
pub fn decision_api(package: &str) -> Option<DecisionApi> {
    let p = package
        .trim_start_matches("@opencode/ai/providers/")
        .trim_end_matches('/');
    Some(match p {
        "typesafe-ai/decisions" | "opencode/decisions" | "digitalocean/decisions" => {
            DecisionApi::SystemOne(MediaInput::Assets)
        }
        "openrouter/decisions" => DecisionApi::SystemOne(MediaInput::State),
        "openai-compatible/decisions" => DecisionApi::SystemOne(MediaInput::Root),
        "baseten/decisions" => DecisionApi::SystemOne(MediaInput::StateMessages),
        "google/decisions" => DecisionApi::Google,
        _ => return None,
    })
}

/// Hosts serving TypeSafe's models want this header to take images.
fn wants_image_header(package: &str) -> bool {
    ["typesafe-ai/", "opencode/", "digitalocean/"]
        .iter()
        .any(|p| package.contains(p))
}

/// One decision request.
#[derive(Debug, Clone, Default)]
pub struct DecisionRequest {
    pub model: String,
    pub input: Vec<Part>,
    pub questions: Vec<Question>,
    /// Fields merged into the body (a variant's or the provider's options).
    pub options: Map<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DecisionResponse {
    pub answers: Vec<Answer>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// USD the host says it charged.
    pub cost: Option<f64>,
}

/// Only what makes a request meaningless is checked here.
pub fn validate(questions: &[Question]) -> Result<(), Error> {
    if questions.is_empty() {
        return Err(Error::Invalid(
            "Decision questions must not be empty".into(),
        ));
    }
    let mut names: Vec<&str> = questions.iter().map(Question::name).collect();
    names.sort_unstable();
    if names.windows(2).any(|w| w[0] == w[1]) {
        return Err(Error::Invalid(
            "Decision question names must be unique".into(),
        ));
    }
    Ok(())
}

/// Ask `req`'s questions. `package` names the host's quirks; `media`
/// overrides where a System One host takes media.
pub async fn decide(
    p: &Provider,
    client: &reqwest::Client,
    api: DecisionApi,
    package: &str,
    media: Option<MediaInput>,
    req: &DecisionRequest,
) -> Result<DecisionResponse, Error> {
    validate(&req.questions)?;
    let (path, body, what) = match api {
        DecisionApi::SystemOne(fallback) => {
            let mut body = req.options.clone();
            body.extend(place_media(&req.input, media.unwrap_or(fallback))?);
            body.insert("questions".into(), encode_questions(&req.questions)?);
            body.insert("model".into(), json!(req.model));
            ("systemone", body, "System One")
        }
        DecisionApi::Google => ("decisions", google_body(req)?, "Google Decisions"),
    };
    let mut headers = p.headers.clone();
    if wants_image_header(package)
        && !headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("typesafe-danger-zone-images"))
    {
        headers.push(("TypeSafe-Danger-Zone-Images".into(), "true".into()));
    }
    let resp = if headers.len() == p.headers.len() {
        post_json(p, client, path, &Value::Object(body)).await?
    } else {
        let p = Provider {
            headers,
            ..p.clone()
        };
        post_json(&p, client, path, &Value::Object(body)).await?
    };
    let v = json_of(resp, what).await?;
    let answers = v["answers"]
        .as_object()
        .ok_or_else(|| Error::Decode(format!("{what} returned no answers")))?;
    let names: Vec<&str> = req.questions.iter().map(Question::name).collect();
    if answers.len() != names.len() || !names.iter().all(|n| answers.contains_key(*n)) {
        return Err(Error::Decode(format!(
            "{what} answers do not match the requested questions"
        )));
    }
    let answers = req
        .questions
        .iter()
        .map(|q| to_answer(q, &answers[q.name()], what))
        .collect::<Result<Vec<_>, _>>()?;
    let usage = &v["usage"];
    let (input_tokens, output_tokens, cost) = match api {
        DecisionApi::SystemOne(_) => (
            usage["input_tokens"].as_u64(),
            usage["output_tokens"].as_u64(),
            usage["cost"].as_f64(),
        ),
        DecisionApi::Google => (
            usage["total_input_tokens"].as_u64(),
            usage["total_output_tokens"].as_u64(),
            None,
        ),
    };
    Ok(DecisionResponse {
        answers,
        input_tokens,
        output_tokens,
        cost,
    })
}

/// Questions as System One's map, keyed by name.
fn encode_questions(questions: &[Question]) -> Result<Value, Error> {
    let mut out = Map::new();
    for q in questions {
        let v = match q {
            Question::Predicate {
                instructions,
                criteria,
                ..
            } => {
                let mut v = json!({"type": "noul", "instructions": instructions});
                if let Some(c) = criteria {
                    v["criteria"] = serde_json::to_value(c).unwrap_or_default();
                }
                v
            }
            Question::Score {
                instructions,
                levels,
                ..
            } => json!({
                "type": "score",
                "instructions": instructions,
                "criteria": levels.iter().map(|l| match &l.description {
                    None => l.label.clone(),
                    Some(d) => format!("{}: {d}", l.label),
                }).collect::<Vec<_>>(),
            }),
            Question::Choice {
                name,
                instructions,
                choices,
            } => {
                // Options are map keys, so `true` is sent as "true".
                let mut criteria = Map::new();
                for c in choices {
                    let key = choice_key(&c.value);
                    if criteria.contains_key(&key) {
                        return Err(Error::Invalid(format!(
                            "Decision choice options are sent as string keys; \"{name}\" has a boolean and a string with the same text"
                        )));
                    }
                    criteria.insert(key, json!(c.description));
                }
                json!({"type": "choice", "instructions": instructions, "criteria": criteria})
            }
        };
        out.insert(q.name().to_string(), v);
    }
    Ok(Value::Object(out))
}

fn choice_key(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn plain(part: &Part) -> Value {
    match part {
        Part::Text(t) => json!(t),
        Part::Json(v) => v.clone(),
        Part::Media(_) => Value::Null,
    }
}

/// A lone text or JSON part is the state itself; several are a list.
fn state(mut items: Vec<Value>) -> Value {
    if items.len() == 1 {
        items.remove(0)
    } else {
        Value::Array(items)
    }
}

fn kind(m: &Media) -> &str {
    m.mime.split('/').next().unwrap_or("")
}

fn inline(m: &Media) -> Result<&str, Error> {
    match &m.source {
        Source::Base64(d) => Ok(d),
        Source::Url(_) => Err(Error::Invalid(
            "System One takes inline media; materialize URL media first".into(),
        )),
    }
}

fn data_url(m: &Media) -> Result<String, Error> {
    Ok(format!("data:{};base64,{}", m.mime, inline(m)?))
}

fn unsupported(m: &Media, accepted: &str) -> Error {
    Error::Invalid(format!("System One takes {accepted} here; got {}", m.mime))
}

/// A media part as a Chat Completions content part.
fn state_item(part: &Part, files: bool) -> Result<Value, Error> {
    let Part::Media(m) = part else {
        return Ok(plain(part));
    };
    let k = kind(m);
    if !files && !["image", "audio", "video"].contains(&k) {
        return Err(unsupported(m, "images, audio and video"));
    }
    Ok(match k {
        "image" => json!({"type": "image_url", "image_url": {"url": data_url(m)?}}),
        "video" => json!({"type": "video_url", "video_url": {"url": data_url(m)?}}),
        "audio" => {
            let format = m.mime["audio/".len()..].to_ascii_lowercase();
            let format = match format.as_str() {
                "x-wav" | "wave" | "vnd.wave" => "wav".to_string(),
                "x-m4a" => "m4a".into(),
                "x-aiff" => "aiff".into(),
                "x-flac" => "flac".into(),
                _ => format,
            };
            json!({"type": "input_audio", "input_audio": {"data": inline(m)?, "format": format}})
        }
        _ => {
            let mut file = json!({"file_data": data_url(m)?});
            if let Some(f) = &m.filename {
                file["filename"] = json!(f);
            }
            json!({"type": "file", "file": file})
        }
    })
}

/// The body's `state` (and media fields) for `mode`.
fn place_media(parts: &[Part], mode: MediaInput) -> Result<Map<String, Value>, Error> {
    let media: Vec<&Media> = parts
        .iter()
        .filter_map(|p| match p {
            Part::Media(m) => Some(m),
            _ => None,
        })
        .collect();
    let mut out = Map::new();
    if media.is_empty() {
        out.insert("state".into(), state(parts.iter().map(plain).collect()));
        return Ok(out);
    }
    match mode {
        MediaInput::State => {
            let items = parts
                .iter()
                .map(|p| state_item(p, true))
                .collect::<Result<Vec<_>, _>>()?;
            out.insert("state".into(), Value::Array(items));
        }
        MediaInput::StateMessages => {
            let content = parts
                .iter()
                .map(|p| match p {
                    Part::Media(_) => state_item(p, false),
                    Part::Text(t) => Ok(json!({"type": "text", "text": t})),
                    Part::Json(v) => Ok(json!({"type": "text", "text": v.to_string()})),
                })
                .collect::<Result<Vec<_>, _>>()?;
            out.insert(
                "state".into(),
                json!({"messages": [{"role": "user", "content": content}]}),
            );
        }
        MediaInput::Root => {
            let lists = [("image", "images"), ("audio", "audio"), ("video", "videos")];
            if let Some(m) = media
                .iter()
                .find(|m| !lists.iter().any(|(k, _)| *k == kind(m)))
            {
                return Err(unsupported(m, "images, audio and video"));
            }
            out.insert(
                "state".into(),
                state(
                    parts
                        .iter()
                        .filter(|p| !matches!(p, Part::Media(_)))
                        .map(plain)
                        .collect(),
                ),
            );
            for (k, field) in lists {
                let items = media
                    .iter()
                    .filter(|m| kind(m) == k)
                    .map(|m| data_url(m).map(Value::String))
                    .collect::<Result<Vec<_>, _>>()?;
                if !items.is_empty() {
                    out.insert(field.into(), Value::Array(items));
                }
            }
        }
        MediaInput::Assets => {
            if let Some(m) = media.iter().find(|m| kind(m) != "image") {
                return Err(unsupported(m, "images"));
            }
            let mut items = Vec::new();
            let mut assets = Map::new();
            for p in parts {
                match p {
                    Part::Media(m) => {
                        let reference = format!("<image-{}>", assets.len() + 1);
                        assets.insert(
                            reference.clone(),
                            json!({"type": "image", "format": &m.mime["image/".len()..],
                                "data_base64": inline(m)?}),
                        );
                        items.push(json!(reference));
                    }
                    other => items.push(plain(other)),
                }
            }
            out.insert("state".into(), Value::Array(items));
            out.insert("assets".into(), Value::Object(assets));
        }
    }
    Ok(out)
}

fn google_body(req: &DecisionRequest) -> Result<Map<String, Value>, Error> {
    let mut options = req.options.clone();
    let resolution = options
        .remove("mediaResolution")
        .and_then(|v| v.as_str().map(str::to_string));
    let mut blocks: Vec<Value> = Vec::new();
    for part in &req.input {
        let block = match part {
            Part::Text(t) => json!({"type": "text", "text": t}),
            // Serialized, so its keys are never read as content blocks.
            Part::Json(v) => json!({"type": "text", "text": v.to_string()}),
            Part::Media(m) => {
                let k = kind(m);
                let t = match k {
                    "image" | "audio" | "video" => k,
                    _ if m.mime.eq_ignore_ascii_case("application/pdf") => "document",
                    _ => {
                        return Err(Error::Invalid(format!(
                            "Google Decisions takes images, audio, video and PDFs; got {}",
                            m.mime
                        )))
                    }
                };
                let mut b = json!({"type": t, "mime_type": m.mime.to_ascii_lowercase()});
                if let Some(r) = &resolution {
                    if t == "image" || (t == "video" && r != "ultra_high") {
                        b["resolution"] = json!(r);
                    }
                }
                match &m.source {
                    Source::Base64(d) => b["data"] = json!(d),
                    Source::Url(u) if u.to_ascii_lowercase().starts_with("https://") => {
                        b["uri"] = json!(u)
                    }
                    Source::Url(_) => {
                        return Err(Error::Invalid(
                            "Google Decisions accepts inline media and public HTTPS URLs".into(),
                        ))
                    }
                }
                b
            }
        };
        // Google joins adjacent text blocks with no separator.
        let block = match (blocks.last(), block) {
            (Some(prev), mut b) if prev["type"] == "text" && b["type"] == "text" => {
                b["text"] = json!(format!("\n{}", b["text"].as_str().unwrap_or("")));
                b
            }
            (_, b) => b,
        };
        blocks.push(block);
    }
    let mut questions = encode_questions(&req.questions)?;
    // Google names predicate criteria `true_description` and
    // `false_description`.
    for q in &req.questions {
        if let Question::Predicate {
            name,
            criteria: Some(c),
            ..
        } = q
        {
            let mut m = Map::new();
            if let Some(t) = &c.r#true {
                m.insert("true_description".into(), json!(t));
            }
            if let Some(f) = &c.r#false {
                m.insert("false_description".into(), json!(f));
            }
            questions[name]["criteria"] = Value::Object(m);
        }
    }
    let mut body = options;
    body.insert("model".into(), json!(req.model));
    body.insert("input".into(), Value::Array(blocks));
    body.insert("questions".into(), questions);
    Ok(body)
}

fn probability(v: &Value, what: &str) -> Result<f64, Error> {
    v.as_f64()
        .filter(|p| (0.0..=1.0).contains(p))
        .ok_or_else(|| Error::Decode(format!("{what} returned an invalid probability: {v}")))
}

/// A host's answer, mapped back to the question it answers.
fn to_answer(q: &Question, a: &Value, what: &str) -> Result<Answer, Error> {
    let name = q.name().to_string();
    let probabilities = || {
        a["probabilities"]
            .as_object()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| probability(&v, what).map(|p| (k, p)))
            .collect::<Result<Vec<_>, _>>()
    };
    Ok(match a["type"].as_str() {
        Some("refusal") => Answer::Error {
            name,
            reason: "refusal".into(),
            message: a["message"].as_str().map(str::to_string),
        },
        Some("noul") => Answer::Predicate {
            name,
            probability: probability(&a["noul"], what)?,
        },
        Some("choice") => {
            let choices: &[Choice] = match q {
                Question::Choice { choices, .. } => choices,
                _ => &[],
            };
            let value = |k: &str| {
                choices
                    .iter()
                    .find(|c| choice_key(&c.value) == k)
                    .map(|c| c.value.clone())
                    .unwrap_or_else(|| json!(k))
            };
            let rank = |k: &str| {
                choices
                    .iter()
                    .position(|c| choice_key(&c.value) == k)
                    .unwrap_or(choices.len())
            };
            let mut ps = probabilities()?;
            ps.sort_by_key(|(k, _)| rank(k));
            Answer::Choice {
                name,
                choice: value(a["choice"].as_str().unwrap_or_default()),
                probabilities: ps
                    .into_iter()
                    .map(|(k, p)| ChoiceProbability {
                        value: value(&k),
                        probability: p,
                    })
                    .collect(),
                confidence: probability(&a["confidence"], what)?,
            }
        }
        Some("score") => {
            let levels: &[Level] = match q {
                Question::Score { levels, .. } => levels,
                _ => &[],
            };
            let mut ps: Vec<LevelProbability> = probabilities()?
                .into_iter()
                .map(|(k, p)| {
                    let i: i64 = k.parse().unwrap_or(-1);
                    LevelProbability {
                        value: i,
                        label: usize::try_from(i)
                            .ok()
                            .and_then(|i| levels.get(i))
                            .map(|l| l.label.clone())
                            .unwrap_or(k),
                        probability: p,
                    }
                })
                .collect();
            ps.sort_by_key(|l| l.value);
            Answer::Score {
                name,
                score: a["score"]
                    .as_f64()
                    .ok_or_else(|| Error::Decode(format!("{what} returned a score without one")))?,
                probabilities: ps,
                confidence: probability(&a["confidence"], what)?,
            }
        }
        _ => {
            return Err(Error::Decode(format!(
                "{what} returned an unknown answer: {a}"
            )))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img() -> Part {
        Part::Media(Media {
            mime: "image/png".into(),
            source: Source::Base64("aW1hZ2U=".into()),
            filename: None,
            meta: Default::default(),
        })
    }

    #[test]
    fn media_lands_where_the_host_reads_it() {
        let parts = vec![Part::Text("Is this a photo?".into()), img()];
        let root = place_media(&parts, MediaInput::Root).unwrap();
        assert_eq!(
            Value::Object(root),
            json!({"state": "Is this a photo?", "images": ["data:image/png;base64,aW1hZ2U="]})
        );
        let assets = place_media(&parts, MediaInput::Assets).unwrap();
        assert_eq!(
            Value::Object(assets),
            json!({"state": ["Is this a photo?", "<image-1>"],
                "assets": {"<image-1>": {"type": "image", "format": "png", "data_base64": "aW1hZ2U="}}})
        );
    }

    #[test]
    fn choice_keys_that_collide_are_rejected() {
        let q = Question::Choice {
            name: "c".into(),
            instructions: "?".into(),
            choices: vec![
                Choice {
                    value: json!(true),
                    description: None,
                },
                Choice {
                    value: json!("true"),
                    description: None,
                },
            ],
        };
        assert!(matches!(encode_questions(&[q]), Err(Error::Invalid(_))));
    }
}
