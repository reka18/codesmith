//! Forgiving model specs: what someone types for a model made into the
//! spec of a model they can run, before anything is sent.
//!
//! OpenCode's `provider/model` and `provider/model#variant` stay the
//! syntax (the first `/` splits, the first `#` after it starts the
//! variant). On top of it:
//! - whitespace around the provider, the model and the variant is dropped
//!   (`anthropic/ opus` is `anthropic/opus`);
//! - a provider is found ignoring case;
//! - a model is found by its id, ignoring case and `-`/`.`/`_`/space
//!   differences, else as a part of an id: the newest model that contains
//!   it (`opus` is the newest `claude-*opus*`). Equally new matches that
//!   are not one model's aliases are ambiguous, and refused;
//! - a model name alone (`opus`) is looked for among every model the user
//!   can run;
//! - a spec naming no known model is refused with the closest ones (as
//!   OpenCode's `ModelNotFoundError` suggests), except on a provider with
//!   no model list or a server on this machine, which may serve models no
//!   list knows yet;
//! - `anthropic/…` without an Anthropic key, while Claude Code is signed
//!   in, runs the same model through Claude Code (`claude-agent/…`) when
//!   there is exactly one, and is refused with the fix otherwise.

use crate::config::{split_variant, Config};
use crate::providers::claude_agent;
use anyhow::{anyhow, Result};
use codesmith_ai::claude_agent::catalog::{self as claude_catalog, Catalog as ClaudeCatalog};

/// A model a spec may name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub provider: String,
    pub id: String,
    /// Release time in milliseconds, 0 when unknown.
    pub released: i64,
}

impl Candidate {
    pub fn new(provider: &str, id: &str, released: i64) -> Candidate {
        Candidate {
            provider: provider.into(),
            id: id.into(),
            released,
        }
    }

    pub fn spec(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }
}

/// What a query matched, by index into the candidates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Match {
    One(usize),
    /// Equally good matches that are different models.
    Ambiguous(Vec<usize>),
    None,
}

