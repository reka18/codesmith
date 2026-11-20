//! models.dev v1 catalogs (`/api.json`), still accepted from
//! `$CODESMITH_MODELS_PATH` and custom `$CODESMITH_MODELS_URL` mirrors.
//! They are converted to the v2 shape, then read like any v2 catalog. v1
//! can't say everything v2 does (one API per model, broad modality kinds),
//! so a converted catalog behaves as v1 did. After OpenCode's
//! `models-dev/v1.ts`.

use super::json::Json;

fn obj(entries: Vec<(&str, Json)>) -> Json {
    Json::Object(
        entries
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    )
}

fn s(v: &str) -> Json {
    Json::String(v.to_string())
}

fn num(v: f64) -> Json {
    serde_json::Number::from_f64(v).map_or(Json::Null, Json::Number)
}

/// v1 providers always name an AI SDK package (`npm`); v2 providers never do.
pub fn is_v1(catalog: &Json) -> bool {
    catalog
        .entries()
        .unwrap_or_default()
        .iter()
        .any(|(_, p)| p.get("npm").and_then(Json::str).is_some())
}

pub fn to_v2(catalog: &Json) -> Json {
    let mut out = Vec::new();
    for (key, provider) in catalog.entries().unwrap_or_default() {
        let (Some(id), Some(name), Some(env), Some(npm), Some(models)) = (
            provider.get("id").and_then(Json::str),
            provider.get("name").and_then(Json::str),
            provider.get("env").and_then(Json::strings),
            provider.get("npm").and_then(Json::str),
            provider.get("models").and_then(Json::entries),
        ) else {
            continue;
        };
        let api = match provider.get("api") {
            None => None,
            Some(a) => match a.str() {
                Some(a) => Some(a),
                None => continue,
            },
        };
        let converted: Vec<(String, Json)> = models
            .iter()
            .filter_map(|(mid, m)| Some((mid.clone(), convert_model(m, npm, api)?)))
            .collect();
        out.push((
            key.clone(),
            obj(vec![
                ("id", s(id)),
                ("name", s(name)),
                ("env", Json::Array(env.iter().map(|e| s(e)).collect())),
                ("models", Json::Object(converted)),
            ]),
        ));
    }
    Json::Object(out)
}

/// The AI SDK packages that call the Responses API unless told otherwise.
const RESPONSES_BY_DEFAULT: [&str; 3] = ["@ai-sdk/openai", "@ai-sdk/azure", "@ai-sdk/xai"];

fn opt_str(m: &Json, key: &str) -> Result<Option<String>, ()> {
    match m.get(key) {
        None => Ok(None),
        Some(v) => v.str().map(|s| Some(s.to_string())).ok_or(()),
    }
}

fn opt_bool(m: &Json, key: &str) -> Result<Option<bool>, ()> {
    match m.get(key) {
        None => Ok(None),
        Some(v) => v.bool().map(Some).ok_or(()),
    }
}

fn convert_model(m: &Json, provider_npm: &str, provider_api: Option<&str>) -> Option<Json> {
    let id = m.get("id")?.str()?;
    let name = m.get("name")?.str()?;
    let release = m.get("release_date")?.str()?;
    let kind = opt_str(m, "type").ok()?;
    let family = opt_str(m, "family").ok()?;
    let status = opt_str(m, "status").ok()?;
    let reasoning = opt_bool(m, "reasoning").ok()?;
    let temperature = opt_bool(m, "temperature").ok()?;
    let tool_call = opt_bool(m, "tool_call").ok()?;
    let mp = match m.get("provider") {
        None => None,
        Some(p) if p.is_object() => Some(p),
        Some(_) => return None,
    };
    let field = |k: &str| mp.and_then(|p| p.get(k)).and_then(Json::str);
    let npm = field("npm").unwrap_or(provider_npm);
    let shape = field("shape");
    let protocol =
        if shape == Some("responses") || (shape.is_none() && RESPONSES_BY_DEFAULT.contains(&npm)) {
            "responses"
        } else {
            "chat-completions"
        };
    let interleaved = match m.get("interleaved") {
        Some(Json::String(f)) => Some(f.clone()),
        Some(o @ Json::Object(_)) => Some(o.get("field")?.str()?.to_string()),
        Some(Json::Bool(_)) | None => None,
        Some(_) => return None,
    };
    let cost = match m.get("cost") {
        None => None,
        Some(c) => Some(convert_cost(c)?),
    };
    let modalities = match m.get("modalities") {
        None => obj(vec![
            ("input", Json::Array(vec![s("text")])),
            ("output", Json::Array(vec![s("text")])),
        ]),
        Some(v) => {
            v.get("input")?.strings()?;
            v.get("output")?.strings()?;
            v.clone()
        }
    };
    let limit = match m.get("limit") {
        None => None,
        Some(l) => {
            if !l.entries()?.iter().all(|(_, v)| v.num().is_some()) {
                return None;
            }
            Some(l.clone())
        }
    };
    let mut api_entry = vec![
        (
            "base_url",
            s(field("api").or(provider_api).unwrap_or_default()),
        ),
        ("sdk", obj(vec![("aisdk", s(npm))])),
    ];
    if let Some(f) = interleaved {
        api_entry.push(("compatibility", obj(vec![("reasoning_field", s(&f))])));
    }
    let mut capabilities = vec![
        (
            "tools",
            obj(vec![("supported", Json::Bool(tool_call.unwrap_or(false)))]),
        ),
        (
            "reasoning",
            convert_reasoning(m, reasoning.unwrap_or(false))?,
        ),
    ];
    if let Some(t) = temperature {
        capabilities.push(("temperature", Json::Bool(t)));
    }
    let mut out = vec![
        ("id", s(id)),
        ("type", s(kind.as_deref().unwrap_or("chat"))),
        ("name", s(name)),
    ];
    if let Some(f) = family {
        out.push(("family", s(&f)));
    }
    out.push(("release_date", s(release)));
    if let Some(st) = status {
        out.push(("status", s(&st)));
    }
    out.push(("modalities", modalities));
    out.push(("capabilities", obj(capabilities)));
    if let Some(l) = limit {
        out.push(("limit", l));
    }
    if let Some(c) = cost {
        out.push(("cost", c));
    }
    out.push(("api", obj(vec![(protocol, obj(api_entry))])));
    if let Some(modes) = m.get("experimental").and_then(|e| e.get("modes")) {
        let mut converted = Vec::new();
        for (name, mode) in modes.entries()? {
            let mut entry = Vec::new();
            if let Some(c) = mode.get("cost") {
                entry.push(("cost", convert_cost(c)?));
            }
            if let Some(p) = mode.get("provider") {
                if let Some(b) = p.get("body") {
                    entry.push(("body", b.clone()));
                }
                if let Some(h) = p.get("headers") {
                    entry.push(("headers", h.clone()));
                }
            }
            converted.push((name.clone(), obj(entry)));
        }
        out.push((
            "experimental",
            obj(vec![("modes", Json::Object(converted))]),
        ));
    }
    Some(obj(out))
}

