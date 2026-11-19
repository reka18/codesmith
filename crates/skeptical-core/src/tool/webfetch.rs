//! `webfetch`: fetch a URL as markdown, text or HTML. Ported from OpenCode's
//! webfetch tool: redirects are followed by hand so every hop is checked
//! against the permission rules, and responses are size-capped.

use super::{opt_str, opt_u64, str_arg, Ctx, Outcome, Output};
use crate::permission::Request;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::time::Duration;

pub const MAX_BYTES: usize = 5 * 1024 * 1024;
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
pub const MAX_TIMEOUT_SECS: u64 = 120;
pub const MAX_REDIRECTS: usize = 10;

pub fn spec() -> (String, Value) {
    (
        "Fetch content from an HTTP or HTTPS URL and return it as text, markdown, or HTML. Markdown is the default.\n\nUse a more targeted tool when one is available. This tool is read-only. Large text results may be replaced with a preview while the complete output is retained in managed storage.".into(),
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "The HTTP or HTTPS URL to fetch content from"},
                "format": {"type": "string", "enum": ["text", "markdown", "html"], "description": "The format to return the content in. Defaults to markdown."},
                "timeout": {"type": "number", "exclusiveMinimum": 0, "maximum": MAX_TIMEOUT_SECS, "description": format!("Optional timeout in seconds (maximum: {MAX_TIMEOUT_SECS})")}
            },
            "required": ["url"]
        }),
    )
}

pub fn accept(format: &str) -> &'static str {
    match format {
        "text" => "text/plain;q=1.0, text/markdown;q=0.9, text/html;q=0.8, */*;q=0.1",
        "html" => "text/html;q=1.0, application/xhtml+xml;q=0.9, text/plain;q=0.8, text/markdown;q=0.7, */*;q=0.1",
        _ => "text/markdown;q=1.0, text/x-markdown;q=0.9, text/plain;q=0.8, text/html;q=0.7, */*;q=0.1",
    }
}

pub fn is_html(content_type: &str) -> bool {
    let c = content_type.to_ascii_lowercase();
    c.contains("text/html") || c.contains("application/xhtml")
}

/// Convert a body to the requested format. Non-HTML bodies pass through.
pub fn convert(body: &str, content_type: &str, format: &str) -> String {
    if !is_html(content_type) || format == "html" {
        return body.to_string();
    }
    match format {
        "text" => html_to_text(body),
        _ => htmd::HtmlToMarkdown::builder()
            .skip_tags(vec!["script", "style", "noscript", "iframe", "svg", "head"])
            .build()
            .convert(body)
            .unwrap_or_else(|_| html_to_text(body)),
    }
}

/// Visible text of an HTML document: scripts and styles dropped, tags removed,
/// common entities decoded, whitespace collapsed.
pub fn html_to_text(html: &str) -> String {
    let drop = regex::Regex::new(
        r"(?is)<(script|style|noscript|head|svg)\b.*?</(script|style|noscript|head|svg)>",
    )
    .unwrap();
    let block =
        regex::Regex::new(r"(?i)<(br|/p|/div|/li|/h[1-6]|/tr|/section|/article)[^>]*>").unwrap();
    let tag = regex::Regex::new(r"(?s)<[^>]*>").unwrap();
    let spaces = regex::Regex::new(r"[ \t]+").unwrap();
    let blank = regex::Regex::new(r"\n\s*\n+").unwrap();
    let t = drop.replace_all(html, "");
    let t = block.replace_all(&t, "\n");
    let t = tag.replace_all(&t, "");
    let t = t
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    let t = spaces.replace_all(&t, " ");
    let lines: Vec<&str> = t.lines().map(str::trim).collect();
    blank
        .replace_all(&lines.join("\n"), "\n\n")
        .trim()
        .to_string()
}