/// An id for loose comparison: lowercase, with runs of whitespace, `-`,
/// `.` and `_` made one `-`, and none at the ends.
pub fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars().flat_map(char::to_lowercase) {
        if c.is_whitespace() || matches!(c, '-' | '.' | '_') {
            if !out.is_empty() && !out.ends_with('-') {
                out.push('-');
            }
        } else {
            out.push(c);
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// The model `query` names among `candidates`: the one with that id
/// (exactly, then ignoring case, then [`normalize`]d), else the newest
/// whose id contains it.
pub fn pick(query: &str, candidates: &[Candidate]) -> Match {
    let q = query.trim();
    if q.is_empty() {
        return Match::None;
    }
    let exact: Vec<usize> = (0..candidates.len())
        .filter(|&i| candidates[i].id == q)
        .collect();
    if !exact.is_empty() {
        return one_or_ambiguous(exact);
    }
    let lower = q.to_lowercase();
    let folded: Vec<usize> = (0..candidates.len())
        .filter(|&i| candidates[i].id.to_lowercase() == lower)
        .collect();
    if !folded.is_empty() {
        return one_or_ambiguous(folded);
    }
    let nq = normalize(q);
    if nq.is_empty() {
        return Match::None;
    }
    let same: Vec<usize> = (0..candidates.len())
        .filter(|&i| normalize(&candidates[i].id) == nq)
        .collect();
    if !same.is_empty() {
        return newest(same, candidates);
    }
    let loose: Vec<usize> = (0..candidates.len())
        .filter(|&i| normalize(&candidates[i].id).contains(&nq))
        .collect();
    if loose.is_empty() {
        return Match::None;
    }
    newest(loose, candidates)
}

fn one_or_ambiguous(found: Vec<usize>) -> Match {
    match found.as_slice() {
        [i] => Match::One(*i),
        _ => Match::Ambiguous(found),
    }
}

/// The newest of `found`; among equally new ones, the alias the others
/// extend (`claude-opus-4-1` over `claude-opus-4-1-20250805`, `opus` over
/// `opus[1m]`), else they are ambiguous.
fn newest(found: Vec<usize>, candidates: &[Candidate]) -> Match {
    let Some(top) = found.iter().map(|&i| candidates[i].released).max() else {
        return Match::None;
    };
    let mut tied: Vec<usize> = found
        .into_iter()
        .filter(|&i| candidates[i].released == top)
        .collect();
    if tied.len() == 1 {
        return Match::One(tied[0]);
    }
    tied.sort_by(|&a, &b| {
        let (a, b) = (&candidates[a], &candidates[b]);
        a.id.len()
            .cmp(&b.id.len())
            .then_with(|| a.spec().cmp(&b.spec()))
    });
    let base = &candidates[tied[0]];
    let aliases = tied[1..].iter().all(|&i| {
        let c = &candidates[i];
        c.provider == base.provider && c.id.starts_with(&base.id)
    });
    if aliases {
        Match::One(tied[0])
    } else {
        tied.sort_by_key(|&i| candidates[i].spec());
        Match::Ambiguous(tied)
    }
}

fn levenshtein(a: &[char], b: &[char]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

/// How many edits make `q` a part of `id` (the edit distance to its
/// closest substring: Sellers' algorithm).
fn window_distance(q: &[char], id: &[char]) -> usize {
    // Row `i`: the cost of the first `i` characters of `q` ending at each
    // position of `id`; starting anywhere in `id` is free.
    let mut prev = vec![0usize; id.len() + 1];
    for (i, cq) in q.iter().enumerate() {
        let mut cur = vec![i + 1; id.len() + 1];
        for (j, ci) in id.iter().enumerate() {
            let sub = prev[j] + usize::from(cq != ci);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    prev.into_iter().min().unwrap_or(q.len())
}

/// The candidates closest to `query` (a typo away from an id or a part of
/// one), at most `n`, closest first.
pub fn closest(query: &str, candidates: &[Candidate], n: usize) -> Vec<usize> {
    let q: Vec<char> = normalize(query).chars().collect();
    if q.is_empty() {
        return vec![];
    }
    let limit = (q.len() / 3).max(1);
    let mut scored: Vec<(usize, usize, usize)> = candidates
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            let id: Vec<char> = normalize(&c.id).chars().collect();
            let near = window_distance(&q, &id);
            (near <= limit).then(|| (near, levenshtein(&q, &id), i))
        })
        .collect();
    scored.sort_by(|a, b| {
        (a.0, a.1)
            .cmp(&(b.0, b.1))
            .then_with(|| candidates[a.2].spec().cmp(&candidates[b.2].spec()))
    });
    scored.into_iter().take(n).map(|s| s.2).collect()
}

fn listing(idx: &[usize], candidates: &[Candidate]) -> String {
    idx.iter()
        .map(|&i| candidates[i].spec())
        .collect::<Vec<_>>()
        .join(", ")
}

/// What can run besides the configuration, read once per resolution.
#[derive(Debug, Clone, Default)]
pub struct Access {
    /// Claude Code's saved models, when it is signed in (and not disabled).
    pub claude_code: Option<ClaudeCatalog>,
    /// Whether the `anthropic` provider has a way in of its own: a key (in
    /// config, its variable or saved), or a configured URL (a proxy).
    pub anthropic_ready: bool,
}

impl Access {
    /// As things stand on this machine.
    pub fn current(config: &Config) -> Access {
        let env = crate::providers::process_env();
        let claude_code = (!config.provider_disabled(claude_agent::ID)
            && claude_agent::signed_in(&env))
        .then(claude_agent::catalog);
        let p = config.provider("anthropic").unwrap_or_default();
        let proxied = config
            .providers
            .get("anthropic")
            .and_then(|p| p.settings.base_url.as_deref())
            .is_some_and(|u| !u.trim().is_empty());
        let keyed =
            crate::providers::signin::state("anthropic", &p, &crate::paths::auth_file(), &env)
                .signed_in();
        Access {
            claude_code,
            anthropic_ready: keyed || proxied,
        }
    }
}

/// A spec made canonical.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// `provider/model` or `provider/model#variant`.
    pub spec: String,
    /// What was changed, when more than spacing: shown to the user.
    pub note: Option<String>,
}

fn with_variant(spec: String, variant: Option<&str>) -> String {
    match variant {
        Some(v) => format!("{spec}#{v}"),
        None => spec,
    }
}

/// The release time of `provider/id` in the catalog (by the configured
/// model's provider id first), 0 when unknown.
fn released(config: &Config, provider: &str, id: &str) -> i64 {
    let Some(c) = config.models_dev.as_ref().map(|c| c.catalog()) else {
        return 0;
    };
    let configured = config
        .providers
        .get(provider)
        .and_then(|p| p.models.get(id))
        .and_then(|m| m.id.clone());
    configured
        .and_then(|cid| c.model(provider, &cid).map(|m| m.time.released))
        .or_else(|| c.model(provider, id).map(|m| m.time.released))
        .unwrap_or(0)
}

/// Claude Code's models as candidates, dated by the models they resolve to.
pub fn claude_candidates(config: &Config, saved: &ClaudeCatalog) -> Vec<Candidate> {
    let catalog = config.models_dev.as_ref().map(|c| c.catalog());
    saved
        .models
        .iter()
        .map(|m| {
            let released = catalog
                .as_ref()
                .and_then(|c| claude_agent::catalog_model(saved, &m.id, c))
                .map_or(0, |cm| cm.time.released);
            Candidate::new(claude_agent::ID, &m.id, released)
        })
        .collect()
}

/// A provider's chat models as candidates.
fn provider_candidates(config: &Config, pid: &str, access: &Access) -> Vec<Candidate> {
    if pid == claude_agent::ID {
        let saved = access
            .claude_code
            .clone()
            .unwrap_or_else(claude_agent::catalog);
        let mut out = claude_candidates(config, &saved);
        // Configured ones too.
        for id in config.list_models(pid, &["chat"]) {
            if !out.iter().any(|c| c.id == id) {
                out.push(Candidate::new(pid, &id, 0));
            }
        }
        return out;
    }
    config
        .list_models(pid, &["chat"])
        .into_iter()
        .map(|id| {
            let r = released(config, pid, &id);
            Candidate::new(pid, &id, r)
        })
        .collect()
}

/// Every model the user can run, for a model named without its provider.
fn all_candidates(config: &Config, access: &Access) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = crate::plugin::codesmith::available(config, "chat")
        .into_iter()
        .filter(|r| r.provider_id != claude_agent::ID)
        .map(|r| Candidate::new(&r.provider_id, &r.id, r.released))
        .collect();
    if let Some(saved) = &access.claude_code {
        out.extend(claude_candidates(config, saved));
    }
    out
}

/// Every provider id CodeSmith knows of.
pub fn known_providers(config: &Config) -> Vec<String> {
    let mut ids: Vec<String> = crate::config::BUILTIN
        .iter()
        .map(|b| b.0.to_string())
        .collect();
    ids.extend(crate::providers::PROVIDERS.iter().map(|p| p.0.to_string()));
    ids.extend(config.providers.keys().cloned());
    ids.extend(crate::providers::local::providers(config));
    if let Some(c) = &config.models_dev {
        ids.extend(c.catalog().ids().map(str::to_string));
    }
    ids.sort();
    ids.dedup();
    ids
}

/// The provider `typed` names: itself, else one known ignoring case.
fn resolve_provider(config: &Config, typed: &str) -> Result<String> {
    if config.provider(typed).is_some() {
        return Ok(typed.to_string());
    }
    let known = known_providers(config);
    if let Some(p) = known.iter().find(|p| p.eq_ignore_ascii_case(typed)) {
        if config.provider(p).is_some() {
            return Ok(p.clone());
        }
    }
    // Compared by id: the provider id goes there.
    let by_id: Vec<Candidate> = known.iter().map(|p| Candidate::new("", p, 0)).collect();
    let near: Vec<String> = closest(typed, &by_id, 3)
        .into_iter()
        .map(|i| by_id[i].id.clone())
        .collect();
    Err(match near.as_slice() {
        [] => anyhow!(
            "unknown provider {typed:?}; add it under \"providers\" in codesmith.json (`codesmith models` lists the known ones)"
        ),
        n => anyhow!(
            "unknown provider {typed:?}; did you mean: {}?",
            n.join(", ")
        ),
    })
}

/// Whether a provider's model list cannot be trusted to be complete: it
/// has none, or it is a server on this machine (it serves what it loads).
fn open_list(config: &Config, pid: &str, candidates: &[Candidate]) -> bool {
    if candidates.is_empty() {
        return true;
    }
    if crate::providers::local::providers(config)
        .iter()
        .any(|p| p == pid)
    {
        return true;
    }
    config.provider(pid).is_some_and(|p| {
        p.settings
            .base_url
            .as_deref()
            .is_some_and(|u| !u.is_empty() && p.is_local(u))
    })
}

/// [`canonical`] as things stand on this machine.
pub fn resolve(config: &Config, spec: &str) -> Result<Resolved> {
    canonical(config, spec, &Access::current(config))
}

/// The canonical spec of the model `spec` names, or why there is none.
pub fn canonical(config: &Config, spec: &str, access: &Access) -> Result<Resolved> {
    let typed = spec.trim();
    if typed.is_empty() {
        return Err(anyhow!(
            "no model given: name one as provider/model (`codesmith models` lists them)"
        ));
    }
    let (base, variant) = split_variant(typed);
    let variant = variant.map(str::trim).filter(|v| !v.is_empty());
    let (provider, model) = match base.split_once('/') {
        Some((p, m)) if !p.trim().is_empty() => (Some(p.trim()), m.trim()),
        // `/opus`: no provider named.
        Some((_, m)) => (None, m.trim()),
        None => (None, base.trim()),
    };
    match provider {
        Some(p) => with_provider(config, p, model, variant, access),
        None => bare(config, model, variant, access),
    }
}

fn with_provider(
    config: &Config,
    typed_provider: &str,
    model: &str,
    variant: Option<&str>,
    access: &Access,
) -> Result<Resolved> {
    let pid = resolve_provider(config, typed_provider)?;
    if config.provider_disabled(&pid) {
        return Err(anyhow!(
            "provider {pid:?} is disabled (\"disabled_providers\" in codesmith.json)"
        ));
    }
    if pid == "anthropic" && !access.anthropic_ready && !model.is_empty() {
        if let Some(saved) = &access.claude_code {
            return through_claude_code(config, model, variant, saved);
        }
    }
    let candidates = provider_candidates(config, &pid, access);
    if model.is_empty() {
        let newest = match newest((0..candidates.len()).collect(), &candidates) {
            Match::One(i) => format!("; e.g. {}", candidates[i].spec()),
            _ => String::new(),
        };
        return Err(anyhow!(
            "no model given for provider {pid:?}: name one as {pid}/<model>{newest} (`codesmith models {pid}` lists them)"
        ));
    }
    let open = open_list(config, &pid, &candidates);
    let typed_spec = format!("{typed_provider}/{model}");
    match pick(model, &candidates) {
        // A server's own list matches only by id: it may serve another.
        Match::One(i) if !open || candidates[i].id.eq_ignore_ascii_case(model) => {
            let spec = candidates[i].spec();
            let note = (!candidates[i].id.eq_ignore_ascii_case(model))
                .then(|| format!("{typed_spec} is {spec}"));
            Ok(Resolved {
                spec: with_variant(spec, variant),
                note,
            })
        }
        Match::Ambiguous(v) if !open => Err(anyhow!(
            "model {model:?} matches more than one {pid} model; name one of: {}",
            listing(&v, &candidates)
        )),
        _ if open => Ok(Resolved {
            spec: with_variant(format!("{pid}/{model}"), variant),
            note: None,
        }),
        _ => Err(unknown(model, Some(&pid), &candidates)),
    }
}

fn unknown(model: &str, pid: Option<&str>, candidates: &[Candidate]) -> anyhow::Error {
    let near = closest(model, candidates, 3);
    let what = match pid {
        Some(p) => format!("unknown model {model:?} for provider {p:?}"),
        None => format!("unknown model {model:?}"),
    };
    let list = match pid {
        Some(p) => format!(
            "`codesmith models {p}` lists them (a model its list lacks can be added under providers.{p}.models in codesmith.json)"
        ),
        None => "`codesmith models` lists them".to_string(),
    };
    if near.is_empty() {
        anyhow!("{what}; {list}")
    } else {
        anyhow!("{what}; did you mean: {}?", listing(&near, candidates))
    }
}

fn bare(config: &Config, model: &str, variant: Option<&str>, access: &Access) -> Result<Resolved> {
    if model.is_empty() {
        return Err(anyhow!(
            "no model given: name one as provider/model (`codesmith models` lists them)"
        ));
    }
    let candidates = all_candidates(config, access);
    if candidates.is_empty() {
        return Err(anyhow!(
            "no models available for {model:?}: add a provider key (`codesmith auth login`) or name the model as provider/model"
        ));
    }
    match pick(model, &candidates) {
        Match::One(i) => {
            let spec = candidates[i].spec();
            Ok(Resolved {
                note: Some(format!("{model} is {spec}")),
                spec: with_variant(spec, variant),
            })
        }
        Match::Ambiguous(v) => Err(anyhow!(
            "model {model:?} matches more than one model; name one of: {}",
            listing(&v, &candidates)
        )),
        Match::None => {
            if known_providers(config).iter().any(|p| p == model) {
                let p = model;
                return Err(anyhow!(
                    "{p:?} is a provider: name one of its models as {p}/<model> (`codesmith models {p}` lists them)"
                ));
            }
            Err(unknown(model, None, &candidates))
        }
    }
}

/// A resolved model id without a trailing `-YYYYMMDD` date or `[1m]`.
fn undated(id: &str) -> &str {
    let id = id.trim_end_matches("[1m]");
    match id.rsplit_once('-') {
        Some((head, date)) if date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) => head,
        _ => id,
    }
}

