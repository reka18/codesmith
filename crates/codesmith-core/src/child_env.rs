//! The environment of the programs CodeSmith starts for the model and the
//! project: shell tool commands (local and through the shell daemon),
//! background jobs, `!cmd` and command-template interpolation, MCP stdio
//! servers, language servers, formatters and the worktree start command.
//!
//! Such a program is under the model's (or the project's) control, so it
//! must not see the credentials CodeSmith itself runs with. Before one
//! starts, every variable [`Policy::removes`] is taken out:
//!
//! - every variable CodeSmith knows holds a credential: the key variables
//!   of the built-in providers and of the bundled models.dev catalog, the
//!   `env` lists of configured providers, the `{env:NAME}` references a
//!   config file uses for credentials (see [`credential_refs`]), and
//!   CodeSmith's own secrets such as `CODESMITH_SERVER_TOKEN`;
//! - every variable whose name looks like a credential: `*_API_KEY`,
//!   `*_APIKEY`, `*_KEY`, `*SECRET*`, `*_TOKEN`, `*PASSWORD*`, `*_PASSWD`,
//!   `*_PAT`, and a list of well-known names (`GITHUB_TOKEN`, `GH_TOKEN`,
//!   `NPM_TOKEN`, `HF_TOKEN`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`,
//!   `AWS_ACCESS_KEY_ID`, `PGPASSWORD`, `MYSQL_PWD`, ...);
//! - and every name the config's `shell.env.remove` lists (any config
//!   file may add to it: it can only remove more).
//!
//! What tools need is never removed ([`KEEP`]): `PATH`, `HOME`, `USER`,
//! `SHELL`, `TERM`, `TMPDIR`, `LANG` and `LC_*`, `SSH_AUTH_SOCK`, the proxy
//! variables, `XDG_*` and toolchain homes such as `CARGO_HOME`, `GOPATH`.
//!
//! A user passes a variable through on purpose with `shell.env.pass` in
//! the global config file (`{"shell": {"env": {"pass": ["GITHUB_TOKEN"]}}}`).
//! A project config (or `$CODESMITH_CONFIG`, or the config content
//! variable) cannot: [`crate::config::load_from`] drops `shell.env.pass`
//! from every source but the global config file. CodeSmith's own secrets
//! ([`INTERNAL`]) are never passed.
//!
//! OpenCode passes the whole host environment to commands (only a plugin's
//! `shell.env` hook can remove a variable) and merges it into MCP stdio
//! servers; CodeSmith removes credentials by default.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{LazyLock, RwLock};

/// CodeSmith's own secrets: never passed, whatever the config says.
pub const INTERNAL: &[&str] = &[
    crate::SERVER_TOKEN_VAR,
    "CODESMITH_SERVER_PASSWORD",
    "OPENCODE_SERVER_PASSWORD",
];

/// Well-known credential variables the patterns do not all catch.
pub const WELL_KNOWN: &[&str] = &[
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_BEARER_TOKEN_BEDROCK",
    "GITHUB_TOKEN",
    "GH_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
    "COPILOT_GITHUB_TOKEN",
    "GITLAB_TOKEN",
    "NPM_TOKEN",
    "NODE_AUTH_TOKEN",
    "HF_TOKEN",
    "HUGGING_FACE_HUB_TOKEN",
    "CARGO_REGISTRY_TOKEN",
    "PYPI_TOKEN",
    "TWINE_PASSWORD",
    "PGPASSWORD",
    "MYSQL_PWD",
    "DOCKER_PASSWORD",
    "VAULT_TOKEN",
    "SLACK_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
];

/// Never removed: what tools need to run. Matched exactly, or as a prefix
/// for the names ending in `*`.
pub const KEEP: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TERM",
    "TERM_PROGRAM",
    "COLORTERM",
    "TMPDIR",
    "TMP",
    "TEMP",
    "TZ",
    "PWD",
    "LANG",
    "LANGUAGE",
    "LC_*",
    "SSH_AUTH_SOCK",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
    "XDG_*",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "GOPATH",
    "GOROOT",
    "GOBIN",
    "GOMODCACHE",
    "JAVA_HOME",
    "NVM_DIR",
    "PYENV_ROOT",
    "VIRTUAL_ENV",
    "CONDA_PREFIX",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    // Paths, not secrets: a key file stays readable, so removing the name
    // would only break the tools that use it.
    "GOOGLE_APPLICATION_CREDENTIALS",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
    "REQUESTS_CA_BUNDLE",
];