fn convert_reasoning(m: &Json, reasoning: bool) -> Option<Json> {
    if !reasoning {
        return Some(obj(vec![("supported", Json::Bool(false))]));
    }
    let options = match m.get("reasoning_options") {
        None => vec![],
        Some(o) => o.array()?.to_vec(),
    };
    let kind = |o: &Json| o.get("type").and_then(Json::str).map(str::to_string);
    let mut out = vec![("supported", Json::Bool(true))];
    if options.iter().any(|o| kind(o).as_deref() == Some("toggle")) {
        out.push(("toggle", Json::Bool(true)));
    }
    if let Some(effort) = options
        .iter()
        .find(|o| kind(o).as_deref() == Some("effort"))
    {
        let values = effort
            .get("values")
            .and_then(Json::array)
            .unwrap_or_default()
            .iter()
            .filter_map(|v| v.str().filter(|v| *v != "null").map(s))
            .collect();
        out.push(("effort", Json::Array(values)));
    }
    if let Some(budget) = options
        .iter()
        .find(|o| kind(o).as_deref() == Some("budget_tokens"))
    {
        let mut b = Vec::new();
        for k in ["min", "max"] {
            if let Some(v) = budget.get(k).and_then(Json::num) {
                b.push((k, num(v)));
            }
        }
        out.push(("budget", obj(b)));
    }
    Some(obj(out))
}

/// v1 tiers are context bands; v2 says the same with `context` overrides.
fn convert_cost(c: &Json) -> Option<Json> {
    let price = |o: &Json, k: &str| -> Option<Option<f64>> {
        match o.get(k) {
            None => Some(None),
            Some(v) => v.num().map(Some),
        }
    };
    let input = price(c, "input")?;
    let output = price(c, "output")?;
    let cache_read = price(c, "cache_read")?;
    let cache_write = price(c, "cache_write")?;
    let tiers = match c.get("tiers") {
        None => vec![],
        Some(t) => t.array()?.to_vec(),
    };
    let cache = |read: Option<f64>, write: Option<f64>| {
        let mut e = Vec::new();
        if let Some(r) = read {
            e.push(("read", num(r)));
        }
        if let Some(w) = write {
            e.push(("write", num(w)));
        }
        obj(e)
    };
    let when = |size: f64| obj(vec![("context", obj(vec![("gt", num(size))]))]);
    let mut input_overrides = Vec::new();
    let mut output_overrides = Vec::new();
    for t in &tiers {
        let size = t.get("tier")?.get("size")?.num()?;
        let (ti, to) = (price(t, "input")?, price(t, "output")?);
        let (tr, tw) = (price(t, "cache_read")?, price(t, "cache_write")?);
        let mut i = vec![("when", when(size))];
        if let Some(p) = ti {
            i.push(("price", num(p)));
        }
        i.push(("cache", cache(tr, tw)));
        input_overrides.push(obj(i));
        let mut o = vec![("when", when(size))];
        if let Some(p) = to {
            o.push(("price", num(p)));
        }
        output_overrides.push(obj(o));
    }
    Some(obj(vec![(
        "text",
        obj(vec![
            (
                "input",
                obj(vec![
                    ("price", num(input.unwrap_or(0.0))),
                    ("cache", cache(cache_read, cache_write)),
                    ("overrides", Json::Array(input_overrides)),
                ]),
            ),
            (
                "output",
                obj(vec![
                    ("price", num(output.unwrap_or(0.0))),
                    ("overrides", Json::Array(output_overrides)),
                ]),
            ),
        ]),
    )]))
}