/// The Claude family a model id names, if any.
fn family(id: &str) -> Option<&'static str> {
    let id = id.to_lowercase();
    ["opus", "sonnet", "haiku"]
        .into_iter()
        .find(|f| id.contains(f))
}

/// The Claude Code model for `anthropic/<model>`, when there is exactly
/// one; else the error saying what to run instead.
fn through_claude_code(
    config: &Config,
    model: &str,
    variant: Option<&str>,
    saved: &ClaudeCatalog,
) -> Result<Resolved> {
    let claude = claude_candidates(config, saved);
    let typed = format!("anthropic/{model}");
    let routed = |i: usize, why: &str| -> Resolved {
        let m = &saved.models[i];
        let kept = variant.filter(|v| m.efforts.iter().any(|e| e == v));
        let spec = with_variant(claude[i].spec(), kept);
        let dropped = match (variant, kept) {
            (Some(v), None) => format!(" (Claude Code has no {v:?} effort for it)"),
            _ => String::new(),
        };
        Resolved {
            note: Some(format!(
                "No Anthropic API key: running {typed} as {spec} through your Claude Code login{why}{dropped}"
            )),
            spec,
        }
    };
    // The name of one of Claude Code's own models (`opus`, `sonnet`).
    let nq = normalize(model);
    if let Some(i) = claude
        .iter()
        .position(|c| c.id.eq_ignore_ascii_case(model) || normalize(&c.id) == nq)
    {
        return Ok(routed(i, ""));
    }
    // Else the Anthropic model it names, and the Claude Code model that
    // resolves to it.
    let anthropic: Vec<Candidate> = config
        .list_models("anthropic", &["chat"])
        .into_iter()
        .map(|id| {
            let r = released(config, "anthropic", &id);
            Candidate::new("anthropic", &id, r)
        })
        .collect();
    let target = match pick(model, &anthropic) {
        Match::One(i) => Some(anthropic[i].id.clone()),
        _ => None,
    };
    if let Some(target) = &target {
        let want = undated(target);
        let mut same: Vec<usize> = (0..saved.models.len())
            .filter(|&i| {
                let resolved = claude_catalog::price_ids(&saved.models[i])
                    .into_iter()
                    .next()
                    .unwrap_or_default();
                undated(&resolved) == want || undated(&saved.models[i].id) == want
            })
            .collect();
        // `sonnet` and `sonnet[1m]` resolve to one model: the plain one.
        if same.len() > 1 {
            same.sort_by_key(|&i| (saved.models[i].id.len(), saved.models[i].id.clone()));
            let base = saved.models[same[0]].id.clone();
            if same.iter().all(|&i| saved.models[i].id.starts_with(&base)) {
                same.truncate(1);
            }
        }
        if let [i] = same.as_slice() {
            return Ok(routed(*i, &format!(" ({target})")));
        }
    }
    // Nothing to stand in for it: say what to run.
    let fam = target.as_deref().and_then(family).or_else(|| family(model));
    let suggestion = fam
        .and_then(|f| {
            claude
                .iter()
                .position(|c| c.id.to_lowercase() == f)
                .or_else(|| claude.iter().position(|c| c.id.to_lowercase().contains(f)))
        })
        .map(|i| claude[i].spec())
        .or_else(|| {
            let access = Access {
                claude_code: Some(saved.clone()),
                anthropic_ready: false,
            };
            newest_of(config, claude_agent::ID, &access)
        })
        .unwrap_or_else(|| "claude-agent/<model>".into());
    Err(anyhow!(
        "{typed} needs an Anthropic API key, and none is set. You're signed into Claude Code: switch to {suggestion}, or run `codesmith auth login anthropic`."
    ))
}