/// Whether `name` is in [`KEEP`].
pub fn kept(name: &str) -> bool {
    KEEP.iter().any(|k| match k.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => name == *k,
    })
}

/// Whether `name` looks like a credential by its name alone.
pub fn looks_secret(name: &str) -> bool {
    let n = name.to_ascii_uppercase();
    if WELL_KNOWN.contains(&n.as_str()) || INTERNAL.contains(&n.as_str()) {
        return true;
    }
    const SUFFIXES: [&str; 7] = [
        "_API_KEY", "_APIKEY", "_KEY", "_TOKEN", "_PASSWD", "_PAT", "_PWD",
    ];
    const EXACT: [&str; 6] = [
        "API_KEY",
        "APIKEY",
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "ACCESS_TOKEN",
    ];
    EXACT.contains(&n.as_str())
        || SUFFIXES.iter().any(|s| n.ends_with(s))
        || n.contains("SECRET")
        || n.contains("PASSWORD")
        || n.contains("_TOKEN_")
}

/// Catalog variables that configure a provider rather than authenticate
/// to it (a region, project, account or URL): models.dev lists them with
/// the keys.
fn catalog_setting(name: &str) -> bool {
    const SUFFIXES: [&str; 10] = [
        "_REGION",
        "_LOCATION",
        "_PROJECT",
        "_PROJECT_ID",
        "_ACCOUNT",
        "_ACCOUNT_ID",
        "_HOST",
        "_URL",
        "_RESOURCE_NAME",
        "_GATEWAY_ID",
    ];
    SUFFIXES.iter().any(|s| name.ends_with(s)) || name.ends_with("_PRODUCT_ID")
}

/// The key variables of every built-in and bundled-catalog provider.
static CATALOG: LazyLock<BTreeSet<String>> = LazyLock::new(|| {
    let mut s: BTreeSet<String> = BTreeSet::new();
    for b in crate::config::BUILTIN {
        s.extend(b.4.iter().map(|v| v.to_string()));
    }
    for p in crate::providers::PROVIDERS {
        s.extend(p.2.iter().map(|v| v.to_string()));
    }
    for p in crate::catalog::bundled().iter() {
        s.extend(
            p.environment
                .iter()
                .filter(|v| !catalog_setting(v) && !kept(v))
                .cloned(),
        );
    }
    s
});

/// Which variables a started program loses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Policy {
    /// Credential names learned from the config.
    pub known: BTreeSet<String>,
    /// Names the global config passes through on purpose.
    pub pass: BTreeSet<String>,
}

impl Policy {
    /// The policy of a loaded config.
    pub fn from_config(c: &crate::config::Config) -> Policy {
        let mut known: BTreeSet<String> = c.shell_env.credentials.iter().cloned().collect();
        known.extend(c.shell_env.remove.iter().cloned());
        for p in c.providers.values() {
            known.extend(p.env.iter().cloned());
        }
        Policy {
            known,
            pass: c.shell_env.pass.iter().cloned().collect(),
        }
    }

    /// Whether a started program loses `name`.
    pub fn removes(&self, name: &str) -> bool {
        if INTERNAL.contains(&name) {
            return true;
        }
        if self.pass.contains(name) {
            return false;
        }
        if self.known.contains(name) {
            // A name the config lists explicitly, unless it is one tools
            // need (a `{env:HOME}` in a provider's settings is no key).
            return !kept(name);
        }
        !kept(name) && (looks_secret(name) || CATALOG.contains(name))
    }

    /// Remove what [`Policy::removes`] from an environment.
    pub fn scrub_map(&self, env: &mut BTreeMap<String, String>) {
        env.retain(|k, _| !self.removes(k));
    }

