//! What the sign-in picker (`/login`, `codesmith auth login` without a
//! provider) shows: each provider a login can be saved for, whether one
//! is in place and where it comes from, and the ways to sign in, each
//! with what it asks for. The logins themselves are the provider modules'
//! (and `codesmith auth login`'s); nothing here signs in.
//!
//! No state carries a key, not even masked: the picker says where a login
//! comes from, never what it is.

use super::{
    azure, bedrock, chatgpt, claude_agent, copilot, digitalocean, gitlab, poe, sap, snowflake,
    vertex, xai, Env, Field,
};
use crate::config::{Config, ProviderConfig};
use std::path::Path;

/// One way to sign in to a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A browser OAuth login. `paste`: a redirect pasted back completes
    /// it, for a browser on another machine; otherwise only the loopback
    /// callback does.
    Browser { paste: bool },
    /// A device-code login: a code shown here, approved on any device.
    Device,
    /// An API key, typed or pasted (masked), with the provider's other
    /// answers.
    Key,
    /// Claude Code's own login (`claude auth login`): checked, never
    /// stored.
    ClaudeCode,
    /// An AWS profile (Amazon Bedrock).
    AwsProfile,
    /// A Google Cloud project with Application Default Credentials.
    VertexProject,
    /// Microsoft Entra ID tokens from the Azure CLI.
    AzureCli,
}

/// A way to sign in, as offered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Method {
    pub kind: Kind,
    pub label: String,
    /// What it asks beside a key (none of them secret).
    pub fields: Vec<Field>,
}

/// Where a provider's login stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// A login saved in CodeSmith's auth store, described (no key).
    Saved(String),
    /// A key from this environment variable.
    Env(String),
    /// A key in the configuration.
    Config,
    /// Usable without a saved login: ambient credentials, or Claude Code
    /// found on `PATH`.
    Ambient(String),
    /// Nothing yet.
    None,
}

impl State {
    pub fn signed_in(&self) -> bool {
        !matches!(self, State::None)
    }

    /// The picker's short description.
    pub fn describe(&self) -> String {
        match self {
            State::Saved(d) => format!("signed in · {d}"),
            State::Env(v) => format!("key from ${v}"),
            State::Config => "key in config".into(),
            State::Ambient(d) => d.clone(),
            State::None => "not signed in".into(),
        }
    }

    /// Whether there is a saved login to sign out of.
    pub fn saved(&self) -> bool {
        matches!(self, State::Saved(_))
    }
}

/// A provider in the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub id: String,
    pub name: String,
    pub state: State,
    pub methods: Vec<Method>,
    /// The page where the provider's API keys are made, when well known.
    pub key_page: Option<&'static str>,
    /// What to do instead, for a provider no method here signs in to.
    pub note: Option<String>,
}

/// API-key pages of providers whose page is well known and stable. Others
/// get a plain key prompt.
pub const KEY_PAGES: &[(&str, &str)] = &[
    ("anthropic", "https://console.anthropic.com/settings/keys"),
    ("openai", "https://platform.openai.com/api-keys"),
    ("google", "https://aistudio.google.com/app/apikey"),
    ("openrouter", "https://openrouter.ai/keys"),
    ("groq", "https://console.groq.com/keys"),
    ("mistral", "https://console.mistral.ai/api-keys"),
    ("deepseek", "https://platform.deepseek.com/api_keys"),
    ("fireworks", "https://fireworks.ai/account/api-keys"),
    ("poe", "https://poe.com/api_key"),
];

pub fn key_page(id: &str) -> Option<&'static str> {
    KEY_PAGES.iter().find(|(p, _)| *p == id).map(|(_, u)| *u)
}

/// The label of the Claude Code entry.
pub const CLAUDE_CODE_LABEL: &str = "Claude Pro/Max: use your Claude Code login";

fn field(
    key: &'static str,
    title: &'static str,
    placeholder: &'static str,
    required: bool,
) -> Field {
    Field {
        key,
        title,
        placeholder,
        required,
    }
}

fn method(kind: Kind, label: &str, fields: Vec<Field>) -> Method {
    Method {
        kind,
        label: label.to_string(),
        fields,
    }
}