/// The Claude Code model to suggest for the Anthropic model `model`, when
/// Claude Code is signed in: its equivalent, else the newest one.
pub fn claude_code_for(config: &Config, model: &str, access: &Access) -> Option<String> {
    let saved = access.claude_code.as_ref()?;
    match through_claude_code(config, model, None, saved) {
        Ok(r) => Some(r.spec),
        Err(_) => newest_of(config, claude_agent::ID, access),
    }
}

/// The newest model of `provider` that takes and makes text, for a login
/// to switch to.
pub fn newest_of(config: &Config, provider: &str, access: &Access) -> Option<String> {
    let text = |v: &[String]| v.is_empty() || v.iter().any(|m| m.starts_with("text"));
    let candidates: Vec<Candidate> = provider_candidates(config, provider, access)
        .into_iter()
        .filter(|c| !crate::providers::hidden(provider, &c.id))
        .filter(|c| {
            config
                .capabilities(&c.spec())
                .is_some_and(|caps| text(&caps.input) && text(&caps.output))
        })
        .collect();
    let top = candidates.iter().map(|c| c.released).max()?;
    // The first of equally new ones, preferring an alias (the shortest).
    candidates
        .iter()
        .filter(|c| c.released == top)
        .min_by(|a, b| a.id.len().cmp(&b.id.len()).then_with(|| a.id.cmp(&b.id)))
        .map(Candidate::spec)
}

