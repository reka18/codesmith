//! Model favourites, recents and remembered variants, as OpenCode keeps
//! them in its `model.json`: the ten models used last (`model.cycle_recent`,
//! F2, steps through them), the favourites (ctrl+f in the model picker;
//! `model.cycle_favorite`), and each model's chosen variant. The model
//! picker lists favourites first, then recents. The file keeps fields it
//! does not know, and every update reads, changes and writes it under one
//! lock, so updates do not lose each other.

use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::Mutex;

/// The model each agent had in each session, until the UI exits, as
/// OpenCode keeps a session's agent and model drafts: switching agents
/// within a session brings back the model (and variant) the agent had
/// there; an agent not used there yet takes its configured model.
#[derive(Debug, Default)]
pub struct Selections(std::collections::HashMap<String, std::collections::HashMap<String, String>>);

impl Selections {
    /// `session` runs `agent` on `model` (`provider/model#variant`).
    pub fn record(&mut self, session: &str, agent: &str, model: &str) {
        self.0
            .entry(session.to_string())
            .or_default()
            .insert(agent.to_string(), model.to_string());
    }

    /// The model `agent` takes in `session`: the one it had there, else
    /// its `configured` one; `None` keeps the session's current model.
    pub fn for_agent(
        &self,
        session: &str,
        agent: &str,
        configured: Option<&str>,
    ) -> Option<String> {
        self.0
            .get(session)
            .and_then(|m| m.get(agent))
            .cloned()
            .or_else(|| configured.map(str::to_string))
    }
}

/// A model: its provider and ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    pub provider: String,
    pub model: String,
}

impl Model {
    pub fn new(provider: &str, model: &str) -> Model {
        Model {
            provider: provider.into(),
            model: model.into(),
        }
    }

    /// From a `provider/model` (or `provider/model#variant`) reference.
    pub fn parse(spec: &str) -> Option<Model> {
        let base = spec.split('#').next().unwrap_or(spec);
        let (p, m) = base.split_once('/')?;
        (!p.is_empty() && !m.is_empty()).then(|| Model::new(p, m))
    }

    /// `provider/model`, OpenCode's preference key.
    pub fn key(&self) -> String {
        format!("{}/{}", self.provider, self.model)
    }

    fn to_json(&self) -> Value {
        json!({"providerID": self.provider, "modelID": self.model})
    }
}

/// How many recent models are kept.
pub const RECENT: usize = 10;

/// The file's preferences, and the rest of its fields.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Prefs {
    pub rest: Map<String, Value>,
    pub recent: Vec<Model>,
    pub favorite: Vec<Model>,
    pub variant: Map<String, Value>,
}

fn models(v: Option<&Value>) -> Vec<Model> {
    v.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let p = m["providerID"].as_str().filter(|s| !s.is_empty())?;
            let id = m["modelID"].as_str().filter(|s| !s.is_empty())?;
            Some(Model::new(p, id))
        })
        .collect()
}