/// The ways to sign in to `id`, first the one a pick runs by default.
pub fn methods(id: &str, p: &ProviderConfig, env: &Env) -> Vec<Method> {
    let key = || {
        let fields = super::key_form(id, &p.settings, env)
            .map(|f| f.fields)
            .unwrap_or_default();
        method(Kind::Key, "Paste an API key", fields)
    };
    match id {
        "anthropic" => vec![key(), method(Kind::ClaudeCode, CLAUDE_CODE_LABEL, vec![])],
        claude_agent::ID => vec![method(Kind::ClaudeCode, CLAUDE_CODE_LABEL, vec![])],
        chatgpt::ID => vec![
            method(Kind::Browser { paste: true }, chatgpt::LABEL, vec![]),
            key(),
        ],
        xai::ID => vec![method(Kind::Device, xai::LABEL, vec![]), key()],
        poe::ID => vec![
            method(Kind::Browser { paste: true }, poe::LABEL, vec![]),
            key(),
        ],
        gitlab::ID => vec![method(
            Kind::Browser { paste: true },
            gitlab::LABEL,
            vec![field(
                "instanceUrl",
                "GitLab instance URL",
                gitlab::DEFAULT_INSTANCE,
                false,
            )],
        )],
        digitalocean::ID => vec![
            method(Kind::Browser { paste: false }, digitalocean::LABEL, vec![]),
            key(),
        ],
        snowflake::ID => {
            let known = env("SNOWFLAKE_ACCOUNT").is_some()
                || super::set(&p.settings.plugin.account).is_some();
            vec![
                method(
                    Kind::Browser { paste: true },
                    snowflake::LABEL,
                    vec![
                        field("account", "Snowflake account", "myorg-myaccount", !known),
                        field("role", "Snowflake role", "optional", false),
                    ],
                ),
                key(),
            ]
        }
        copilot::ID => vec![method(
            Kind::Device,
            "Login with GitHub (device code)",
            vec![],
        )],
        bedrock::ID => vec![
            method(
                Kind::AwsProfile,
                "Use an AWS profile",
                vec![field("profile", "AWS profile", "default", true)],
            ),
            key(),
        ],
        vertex::ID => vec![method(
            Kind::VertexProject,
            "Use Application Default Credentials",
            vec![
                field("project", "Google Cloud project", "my-project", true),
                field("location", "Location", "global", false),
            ],
        )],
        azure::ID => {
            let mut out = vec![key()];
            if azure::find_cli(env).is_some() {
                out.push(method(Kind::AzureCli, "Use the Azure CLI (az login)", {
                    azure::key_form(&p.settings)
                        .fields
                        .into_iter()
                        .map(|f| Field {
                            required: false,
                            ..f
                        })
                        .collect()
                }));
            }
            out
        }
        sap::ID => vec![],
        _ => vec![key()],
    }
}

/// What `id` needs instead of a login here.
fn note(id: &str) -> Option<String> {
    match id {
        sap::ID => Some(format!(
            "SAP AI Core takes a service key: set {} or settings.serviceKey",
            sap::KEY_VAR
        )),
        _ => None,
    }
}

/// Where `id`'s login stands, from config, the environment and the saved
/// entry in `file` (as `codesmith auth list` reads them).
pub fn state(id: &str, p: &ProviderConfig, file: &Path, env: &Env) -> State {
    if id == claude_agent::ID {
        return if claude_agent::available(env) {
            if claude_agent::catalog().models.is_empty() {
                State::Ambient("Claude Code found · not checked yet".into())
            } else if !claude_agent::signed_in_at(&claude_agent::storage()) {
                State::Ambient("Claude Code found · not logged in (claude auth login)".into())
            } else {
                // Nothing is saved here to sign out of: Claude Code
                // keeps its own login (`claude auth logout`).
                State::Ambient("signed in · Claude Code's own login".into())
            }
        } else {
            State::None
        };
    }
    if super::set(&p.settings.api_key).is_some() {
        return State::Config;
    }
    if let Some(v) = p.env.iter().find(|v| env(v).is_some()) {
        return State::Env(v.clone());
    }
    let login = super::login_id(id);
    let stored = crate::auth::entry_in(file, login);
    if crate::auth::key_in(file, login).is_some() {
        return State::Saved("saved key".into());
    }
    if let Some(d) = super::describe_login(id, stored.as_ref(), &p.settings.plugin) {
        return if stored.is_some() {
            State::Saved(d)
        } else {
            State::Ambient(d)
        };
    }
    match stored {
        Some(s) if s["type"] == "oauth" => State::Saved("saved login".into()),
        Some(_) => State::Saved("saved".into()),
        None => State::None,
    }
}