/// Where the model a login chose is remembered (in the data folder), so a
/// later login may change it while a model the user set stays theirs.
const AUTO_FILE: &str = "model-auto.json";

/// The model a login last chose, if any.
pub fn auto_model(data_dir: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(data_dir.join(AUTO_FILE)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v["model"].as_str().map(str::to_string)
}

/// Whether the configured `model` is the implicit default rather than one
/// the user chose: none is set, or it is the one a login chose.
pub fn implicit(configured: Option<&str>, auto: Option<&str>) -> bool {
    match configured {
        None => true,
        Some(m) => auto.is_some_and(|a| a.trim() == m.trim()),
    }
}

/// What a login switched to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adopted {
    pub spec: String,
    /// The config file the choice was saved in, or why it was not.
    pub saved: std::result::Result<std::path::PathBuf, String>,
}

/// After a login to `provider`: when the user has not chosen a model
/// themselves, make the provider's newest model the default, saving it as
/// `model` in the global codesmith.json. `None` when nothing changes.
pub fn adopt_login(
    config: &Config,
    provider: &str,
    access: &Access,
    config_dir: &std::path::Path,
    data_dir: &std::path::Path,
) -> Option<Adopted> {
    let auto = auto_model(data_dir);
    if !implicit(config.model.as_deref(), auto.as_deref()) {
        return None;
    }
    let spec = newest_of(config, provider, access)?;
    if config.model.as_deref() == Some(spec.as_str()) {
        return None;
    }
    let saved = if crate::config::global_disabled() {
        Err("the global config is turned off (CODESMITH_DISABLE_GLOBAL_CONFIG)".to_string())
    } else {
        save_global_model(config_dir, &spec, auto.as_deref()).map_err(|e| format!("{e:#}"))
    };
    if saved.is_ok() {
        let text = serde_json::json!({ "model": spec }).to_string();
        let _ = std::fs::create_dir_all(data_dir);
        let _ = std::fs::write(data_dir.join(AUTO_FILE), text);
    }
    Some(Adopted { spec, saved })
}