pub async fn run(ctx: &Ctx<'_>, input: &Value) -> Outcome {
    let url = str_arg(input, "url")?;
    let format = opt_str(input, "format").unwrap_or("markdown");
    if !matches!(format, "text" | "markdown" | "html") {
        return Err(format!(
            "Invalid format {format:?}: use text, markdown or html"
        ));
    }
    let timeout = input
        .get("timeout")
        .and_then(Value::as_f64)
        .or_else(|| opt_u64(input, "timeout").map(|t| t as f64))
        .unwrap_or(DEFAULT_TIMEOUT_SECS as f64);
    if timeout <= 0.0 || timeout > MAX_TIMEOUT_SECS as f64 {
        return Err(format!(
            "Timeout must be between 0 and {MAX_TIMEOUT_SECS} seconds"
        ));
    }
    let mut current = parse_url(url)?;
    for hop in 0..=MAX_REDIRECTS {
        ctx.ask(Request {
            action: "webfetch".into(),
            resources: vec![current.to_string()],
            save: vec![format!(
                "{}://{}/*",
                current.scheme(),
                current.host_str().unwrap_or("")
            )],
            title: format!("Fetch {current}"),
        })?;
        let resp = ctx
            .client
            .get(current.clone())
            .header(
                "User-Agent",
                concat!("skeptical/", env!("CARGO_PKG_VERSION")),
            )
            .header("Accept", accept(format))
            .timeout(Duration::from_secs_f64(timeout))
            .send()
            .await
            .map_err(|e| format!("Unable to fetch {url}: {e}"))?;
        let status = resp.status();
        if status.is_redirection() {
            let location = resp
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| format!("Redirect from {current} has no Location header"))?;
            if hop == MAX_REDIRECTS {
                return Err(format!("Too many redirects (more than {MAX_REDIRECTS})"));
            }
            current = current
                .join(location)
                .map_err(|e| format!("Invalid redirect location {location:?}: {e}"))?;
            if !matches!(current.scheme(), "http" | "https") {
                return Err(format!("Refusing to follow a redirect to {current}"));
            }
            continue;
        }
        if status.as_u16() == 403
            && resp
                .headers()
                .get("cf-mitigated")
                .map(|v| v == "challenge")
                .unwrap_or(false)
        {
            return Err(format!(
                "Unable to fetch {url}: the site requires a browser challenge (Cloudflare)"
            ));
        }
        if !status.is_success() {
            return Err(format!("Unable to fetch {url}: HTTP {}", status.as_u16()));
        }
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let mut body = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| format!("Unable to fetch {url}: {e}"))?;
            if body.len() + chunk.len() > MAX_BYTES {
                return Err(format!(
                    "Response too large (exceeds {MAX_BYTES} byte limit)"
                ));
            }
            body.extend_from_slice(&chunk);
        }
        if crate::tool::read::is_binary(&body) {
            return Err(format!(
                "Unable to fetch {url}: the response is binary ({content_type})"
            ));
        }
        let text = String::from_utf8_lossy(&body);
        return Ok(Output::text(convert(&text, &content_type, format)));
    }
    Err(format!("Too many redirects (more than {MAX_REDIRECTS})"))
}

fn parse_url(url: &str) -> Result<reqwest::Url, String> {
    let u = reqwest::Url::parse(url).map_err(|e| format!("Invalid URL {url:?}: {e}"))?;
    if !matches!(u.scheme(), "http" | "https") {
        return Err(format!(
            "Only http and https URLs can be fetched, not {}",
            u.scheme()
        ));
    }
    Ok(u)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_headers() {
        assert!(accept("markdown").starts_with("text/markdown;q=1.0"));
        assert!(accept("text").starts_with("text/plain;q=1.0"));
        assert!(accept("html").starts_with("text/html;q=1.0"));
    }

    #[test]
    fn html_conversion() {
        let html = "<html><head><title>T</title><style>p{}</style></head><body><h1>Hello</h1><p>One &amp; two</p><script>evil()</script><ul><li>a</li><li>b</li></ul></body></html>";
        let md = convert(html, "text/html; charset=utf-8", "markdown");
        assert!(
            md.contains("# Hello")
                && md.contains("One & two")
                && !md.contains("evil")
                && !md.contains("p{}"),
            "{md}"
        );
        let text = convert(html, "text/html", "text");
        assert!(
            text.contains("Hello")
                && text.contains("One & two")
                && !text.contains('<')
                && !text.contains("evil"),
            "{text}"
        );
        assert_eq!(convert(html, "text/html", "html"), html);
        assert_eq!(
            convert("{\"a\":1}", "application/json", "markdown"),
            "{\"a\":1}"
        );
    }

    #[test]
    fn url_validation() {
        assert!(parse_url("https://example.com/x").is_ok());
        assert!(parse_url("file:///etc/passwd")
            .unwrap_err()
            .contains("Only http and https"));
        assert!(parse_url("not a url")
            .unwrap_err()
            .starts_with("Invalid URL"));
    }
}