/// Providers that need nothing to sign in (local servers).
fn needs_nothing(id: &str, p: &ProviderConfig) -> bool {
    let url = p.settings.base_url.as_deref().unwrap_or("");
    p.env.is_empty() && methods_known(id).is_none() && (url.is_empty() || p.is_local(url))
}

fn methods_known(id: &str) -> Option<()> {
    [
        "anthropic",
        claude_agent::ID,
        chatgpt::ID,
        xai::ID,
        poe::ID,
        gitlab::ID,
        digitalocean::ID,
        snowflake::ID,
        copilot::ID,
        bedrock::ID,
        vertex::ID,
        azure::ID,
        sap::ID,
    ]
    .contains(&id)
    .then_some(())
}

/// The picker's providers: the built-in ones, the provider plugins,
/// GitLab, then configured and saved ones, each once; without local
/// servers (nothing to sign in to) or Claude on Vertex (Vertex's login).
pub fn choices(config: &Config, file: &Path, env: &Env) -> Vec<Choice> {
    let mut ids: Vec<String> = crate::config::BUILTIN
        .iter()
        .map(|b| b.0.to_string())
        .collect();
    for (id, _, _) in super::PROVIDERS {
        ids.push(id.to_string());
    }
    ids.push(gitlab::ID.to_string());
    ids.extend(config.providers.keys().cloned());
    ids.extend(crate::auth::providers_in(file));
    let mut seen = std::collections::HashSet::new();
    ids.retain(|id| seen.insert(id.clone()));
    ids.into_iter()
        .filter(|id| id != vertex::ANTHROPIC_ID)
        .filter_map(|id| {
            let p = config.provider(&id).unwrap_or_default();
            let st = state(&id, &p, file, env);
            if needs_nothing(&id, &p) && !st.saved() {
                return None;
            }
            let name = match id.as_str() {
                gitlab::ID => "GitLab".to_string(),
                claude_agent::ID => "Claude Pro/Max (Claude Code)".to_string(),
                _ => p.name.clone().unwrap_or_else(|| id.clone()),
            };
            Some(Choice {
                methods: methods(&id, &p, env),
                key_page: key_page(&id),
                note: note(&id),
                state: st,
                name,
                id,
            })
        })
        .collect()
}