/// Set `model` in the global config file in `dir` (codesmith.json, or the
/// codesmith.jsonc there), replacing only a `model` that is `replace`.
pub fn save_global_model(
    dir: &std::path::Path,
    spec: &str,
    replace: Option<&str>,
) -> Result<std::path::PathBuf> {
    let path = crate::config::NAMES
        .iter()
        .map(|n| dir.join(n))
        .find(|p| p.exists())
        .unwrap_or_else(|| dir.join(crate::config::NAMES[0]));
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(anyhow!("reading {}: {e}", path.display())),
    };
    let next =
        set_model_text(&text, spec, replace).map_err(|e| anyhow!("{}: {e}", path.display()))?;
    std::fs::create_dir_all(dir)?;
    // Written whole, then moved over the file.
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("codesmith.json");
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, &next)?;
    if let Ok(meta) = std::fs::metadata(&path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(path)
}

/// Where a JSON(C) document's top-level `model` value is: the offset just
/// past the object's `{`, whether the object is empty, and the byte range
/// of `model`'s string value when it is set.
type ModelSlot = (usize, bool, Option<(usize, usize)>);

fn find_model(text: &str) -> Result<ModelSlot> {
    let b = text.as_bytes();
    // Past whitespace and comments.
    let skip = |mut i: usize| -> usize {
        loop {
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if b[i..].starts_with(b"//") {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            } else if b[i..].starts_with(b"/*") {
                i += 2;
                while i < b.len() && !b[i..].starts_with(b"*/") {
                    i += 1;
                }
                i = (i + 2).min(b.len());
            } else {
                return i;
            }
        }
    };
    // Past the closing quote of the string starting at `i`.
    let string_end = |mut i: usize| -> Result<usize> {
        i += 1;
        while i < b.len() {
            match b[i] {
                b'\\' => i += 2,
                b'"' => return Ok(i + 1),
                _ => i += 1,
            }
        }
        Err(anyhow!("an unterminated string"))
    };
    let mut i = skip(0);
    if b.get(i) != Some(&b'{') {
        return Err(anyhow!("the config is not a JSON object"));
    }
    let open = i + 1;
    let empty = b.get(skip(open)) == Some(&b'}');
    let mut depth = 0usize;
    let mut expect_key = false;
    let mut slot = None;
    while i < b.len() {
        i = skip(i);
        if i >= b.len() {
            break;
        }
        match b[i] {
            c @ (b'{' | b'[') => {
                depth += 1;
                expect_key = depth == 1 && c == b'{';
                i += 1;
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                i += 1;
                if depth == 0 {
                    break;
                }
            }
            b',' => {
                expect_key = depth == 1;
                i += 1;
            }
            b'"' => {
                let end = string_end(i)?;
                if depth == 1 && expect_key {
                    expect_key = false;
                    let key: String = serde_json::from_str(&text[i..end]).unwrap_or_default();
                    let colon = skip(end);
                    if b.get(colon) != Some(&b':') {
                        return Err(anyhow!("a key without a value"));
                    }
                    let v = skip(colon + 1);
                    if key == "model" {
                        if b.get(v) != Some(&b'"') {
                            return Err(anyhow!("\"model\" is not a string"));
                        }
                        slot = Some((v, string_end(v)?));
                    }
                    i = v;
                } else {
                    i = end;
                }
            }
            _ => i += 1,
        }
    }
    Ok((open, empty, slot))
}

