//! Defaults that make known MCP servers work well with Code Mode, as
//! OpenCode's `mcp-codemode-defaults` plugin sets them, so a server's own
//! Code Mode never ends up nested inside CodeSmith's. A server with
//! `codemode` set in config is left exactly as configured. Only config is
//! changed: nothing is contacted.

use super::ServerConfig;

/// The header PostHog reads to choose between its own Code Mode tool and
/// its plain tools.
pub const POSTHOG_MODE: &str = "x-posthog-mcp-mode";

/// Executor (`executor.sh`) is a Code Mode server itself: its endpoint
/// `https://executor.sh/<workspace>/mcp`, or `executor mcp` run locally.
fn executor(url: &reqwest::Url) -> bool {
    let segments: Vec<&str> = url.path().trim_end_matches('/').split('/').collect();
    url.scheme() == "https"
        && url.host_str() == Some("executor.sh")
        && url.port().is_none()
        && matches!(segments.as_slice(), ["", ws, "mcp"] if !ws.is_empty())
}

/// PostHog's MCP hosts (`mcp.posthog.com`, `mcp-eu.`, `mcp.us.`, `mcp.eu.`).
fn posthog(host: &str) -> bool {
    matches!(
        host,
        "mcp.posthog.com" | "mcp-eu.posthog.com" | "mcp.us.posthog.com" | "mcp.eu.posthog.com"
    )
}