/// Why a browser or device login of `id` cannot start without a client id
/// typed in: the provider's own explanation of where to register one and
/// the redirect URL to give it. `None` when one is configured, or the
/// login needs none.
pub fn client_id_missing(id: &str, p: &ProviderConfig, file: &Path, env: &Env) -> Option<String> {
    let s = &p.settings.plugin;
    let r = match id {
        chatgpt::ID => {
            let saved = crate::auth::entry_in(file, id)
                .filter(|e| chatgpt::is_login(Some(e)))
                .and_then(|e| e["metadata"]["clientID"].as_str().map(str::to_string));
            chatgpt::client_id(saved.as_deref(), None, false, s, env).map(|_| ())
        }
        poe::ID => poe::client_id(None, s, env).map(|_| ()),
        gitlab::ID => {
            let instance = gitlab::default_instance(env);
            gitlab::client_id(None, s, env, &instance).map(|_| ())
        }
        digitalocean::ID => digitalocean::client_id(None, s, env).map(|_| ()),
        xai::ID => xai::client_id(None, s, env).map(|_| ()),
        copilot::ID => copilot::client_id(None, s, env).map(|_| ()),
        _ => Ok(()),
    };
    r.err().map(|e| format!("{e:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn env(pairs: &[(&str, &str)]) -> Env {
        let m: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Arc::new(move |k| m.get(k).cloned())
    }

    fn find<'a>(c: &'a [Choice], id: &str) -> &'a Choice {
        c.iter()
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("{id}"))
    }

    #[test]
    fn states_come_from_the_store_the_environment_and_config() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("auth.json");
        crate::auth::set_in(&file, "groq", "gsk-secret-123456").unwrap();
        crate::auth::set_entry_in(
            &file,
            "poe",
            serde_json::json!({"type": "oauth", "access": "poe-secret", "refresh": "", "expires": poe::NEVER}),
        )
        .unwrap();
        let config: Config = serde_json::from_value(serde_json::json!({
            "providers": {"mistral": {"settings": {"apiKey": "cfg-secret"}}}
        }))
        .unwrap();
        let c = choices(&config, &file, &env(&[("ANTHROPIC_API_KEY", "env-secret")]));
        assert_eq!(find(&c, "groq").state, State::Saved("saved key".into()));
        assert_eq!(
            find(&c, "poe").state,
            State::Saved("saved Poe login".into())
        );
        assert_eq!(
            find(&c, "anthropic").state,
            State::Env("ANTHROPIC_API_KEY".into())
        );
        assert_eq!(find(&c, "mistral").state, State::Config);
        assert_eq!(find(&c, "fireworks").state, State::None);
        assert!(!find(&c, "fireworks").state.signed_in());
        // No state or description carries a key.
        let all = format!("{c:?}");
        for secret in ["gsk-secret", "poe-secret", "cfg-secret", "env-secret"] {
            assert!(!all.contains(secret), "{secret} in {all}");
        }
        // Local servers and Claude on Vertex are left out.
        for id in ["ollama", "lmstudio", "google-vertex-anthropic"] {
            assert!(c.iter().all(|x| x.id != id), "{id}");
        }
    }

    #[test]
    fn each_provider_offers_its_own_ways_in() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("auth.json");
        let c = choices(&Config::default(), &file, &env(&[]));
        let kinds =
            |id: &str| -> Vec<Kind> { find(&c, id).methods.iter().map(|m| m.kind).collect() };
        assert_eq!(kinds("openai"), [Kind::Browser { paste: true }, Kind::Key]);
        assert_eq!(kinds("anthropic"), [Kind::Key, Kind::ClaudeCode]);
        assert_eq!(kinds("claude-agent"), [Kind::ClaudeCode]);
        assert_eq!(kinds("xai"), [Kind::Device, Kind::Key]);
        assert_eq!(kinds("github-copilot"), [Kind::Device]);
        assert_eq!(
            kinds("digitalocean"),
            [Kind::Browser { paste: false }, Kind::Key]
        );
        assert_eq!(kinds("gitlab"), [Kind::Browser { paste: true }]);
        assert_eq!(kinds("groq"), [Kind::Key]);
        assert!(kinds("sap-ai-core").is_empty());
        assert!(find(&c, "sap-ai-core").note.is_some());
        assert_eq!(
            find(&c, "openai").key_page,
            Some("https://platform.openai.com/api-keys")
        );
        assert_eq!(find(&c, "cerebras").key_page, None);
        // A key form's other answers come with the key method.
        let cf = &find(&c, "cloudflare-ai-gateway").methods[0];
        assert!(cf.fields.iter().any(|f| f.key == "accountId"), "{cf:?}");
    }

    #[test]
    fn a_missing_client_id_explains_where_to_register_one() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("auth.json");
        let p = ProviderConfig::default();
        let gl = client_id_missing("gitlab", &p, &file, &env(&[])).unwrap();
        assert!(
            gl.contains("redirect URI http://127.0.0.1:8080/callback"),
            "{gl}"
        );
        let poe = client_id_missing("poe", &p, &file, &env(&[])).unwrap();
        assert!(poe.contains("ships none"), "{poe}");
        assert_eq!(
            client_id_missing("poe", &p, &file, &env(&[(poe::CLIENT_ID_VAR, "mine")])),
            None
        );
        assert_eq!(client_id_missing("groq", &p, &file, &env(&[])), None);
        assert_eq!(
            client_id_missing("snowflake-cortex", &p, &file, &env(&[])),
            None
        );
    }
}