/// The preferences in a `model.json` document, repaired: malformed models
/// and variants dropped, unrelated fields kept.
pub fn decode(v: &Value) -> Prefs {
    let mut rest = v.as_object().cloned().unwrap_or_default();
    let recent = models(rest.get("recent"));
    let favorite = models(rest.get("favorite"));
    let variant = rest
        .get("variant")
        .and_then(Value::as_object)
        .map(|o| {
            o.iter()
                .filter(|(k, v)| !k.is_empty() && v.as_str().is_some_and(|s| !s.is_empty()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    for k in ["recent", "favorite", "variant"] {
        rest.remove(k);
    }
    Prefs {
        rest,
        recent,
        favorite,
        variant,
    }
}

/// The document for `p`.
pub fn encode(p: &Prefs) -> Value {
    let mut o = p.rest.clone();
    o.insert(
        "recent".into(),
        p.recent.iter().map(Model::to_json).collect(),
    );
    o.insert(
        "favorite".into(),
        p.favorite.iter().map(Model::to_json).collect(),
    );
    o.insert("variant".into(), Value::Object(p.variant.clone()));
    Value::Object(o)
}

/// `model` first, then the others once each, at most [`RECENT`].
pub fn recent_models(model: &Model, recent: &[Model]) -> Vec<Model> {
    let mut out: Vec<Model> = Vec::new();
    for m in std::iter::once(model).chain(recent) {
        if !out.iter().any(|o| o.key() == m.key()) {
            out.push(m.clone());
        }
    }
    out.truncate(RECENT);
    out
}

/// The favourites with `model` added first (`enabled`) or removed.
pub fn favorite_models(model: &Model, favorite: &[Model], enabled: bool) -> Vec<Model> {
    let rest = favorite.iter().filter(|m| m.key() != model.key()).cloned();
    if enabled {
        std::iter::once(model.clone()).chain(rest).collect()
    } else {
        rest.collect()
    }
}

/// The model after (`dir` 1) or before (-1) `current` in `list`, wrapping;
/// the first when `current` is not in it.
pub fn cycle(list: &[Model], current: Option<&Model>, dir: i32) -> Option<Model> {
    if list.is_empty() {
        return None;
    }
    let n = list.len() as i32;
    let at = current.and_then(|c| list.iter().position(|m| m.key() == c.key()));
    let next = match at {
        Some(i) => ((i as i32 + dir) % n + n) % n,
        None => 0,
    };
    Some(list[next as usize].clone())
}

/// The preferences file, updated whole under a lock.
pub struct Repository {
    path: PathBuf,
    lock: Mutex<()>,
}

impl Repository {
    pub fn new(path: PathBuf) -> Repository {
        Repository {
            path,
            lock: Mutex::new(()),
        }
    }

    /// `model.json` in CodeSmith's data folder.
    pub fn default_file() -> PathBuf {
        codesmith_core::paths::data_dir().join("model.json")
    }

    pub fn read(&self) -> Prefs {
        let text = std::fs::read_to_string(&self.path).unwrap_or_default();
        decode(&serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    fn update(&self, change: impl FnOnce(&mut Prefs)) -> Prefs {
        let _held = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut p = self.read();
        change(&mut p);
        let _ = codesmith_core::paths::write_private(&self.path, encode(&p).to_string().as_bytes());
        p
    }

    pub fn add_recent(&self, model: &Model) -> Prefs {
        self.update(|p| p.recent = recent_models(model, &p.recent))
    }

    /// Add `model` to the favourites, or take it off; whether it is one now.
    pub fn toggle_favorite(&self, model: &Model) -> bool {
        let mut on = false;
        self.update(|p| {
            on = !p.favorite.iter().any(|m| m.key() == model.key());
            p.favorite = favorite_models(model, &p.favorite, on);
        });
        on
    }

    /// Remember the variant chosen for `model` (`default` for none).
    pub fn save_variant(&self, model: &Model, variant: &str) -> Prefs {
        self.update(|p| {
            p.variant.insert(model.key(), Value::String(variant.into()));
        })
    }

    /// The variant remembered for `model` (none for `default`).
    pub fn resolve_variant(&self, model: &Model) -> Option<String> {
        self.read()
            .variant
            .get(&model.key())?
            .as_str()
            .filter(|v| *v != "default")
            .map(str::to_string)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // model-preference.test.ts: "repairs known model preferences and preserves unrelated fields"
    #[test]
    fn oc_repairs_known_model_preferences_and_preserves_unrelated_fields() {
        let p = decode(&json!({
            "unrelated": {"keep": true},
            "recent": [{"providerID": "openai", "modelID": "gpt-5", "ignored": true}, null],
            "favorite": "malformed",
            "variant": {"openai/gpt-5": "high", "default": "default", "invalid": 42},
        }));
        assert_eq!(
            encode(&p),
            json!({
                "unrelated": {"keep": true},
                "recent": [{"providerID": "openai", "modelID": "gpt-5"}],
                "favorite": [],
                "variant": {"openai/gpt-5": "high", "default": "default"},
            })
        );
    }

    // model-preference.test.ts: "atomically serializes model preference updates"
    #[test]
    fn oc_atomically_serializes_model_preference_updates() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("model.json");
        std::fs::write(
            &file,
            r#"{"unrelated": "keep", "favorite": [], "variant": {}}"#,
        )
        .unwrap();
        let repo = std::sync::Arc::new(Repository::new(file.clone()));
        let openai = Model::new("openai", "org/gpt-5");
        let anthropic = Model::new("anthropic", "claude/sonnet");
        let jobs: Vec<std::thread::JoinHandle<()>> = vec![
            {
                let (r, m) = (repo.clone(), openai.clone());
                std::thread::spawn(move || {
                    r.add_recent(&m);
                })
            },
            {
                let (r, m) = (repo.clone(), openai.clone());
                std::thread::spawn(move || {
                    r.save_variant(&m, "high");
                })
            },
            {
                let (r, m) = (repo.clone(), anthropic.clone());
                std::thread::spawn(move || {
                    r.save_variant(&m, "low");
                })
            },
        ];
        for j in jobs {
            j.join().unwrap();
        }
        let read = |f: &PathBuf| -> Value {
            serde_json::from_str(&std::fs::read_to_string(f).unwrap()).unwrap()
        };
        assert_eq!(
            read(&file),
            json!({
                "unrelated": "keep",
                "recent": [{"providerID": "openai", "modelID": "org/gpt-5"}],
                "favorite": [],
                "variant": {"openai/org/gpt-5": "high", "anthropic/claude/sonnet": "low"},
            })
        );
        repo.save_variant(&openai, "default");
        assert_eq!(repo.resolve_variant(&openai), None);
        assert_eq!(
            read(&file)["variant"],
            json!({"openai/org/gpt-5": "default", "anthropic/claude/sonnet": "low"})
        );
    }

    // local.test.ts: "moves a model to the front, deduplicates, and limits recents"
    #[test]
    fn oc_moves_a_model_to_the_front_deduplicates_and_limits_recents() {
        let recent: Vec<Model> = (0..12)
            .map(|i| Model::new("provider", &format!("model-{i}")))
            .collect();
        let mut want = vec![Model::new("provider", "model-5")];
        want.extend_from_slice(&recent[0..5]);
        want.extend_from_slice(&recent[6..10]);
        assert_eq!(
            recent_models(&Model::new("provider", "model-5"), &recent),
            want
        );
    }

    // local-selection.test.tsx: "cycles all recent models in a stable order
    // in both directions" (adapted: the cycle over the recents)
    #[test]
    fn oc_cycles_all_recent_models_in_a_stable_order_in_both_directions() {
        let recent: Vec<Model> = ["first", "second", "third"]
            .iter()
            .map(|m| Model::new("provider", m))
            .collect();
        let mut current = recent[0].clone();
        for id in ["second", "third", "first"] {
            current = cycle(&recent, Some(&current), 1).unwrap();
            assert_eq!(current.model, id);
        }
        for id in ["third", "second", "first"] {
            current = cycle(&recent, Some(&current), -1).unwrap();
            assert_eq!(current.model, id);
        }
    }

    #[test]
    fn favourites_toggle() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::new(dir.path().join("model.json"));
        let m = Model::parse("openai/gpt-5#high").unwrap();
        assert_eq!(m.key(), "openai/gpt-5");
        assert!(repo.toggle_favorite(&m));
        assert_eq!(repo.read().favorite, vec![m.clone()]);
        assert!(!repo.toggle_favorite(&m));
        assert!(repo.read().favorite.is_empty());
    }
}