/// Apply the known-server defaults to `server` unless its `codemode` is
/// configured: Code Mode off for Executor, and PostHog asked for its plain
/// tools (`x-posthog-mcp-mode: tools`, unless the URL's `mode` or a header
/// already picks one) so CodeSmith's Code Mode wraps them.
pub fn apply(server: &mut ServerConfig) {
    match server {
        ServerConfig::Local {
            command, codemode, ..
        } => {
            if codemode.is_none()
                && command.first().map(String::as_str) == Some("executor")
                && command.get(1).map(String::as_str) == Some("mcp")
            {
                *codemode = Some(false);
            }
        }
        ServerConfig::Remote {
            url,
            headers,
            codemode,
            ..
        } => {
            if codemode.is_some() {
                return;
            }
            let Ok(u) = reqwest::Url::parse(url) else {
                return;
            };
            if executor(&u) {
                *codemode = Some(false);
            }
            if u.host_str().is_some_and(posthog)
                && !u.query_pairs().any(|(k, _)| k == "mode")
                && !headers.keys().any(|k| k.eq_ignore_ascii_case(POSTHOG_MODE))
            {
                headers.insert(POSTHOG_MODE.into(), "tools".into());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::collections::BTreeMap;

    /// A server's `codemode` and (for a remote one) headers.
    type Applied = (Option<bool>, Option<BTreeMap<String, String>>);
    /// A row of the table: its name, the server, and what it ends up with.
    type Case = (
        &'static str,
        Value,
        Option<bool>,
        Option<BTreeMap<String, String>>,
    );

    fn applied(server: Value) -> Applied {
        let mut s: ServerConfig = serde_json::from_value(server).unwrap();
        apply(&mut s);
        match s {
            ServerConfig::Local { codemode, .. } => (codemode, None),
            ServerConfig::Remote {
                codemode, headers, ..
            } => (codemode, Some(headers)),
        }
    }

    fn h(pairs: &[(&str, &str)]) -> Option<BTreeMap<String, String>> {
        Some(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    // plugin/mcp-codemode-defaults.test.ts: "applies Code Mode defaults for
    // known MCP servers unless codemode is configured" (each row of its
    // table)
    #[test]
    fn oc_mcp_codemode_defaults_apply_to_known_servers_unless_codemode_is_configured() {
        let remote = |url: &str| json!({"type": "remote", "url": url});
        let cases: Vec<Case> = vec![
            (
                "executor remote",
                remote("https://executor.sh/example/mcp?source=opencode"),
                Some(false),
                h(&[]),
            ),
            (
                "executor local",
                json!({"type": "local", "command": ["executor", "mcp"]}),
                Some(false),
                None,
            ),
            (
                "cloudflare code mode",
                remote("https://mcp.cloudflare.com/mcp/"),
                None,
                h(&[]),
            ),
            (
                "cloudflare raw tools",
                remote("https://mcp.cloudflare.com/mcp?codemode=false"),
                None,
                h(&[]),
            ),
            (
                "cloudflare docs",
                remote("https://docs.mcp.cloudflare.com/mcp"),
                None,
                h(&[]),
            ),
            (
                "explicit true",
                json!({"type": "remote", "url": "https://mcp.cloudflare.com/mcp", "codemode": true}),
                Some(true),
                h(&[]),
            ),
            (
                "explicit false",
                json!({"type": "remote", "url": "https://executor.sh/example/mcp", "codemode": false}),
                Some(false),
                h(&[]),
            ),
            ("exa", remote("https://mcp.exa.ai/mcp"), None, h(&[])),
            ("unrelated", remote("https://example.com/mcp"), None, h(&[])),
            (
                "posthog",
                remote("https://mcp.posthog.com/mcp"),
                None,
                h(&[("x-posthog-mcp-mode", "tools")]),
            ),
            (
                "posthog eu with auth",
                json!({"type": "remote", "url": "https://mcp-eu.posthog.com/mcp", "headers": {"Authorization": "Bearer phx_test"}}),
                None,
                h(&[
                    ("Authorization", "Bearer phx_test"),
                    ("x-posthog-mcp-mode", "tools"),
                ]),
            ),
            (
                "posthog explicit code mode",
                json!({"type": "remote", "url": "https://mcp.us.posthog.com/mcp", "codemode": true}),
                Some(true),
                h(&[]),
            ),
            (
                "posthog mode in url",
                remote("https://mcp.posthog.com/mcp?mode=cli"),
                None,
                h(&[]),
            ),
            (
                "posthog mode header",
                json!({"type": "remote", "url": "https://mcp.posthog.com/mcp", "headers": {"X-PostHog-MCP-Mode": "cli"}}),
                None,
                h(&[("X-PostHog-MCP-Mode", "cli")]),
            ),
            (
                "posthog without code mode",
                json!({"type": "remote", "url": "https://mcp.posthog.com/mcp", "codemode": false}),
                Some(false),
                h(&[]),
            ),
        ];
        for (name, server, codemode, headers) in cases {
            assert_eq!(applied(server), (codemode, headers), "{name}");
        }
    }

    // Look-alikes get no defaults: other hosts, schemes, ports and paths,
    // and commands that only start like Executor's.
    #[test]
    fn mcp_codemode_defaults_skip_look_alikes() {
        let remote = |url: &str| applied(json!({"type": "remote", "url": url}));
        for url in [
            "http://executor.sh/example/mcp",
            "https://executor.sh.evil.example/example/mcp",
            "https://evil.example/executor.sh/x/mcp",
            "https://executor.sh:8443/example/mcp",
            "https://executor.sh/example/other",
            "https://executor.sh/a/b/mcp",
            "https://executor.sh//mcp",
            "not a url",
            "",
        ] {
            assert_eq!(remote(url).0, None, "{url}");
        }
        for host in [
            "mcp.posthog.com.evil.example",
            "posthog.com",
            "mcp-us.posthog.com",
            "evil-mcp.posthog.com",
        ] {
            assert_eq!(
                remote(&format!("https://{host}/mcp")),
                (None, h(&[])),
                "{host}"
            );
        }
        for command in [
            json!(["executor"]),
            json!(["executor", "serve"]),
            json!(["/usr/bin/executor", "mcp"]),
            json!([]),
        ] {
            let (codemode, _) = applied(json!({"type": "local", "command": command}));
            assert_eq!(codemode, None, "{command}");
        }
        // The trailing slash and the default port spelled out are the same
        // endpoint.
        assert_eq!(remote("https://executor.sh/w/mcp/").0, Some(false));
        assert_eq!(remote("https://executor.sh:443/w/mcp").0, Some(false));
        assert_eq!(remote("https://EXECUTOR.SH/w/mcp").0, Some(false));
    }
}