    /// Remove what [`Policy::removes`] of this process's environment from
    /// what `cmd` inherits. Variables set on `cmd` afterwards are kept.
    pub fn scrub_command(&self, cmd: &mut std::process::Command) {
        for (k, _) in std::env::vars_os() {
            if let Some(k) = k.to_str() {
                if self.removes(k) {
                    cmd.env_remove(k);
                }
            }
        }
    }
}

/// The installed policy, learned from every config loaded (the known
/// credential names accumulate; the pass list is the latest global one).
static CURRENT: LazyLock<RwLock<Policy>> = LazyLock::new(|| RwLock::new(Policy::default()));

/// Install a loaded config's policy (see [`crate::config::load`]).
pub fn install(c: &crate::config::Config) {
    let p = Policy::from_config(c);
    let mut cur = CURRENT.write().unwrap_or_else(|e| e.into_inner());
    cur.known.extend(p.known);
    cur.pass = p.pass;
}

/// The policy in force.
pub fn current() -> Policy {
    CURRENT.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// [`Policy::scrub_map`] with the policy in force.
pub fn scrub_map(env: &mut BTreeMap<String, String>) {
    current().scrub_map(env)
}

/// [`Policy::scrub_command`] with the policy in force.
pub fn scrub(cmd: &mut std::process::Command) {
    current().scrub_command(cmd)
}

/// [`scrub`] for a Tokio command.
pub fn scrub_tokio(cmd: &mut tokio::process::Command) {
    scrub(cmd.as_std_mut())
}

/// A command for `program` that inherits this process's environment
/// without what the policy in force removes.
pub fn scrubbed(program: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    let mut c = std::process::Command::new(program);
    scrub(&mut c);
    c
}

/// [`scrubbed`] for Tokio.
pub fn scrubbed_tokio(program: impl AsRef<std::ffi::OsStr>) -> tokio::process::Command {
    let mut c = tokio::process::Command::new(program);
    scrub_tokio(&mut c);
    c
}

/// This process's environment as a program CodeSmith starts gets it, with
/// the policy in force. Variables that are not UTF-8 are left out.
pub fn host_env() -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect();
    scrub_map(&mut env);
    env
}

/// The `{env:NAME}` names a raw (unsubstituted) config document uses for
/// credentials: anywhere under `providers`, under an MCP server's
/// `headers`, `oauth` or `environment`, and anywhere under a key whose
/// name says it holds a credential (`apiKey`, `token`, `secret`,
/// `password`, `authorization`, `credential`, `headers`).
pub fn credential_refs(raw: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&crate::config::strip_jsonc(raw)) else {
        return refs_in(raw);
    };
    let mut out = Vec::new();
    walk(&v, &mut Vec::new(), &mut out);
    out.sort();
    out.dedup();
    out
}

fn credential_key(k: &str) -> bool {
    let k = k.to_ascii_lowercase().replace(['_', '-'], "");
    [
        "apikey",
        "token",
        "secret",
        "password",
        "authorization",
        "credential",
        "headers",
    ]
    .iter()
    .any(|w| k.contains(w))
}

fn walk(v: &serde_json::Value, path: &mut Vec<String>, out: &mut Vec<String>) {
    match v {
        serde_json::Value::Object(m) => {
            for (k, x) in m {
                path.push(k.clone());
                walk(x, path, out);
                path.pop();
            }
        }
        serde_json::Value::Array(a) => a.iter().for_each(|x| walk(x, path, out)),
        serde_json::Value::String(s) => {
            let first = path.first().map(String::as_str);
            let counts = first == Some("providers")
                || (first == Some("mcp")
                    && path.get(2).is_some_and(|k| {
                        matches!(k.as_str(), "headers" | "oauth" | "environment")
                    }))
                || path.iter().any(|k| credential_key(k));
            if counts {
                out.extend(refs_in(s));
            }
        }
        _ => {}
    }
}

