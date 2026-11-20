//! Where each built-in tool goes for a model (OpenCode's
//! `tool/model-policy.ts`): with `full_codemode` on, glob, grep and
//! webfetch are hidden for every model (Code Mode programs search and
//! fetch through `shell` and the `fetch` global), and strong models (GPT
//! and Claude) call read, write, edit, patch, shell and websearch from
//! inside `execute`; every other model, or an unresolved one, keeps them
//! as direct tools.

use std::collections::BTreeSet;

/// The built-ins a strong model calls from inside Code Mode.
pub const CODE_MODE_BUILTINS: [&str; 6] = ["read", "write", "edit", "patch", "shell", "websearch"];
/// The built-ins `full_codemode` hides for every model.
pub const FULL_CODE_MODE_HIDDEN: [&str; 3] = ["glob", "grep", "webfetch"];

/// What the placement looks at in a model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Model {
    pub provider_id: String,
    /// The model's name in config or the catalog.
    pub id: String,
    /// The id sent to the provider.
    pub model_id: String,
    pub family: Option<String>,
    /// The provider a gateway model really comes from, when known.
    pub canonical: Option<String>,
}

impl Model {
    pub fn new(provider_id: &str, id: &str) -> Model {
        Model {
            provider_id: provider_id.into(),
            id: id.into(),
            model_id: id.into(),
            ..Default::default()
        }
    }
}

/// The tools a model does not see at all, and the built-ins it keeps as
/// direct tools under `full_codemode` (empty without it: then every
/// built-in that is not hidden is direct, as always).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelTools {
    pub hidden: BTreeSet<String>,
    pub direct: BTreeSet<String>,
}

pub fn model_tools(model: Option<&Model>, websearch: bool, full_codemode: bool) -> ModelTools {
    let mut hidden = BTreeSet::new();
    if !websearch {
        hidden.insert("websearch".to_string());
    }
    if full_codemode {
        hidden.extend(FULL_CODE_MODE_HIDDEN.iter().map(|s| s.to_string()));
    }
    // GPT-5+ edits with patch; every other model with edit and write.
    if let Some(m) = model {
        if crate::tool::apply_patch::edits_with_patch(&m.id, &m.model_id) {
            hidden.insert("edit".into());
            hidden.insert("write".into());
        } else {
            hidden.insert("patch".into());
        }
    }
    let mut direct = BTreeSet::new();
    if full_codemode && !strong(model) {
        for name in CODE_MODE_BUILTINS {
            if !hidden.contains(name) {
                direct.insert(name.to_string());
            }
        }
    }
    ModelTools { hidden, direct }
}

/// GPT and Claude models, which write Code Mode programs well.
pub fn strong(model: Option<&Model>) -> bool {
    let Some(m) = model else {
        return false;
    };
    let (id, model_id) = (m.id.to_lowercase(), m.model_id.to_lowercase());
    let gpt = id.contains("gpt") || model_id.contains("gpt");
    let anthropic = |p: &str| p == "anthropic" || p == "claude-agent";
    gpt || anthropic(&m.provider_id)
        || m.canonical.as_deref().is_some_and(anthropic)
        || id.contains("claude")
        || model_id.contains("claude")
        || m.family
            .as_deref()
            .is_some_and(|f| f.to_lowercase().contains("claude"))
}