/// `text` (a JSON or JSONC config, possibly empty) with `model` set to
/// `spec`, keeping everything else, comments included. A `model` already
/// there is replaced only when it is `replace`.
pub fn set_model_text(text: &str, spec: &str, replace: Option<&str>) -> Result<String> {
    let value = serde_json::to_string(spec)?;
    if text.trim().is_empty() {
        return Ok(format!("{{\n  \"model\": {value}\n}}\n"));
    }
    let (open, empty, slot) = find_model(text)?;
    let out = match slot {
        Some((start, end)) => {
            let current: String = serde_json::from_str(&text[start..end]).unwrap_or_default();
            if replace.is_none_or(|r| r.trim() != current.trim()) {
                return Err(anyhow!("it already sets \"model\" to {current:?}"));
            }
            format!("{}{value}{}", &text[..start], &text[end..])
        }
        None if empty => format!("{}\n  \"model\": {value}\n{}", &text[..open], &text[open..]),
        None => format!("{}\n  \"model\": {value},{}", &text[..open], &text[open..]),
    };
    // Only a change that reads back as intended is written.
    let back: serde_json::Value = serde_json::from_str(&crate::config::strip_jsonc(&out))
        .map_err(|e| anyhow!("the edit would not parse: {e}"))?;
    if back["model"].as_str() != Some(spec) {
        return Err(anyhow!("the edit did not take"));
    }
    Ok(out)
}

#[cfg(test)]
#[path = "model_spec_tests.rs"]
mod tests;