/// Every `{env:NAME}` in `s`.
fn refs_in(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find("{env:") {
        rest = &rest[i + 5..];
        if let Some(j) = rest.find('}') {
            let name = rest[..j].trim();
            if !name.is_empty() {
                out.push(name.to_string());
            }
            rest = &rest[j..];
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(known: &[&str], pass: &[&str]) -> Policy {
        Policy {
            known: known.iter().map(|s| s.to_string()).collect(),
            pass: pass.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn credentials_go_and_what_tools_need_stays() {
        let p = Policy::default();
        for gone in [
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "GEMINI_API_KEY",
            "OPENROUTER_API_KEY",
            "GITHUB_TOKEN",
            "GH_TOKEN",
            "NPM_TOKEN",
            "HF_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "AWS_ACCESS_KEY_ID",
            "MY_SERVICE_TOKEN",
            "DB_PASSWORD",
            "STRIPE_SECRET",
            "CLIENT_SECRET_ID",
            "GITHUB_PAT",
            "PGPASSWORD",
            "MYSQL_PWD",
            "CODESMITH_SERVER_TOKEN",
            "CODESMITH_SERVER_PASSWORD",
            "my_lowercase_api_key",
        ] {
            assert!(p.removes(gone), "{gone}");
        }
        for kept in [
            "PATH",
            "HOME",
            "USER",
            "SHELL",
            "TERM",
            "TMPDIR",
            "LANG",
            "LC_ALL",
            "LC_CTYPE",
            "SSH_AUTH_SOCK",
            "HTTPS_PROXY",
            "no_proxy",
            "CARGO_HOME",
            "GOPATH",
            "XDG_CONFIG_HOME",
            "AWS_REGION",
            "AWS_PROFILE",
            "GOOGLE_VERTEX_PROJECT",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "EDITOR",
            "CI",
            "TOKENIZERS_PARALLELISM",
        ] {
            assert!(!p.removes(kept), "{kept}");
        }
    }

    #[test]
    fn catalog_key_variables_go() {
        let p = Policy::default();
        // Key variables of the bundled catalog whose names the patterns
        // would not catch go too.
        assert!(CATALOG.len() > 20);
        for v in CATALOG.iter() {
            assert!(p.removes(v), "{v}");
        }
        assert!(!CATALOG.contains("AWS_REGION"));
    }

    #[test]
    fn config_names_go_unless_tools_need_them() {
        let p = policy(&["MY_PROVIDER_CRED", "HOME"], &[]);
        assert!(p.removes("MY_PROVIDER_CRED"));
        assert!(!p.removes("HOME"));
    }

    #[test]
    fn passed_names_stay_but_internal_ones_never_do() {
        let p = policy(
            &["MY_PROVIDER_CRED"],
            &["GITHUB_TOKEN", "MY_PROVIDER_CRED", "CODESMITH_SERVER_TOKEN"],
        );
        assert!(!p.removes("GITHUB_TOKEN"));
        assert!(!p.removes("MY_PROVIDER_CRED"));
        assert!(p.removes("CODESMITH_SERVER_TOKEN"));
        assert!(p.removes("GH_TOKEN"));
    }

    #[test]
    fn scrub_map_and_command() {
        let p = policy(&[], &["NPM_TOKEN"]);
        let mut env: BTreeMap<String, String> = [
            ("PATH", "/bin"),
            ("OPENAI_API_KEY", "sk-fake"),
            ("NPM_TOKEN", "npm-fake"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        p.scrub_map(&mut env);
        assert_eq!(env.keys().collect::<Vec<_>>(), ["NPM_TOKEN", "PATH"]);
    }

    #[test]
    fn credential_refs_are_found_where_credentials_live() {
        let raw = r#"{
            // a comment
            "providers": {"p": {"settings": {"apiKey": "{env:MY_KEY_VAR}", "baseURL": "{env:MY_URL}"}}},
            "mcp": {"s": {"type": "remote", "headers": {"Authorization": "Bearer {env:MCP_BEARER}"},
                          "environment": {"X": "{env:MCP_ENV}"}, "url": "{env:MCP_URL}"}},
            "instructions": ["{env:HOME}/notes.md"],
            "lsp": {"x": {"env": {"AUTH": "{env:LSP_AUTH}", "token": "{env:LSP_TOKEN_VAR}"}}}
        }"#;
        assert_eq!(
            credential_refs(raw),
            [
                "LSP_TOKEN_VAR",
                "MCP_BEARER",
                "MCP_ENV",
                "MY_KEY_VAR",
                "MY_URL"
            ]
        );
    }
}
