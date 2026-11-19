//! `websearch`: search the web through a self-hosted SearXNG instance (no
//! hosted search service). The tool exists only when `websearch.url` is set.
//! The model-facing description and result format follow OpenCode.

use super::{opt_u64, str_arg, Ctx, Outcome, Output};
use crate::permission::Request;
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;

pub const NO_RESULTS: &str = "No search results found. Please try a different query.";
pub const DEFAULT_RESULTS: usize = 8;

/// `websearch` in codesmith.json.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct WebSearchConfig {
    /// Base URL of a SearXNG instance with the JSON format enabled.
    pub url: Option<String>,
    /// Comma-separated SearXNG engines to use (instance defaults otherwise).
    pub engines: Option<String>,
    /// Maximum results returned to the model.
    pub results: Option<usize>,
}

impl WebSearchConfig {
    pub fn enabled(&self) -> bool {
        self.url
            .as_deref()
            .map(|u| !u.trim().is_empty())
            .unwrap_or(false)
    }
}

pub fn spec() -> (String, Value) {
    let year = chrono::Local::now().format("%Y");
    (
        format!("Search the web using the user's selected search integration. Use this for current information beyond knowledge cutoff.\n\nThe current year is {year}. Use this year when searching for recent information or current events."),
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Websearch query"},
                "limit": {"type": "integer", "minimum": 1, "maximum": 20, "description": format!("Maximum number of results (default {DEFAULT_RESULTS})")}
            },
            "required": ["query"]
        }),
    )
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub url: String,
    pub title: Option<String>,
    pub content: Option<String>,
    pub published: Option<String>,
}

/// Parse a SearXNG JSON response.
pub fn parse(v: &Value) -> Vec<Hit> {
    v["results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let url = r["url"].as_str()?.to_string();
            let s = |k: &str| {
                r[k].as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            };
            Some(Hit {
                url,
                title: s("title"),
                content: s("content"),
                published: s("publishedDate"),
            })
        })
        .collect()
}

/// Model-facing text (OpenCode's format).
pub fn format(hits: &[Hit]) -> String {
    if hits.is_empty() {
        return NO_RESULTS.into();
    }
    hits.iter()
        .map(|h| {
            let title = h.title.as_deref().unwrap_or(&h.url);
            let published = h
                .published
                .as_deref()
                .map(|p| format!(" (published {p})"))
                .unwrap_or_default();
            let content = h
                .content
                .as_deref()
                .map(|c| format!("\n\n{c}"))
                .unwrap_or_default();
            format!("## [{title}]({}){published}{content}", h.url)
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub async fn run(ctx: &Ctx<'_>, input: &Value) -> Outcome {
    let query = str_arg(input, "query")?.trim();
    if query.is_empty() {
        return Err("Query must not be empty".into());
    }
    let cfg = ctx.websearch.as_ref().filter(|c| c.enabled()).ok_or(
        "Web search is not configured. Set websearch.url to a SearXNG instance in codesmith.json.",
    )?;
    let base = cfg.url.as_deref().unwrap().trim_end_matches('/');
    let limit = opt_u64(input, "limit")
        .map(|l| l.clamp(1, 20) as usize)
        .unwrap_or(cfg.results.unwrap_or(DEFAULT_RESULTS));
    ctx.ask(Request {
        action: "websearch".into(),
        resources: vec![query.to_string()],
        save: vec!["*".into()],
        title: format!("Search the web for {query:?}"),
    })?;
    let mut params = vec![("q", query.to_string()), ("format", "json".to_string())];
    if let Some(e) = &cfg.engines {
        params.push(("engines", e.clone()));
    }
    let resp = ctx
        .client
        .get(format!("{base}/search"))
        .query(&params)
        .header(
            "User-Agent",
            concat!("codesmith/", env!("CARGO_PKG_VERSION")),
        )
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .map_err(|e| format!("Unable to search the web for {query}: {e}"))?;
    match resp.status().as_u16() {
        200 => {}
        403 => return Err("The SearXNG instance refused the JSON format. Enable it under search.formats (add json) in its settings.yml.".into()),
        429 => return Err("Web search rate limited (HTTP 429)".into()),
        s => return Err(format!("Unable to search the web for {query}: HTTP {s}")),
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|e| format!("Unable to read search results: {e}"))?;
    let mut hits = parse(&v);
    hits.truncate(limit);
    Ok(Output::text(format(&hits)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_searxng_json() {
        let v = json!({"results": [
            {"url": "https://a.dev", "title": "A dev", "content": "About A", "publishedDate": "2026-11-01"},
            {"url": "https://b.dev", "title": "  ", "content": ""},
            {"title": "no url"}
        ]});
        let hits = parse(&v);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[1].title, None);
        assert_eq!(format(&hits), "## [A dev](https://a.dev) (published 2026-11-01)\n\nAbout A\n\n## [https://b.dev](https://b.dev)");
        assert_eq!(format(&[]), NO_RESULTS);
        assert!(parse(&json!({})).is_empty());
    }

    #[test]
    fn config() {
        assert!(!WebSearchConfig::default().enabled());
        assert!(!WebSearchConfig {
            url: Some(" ".into()),
            ..Default::default()
        }
        .enabled());
        assert!(WebSearchConfig {
            url: Some("http://localhost:8888".into()),
            ..Default::default()
        }
        .enabled());
        let (d, p) = spec();
        assert!(d.contains("The current year is 20"));
        assert_eq!(p["required"], json!(["query"]));
    }
}
