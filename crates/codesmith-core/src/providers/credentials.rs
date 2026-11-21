//! Saved provider credentials (after OpenCode's `Credential` service):
//! several per provider, one of them active, and OAuth logins refreshed
//! once per expiry however many requests need them.
//!
//! They live in `auth.json` (owner-only, under its file lock). The active
//! credential is the provider's entry, where every reader looks; the
//! others, with each credential's id, label and creation order, are kept
//! under `$credentials` beside it:
//!
//! ```json
//! {"openai": {"type": "api", "key": "…"},
//!  "$credentials": {"openai": {"active": {"id": "cred_…", "label": "Work", "created": 2},
//!                              "saved": [{"id": "cred_…", "label": "Personal", "created": 1, "value": {…}}]}}}
//! ```
//!
//! An entry saved before this (no `$credentials` record) is a credential
//! of its own, `cred_legacy`. A value's legacy `configuration` (key form
//! answers before OpenCode 2.1) reads as `metadata`.
//!
//! A refresh ([`refresh_in`]) runs once for concurrent callers of the same
//! credential, who share its result or its failure; a later call may try
//! again. It keeps a credential that changed while the refresh ran (another
//! process refreshed it, or the user logged in again), and saves the
//! refreshed value only to the entry it was refreshed from. Credentials are
//! never logged.

use anyhow::{anyhow, Result};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Where the records of saved credentials are kept in `auth.json`.
pub const KEY: &str = "$credentials";
/// The id of an entry saved without a record.
pub const LEGACY_ID: &str = "cred_legacy";
/// A login is refreshed this long before it expires.
pub const EXPIRY_MARGIN_MS: u64 = 5 * 60_000;

/// One saved credential.
#[derive(Debug, Clone, PartialEq)]
pub struct Credential {
    pub id: String,
    pub provider: String,
    pub label: Option<String>,
    /// Creation order (larger is newer).
    pub created: u64,
    pub active: bool,
    pub value: Value,
}

/// Whether an OAuth login `value` is due for a refresh at `now` (ms): within
/// five minutes of its expiry. A login without a lifetime (`expires` 0 or
/// absent) never is.
pub fn expiring(value: &Value, now: u64) -> bool {
    if value["type"] != "oauth" {
        return false;
    }
    match value["expires"].as_u64() {
        Some(0) | None => false,
        Some(e) => e <= now + EXPIRY_MARGIN_MS,
    }
}

/// A value as read: a legacy `configuration` becomes `metadata`.
pub fn normalize(mut value: Value) -> Value {
    if let Some(Value::Object(conf)) = value
        .as_object_mut()
        .and_then(|o| o.remove("configuration"))
    {
        let meta = value
            .as_object_mut()
            .unwrap()
            .entry("metadata")
            .or_insert_with(|| json!({}));
        if let Some(m) = meta.as_object_mut() {
            m.extend(conf);
        }
    }
    value
}

fn lock_and_read(path: &Path) -> Result<(crate::auth::FileLock, Map<String, Value>)> {
    let lock = crate::auth::FileLock::acquire(path)?;
    let m: Map<String, Value> = crate::auth::read_for_update(path)?;
    Ok((lock, m))
}

fn write(path: &Path, m: &Map<String, Value>) -> Result<()> {
    Ok(crate::paths::write_private(
        path,
        serde_json::to_string_pretty(m)?.as_bytes(),
    )?)
}

fn read(path: &Path) -> Map<String, Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// The `$credentials` record of `provider` in `m` (created empty).
fn record<'a>(m: &'a mut Map<String, Value>, provider: &str) -> &'a mut Map<String, Value> {
    let all = m.entry(KEY).or_insert_with(|| json!({}));
    if !all.is_object() {
        *all = json!({});
    }
    let r = all
        .as_object_mut()
        .unwrap()
        .entry(provider)
        .or_insert_with(|| json!({}));
    if !r.is_object() {
        *r = json!({});
    }
    r.as_object_mut().unwrap()
}

fn next_created(m: &Map<String, Value>) -> u64 {
    let mut max = 0;
    if let Some(all) = m.get(KEY).and_then(Value::as_object) {
        for r in all.values() {
            max = max.max(r["active"]["created"].as_u64().unwrap_or(0));
            for s in r["saved"].as_array().into_iter().flatten() {
                max = max.max(s["created"].as_u64().unwrap_or(0));
            }
        }
    }
    max + 1
}

fn new_id() -> String {
    format!("cred_{}", crate::paths::random_token(12))
}

fn list_from(m: &Map<String, Value>, provider: &str) -> Vec<Credential> {
    let r = m.get(KEY).and_then(|a| a.get(provider));
    let mut out: Vec<Credential> = r
        .and_then(|r| r["saved"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|s| {
            Some(Credential {
                id: s["id"].as_str()?.to_string(),
                provider: provider.to_string(),
                label: s["label"].as_str().map(str::to_string),
                created: s["created"].as_u64().unwrap_or(0),
                active: false,
                value: normalize(s["value"].clone()),
            })
        })
        .collect();
    out.sort_by_key(|c| c.created);
    if let Some(v) = m.get(provider).filter(|v| v.is_object()) {
        let a = r.map(|r| &r["active"]);
        out.push(Credential {
            id: a
                .and_then(|a| a["id"].as_str())
                .unwrap_or(LEGACY_ID)
                .to_string(),
            provider: provider.to_string(),
            label: a.and_then(|a| a["label"].as_str()).map(str::to_string),
            created: a.and_then(|a| a["created"].as_u64()).unwrap_or(0),
            active: true,
            value: normalize(v.clone()),
        });
    }
    out
}

/// A provider's saved credentials: the others in creation order, then the
/// active one.
pub fn list_in(path: &Path, provider: &str) -> Vec<Credential> {
    list_from(&read(path), provider)
}

/// One saved credential by id, and its provider.
pub fn get_in(path: &Path, id: &str) -> Option<Credential> {
    let m = read(path);
    providers(&m)
        .into_iter()
        .flat_map(|p| list_from(&m, &p))
        .find(|c| c.id == id)
}

/// Every provider with a saved credential.
fn providers(m: &Map<String, Value>) -> Vec<String> {
    let mut out: Vec<String> = m.keys().filter(|k| !k.starts_with('$')).cloned().collect();
    if let Some(all) = m.get(KEY).and_then(Value::as_object) {
        for k in all.keys() {
            if !out.contains(k) {
                out.push(k.clone());
            }
        }
    }
    out
}

/// What to save with [`create_in`].
#[derive(Debug, Clone, Default)]
pub struct New {
    pub provider: String,
    pub value: Value,
    pub label: Option<String>,
    /// Keep this id (an import) instead of making one.
    pub id: Option<String>,
    /// Leave the current selection: the credential is saved inactive unless
    /// the provider has none active.
    pub inactive: bool,
}

/// Save a credential. By default it becomes the provider's active one, and
/// the one it replaces is kept.
pub fn create_in(path: &Path, new: New) -> Result<Credential> {
    let (_lock, mut m) = lock_and_read(path)?;
    let id = new.id.clone().unwrap_or_else(new_id);
    if list_from(&m, &new.provider).iter().any(|c| c.id == id) {
        return Err(anyhow!("a credential {id} is already saved"));
    }
    let created = next_created(&m);
    let has_active = m.get(&new.provider).is_some_and(Value::is_object);
    let activate = !new.inactive || !has_active;
    let meta = json!({"id": id, "label": new.label, "created": created});
    if activate {
        demote(&mut m, &new.provider);
        m.insert(new.provider.clone(), new.value.clone());
        record(&mut m, &new.provider).insert("active".into(), meta);
    } else {
        let mut saved = meta;
        saved["value"] = new.value.clone();
        push_saved(&mut m, &new.provider, saved);
    }
    write(path, &m)?;
    Ok(Credential {
        id,
        provider: new.provider,
        label: new.label,
        created,
        active: activate,
        value: normalize(new.value),
    })
}

fn push_saved(m: &mut Map<String, Value>, provider: &str, saved: Value) {
    let r = record(m, provider);
    let list = r.entry("saved").or_insert_with(|| json!([]));
    if !list.is_array() {
        *list = json!([]);
    }
    list.as_array_mut().unwrap().push(saved);
}

/// Move the provider's active entry (if any) to its saved ones.
fn demote(m: &mut Map<String, Value>, provider: &str) {
    let Some(v) = m.remove(provider).filter(Value::is_object) else {
        return;
    };
    let created = next_created(m);
    let a = record(m, provider)
        .remove("active")
        .unwrap_or_else(|| json!({"id": LEGACY_ID, "created": 0}));
    let mut saved = json!({
        "id": a["id"].as_str().unwrap_or(LEGACY_ID),
        "label": a["label"],
        "created": a["created"].as_u64().unwrap_or(created),
    });
    saved["value"] = v;
    push_saved(m, provider, saved);
}

/// Make saved credential `id` its provider's active one; `false` when there
/// is no such credential. Other providers are left alone.
pub fn activate_in(path: &Path, id: &str) -> Result<bool> {
    let (_lock, mut m) = lock_and_read(path)?;
    let Some(c) = providers(&m)
        .into_iter()
        .flat_map(|p| list_from(&m, &p))
        .find(|c| c.id == id)
    else {
        return Ok(false);
    };
    if c.active {
        return Ok(true);
    }
    promote(&mut m, &c.provider, id);
    write(path, &m)?;
    Ok(true)
}

/// Take saved credential `id` out of the saved list and make it active.
fn promote(m: &mut Map<String, Value>, provider: &str, id: &str) {
    demote(m, provider);
    let r = record(m, provider);
    let list = r
        .get_mut("saved")
        .and_then(Value::as_array_mut)
        .map(std::mem::take)
        .unwrap_or_default();
    let (picked, rest): (Vec<Value>, Vec<Value>) =
        list.into_iter().partition(|s| s["id"].as_str() == Some(id));
    r.insert("saved".into(), Value::Array(rest));
    if let Some(mut s) = picked.into_iter().next() {
        let value = s.as_object_mut().and_then(|o| o.remove("value"));
        r.insert("active".into(), s);
        if let Some(v) = value {
            m.insert(provider.to_string(), v);
        }
    }
}

/// What [`update_in`] changes.
#[derive(Debug, Clone, Default)]
pub struct Update {
    pub label: Option<String>,
    pub value: Option<Value>,
}

/// Change a saved credential's label or value; `false` when there is none.
pub fn update_in(path: &Path, id: &str, update: Update) -> Result<bool> {
    let (_lock, mut m) = lock_and_read(path)?;
    let Some(c) = providers(&m)
        .into_iter()
        .flat_map(|p| list_from(&m, &p))
        .find(|c| c.id == id)
    else {
        return Ok(false);
    };
    if c.active {
        if let Some(v) = update.value {
            m.insert(c.provider.clone(), v);
        }
        if let Some(l) = update.label {
            let r = record(&mut m, &c.provider);
            let a = r
                .entry("active")
                .or_insert_with(|| json!({"id": LEGACY_ID, "created": 0}));
            a["label"] = l.into();
        }
    } else {
        let r = record(&mut m, &c.provider);
        for s in r
            .get_mut("saved")
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            if s["id"].as_str() == Some(id) {
                if let Some(v) = &update.value {
                    s["value"] = v.clone();
                }
                if let Some(l) = &update.label {
                    s["label"] = l.clone().into();
                }
            }
        }
    }
    write(path, &m)?;
    Ok(true)
}

/// Remove a saved credential; `false` when there is none. Removing the
/// active one makes the newest remaining one active.
pub fn remove_in(path: &Path, id: &str) -> Result<bool> {
    let (_lock, mut m) = lock_and_read(path)?;
    let Some(c) = providers(&m)
        .into_iter()
        .flat_map(|p| list_from(&m, &p))
        .find(|c| c.id == id)
    else {
        return Ok(false);
    };
    if c.active {
        m.remove(&c.provider);
        let r = record(&mut m, &c.provider);
        r.remove("active");
        let newest = r
            .get("saved")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .max_by_key(|s| s["created"].as_u64().unwrap_or(0))
            .and_then(|s| s["id"].as_str())
            .map(str::to_string);
        if let Some(n) = newest {
            promote(&mut m, &c.provider, &n);
        }
    } else {
        let r = record(&mut m, &c.provider);
        if let Some(list) = r.get_mut("saved").and_then(Value::as_array_mut) {
            list.retain(|s| s["id"].as_str() != Some(id));
        }
    }
    // Drop records left empty.
    if let Some(all) = m.get_mut(KEY).and_then(Value::as_object_mut) {
        all.retain(|_, r| {
            r.get("active").is_some() || r["saved"].as_array().is_some_and(|s| !s.is_empty())
        });
        if all.is_empty() {
            m.remove(KEY);
        }
    }
    write(path, &m)?;
    Ok(true)
}

type Flight = Arc<tokio::sync::OnceCell<Result<Value, String>>>;

fn flights() -> &'static Mutex<HashMap<(PathBuf, String), Flight>> {
    static F: std::sync::OnceLock<Mutex<HashMap<(PathBuf, String), Flight>>> =
        std::sync::OnceLock::new();
    F.get_or_init(Default::default)
}

/// Refresh provider `provider`'s active credential in `path` when `stale`
/// says so, with `refresh`: the value saved (and returned), or the stored
/// one when it is not stale or changed while the refresh ran. `None` when
/// the provider has no credential. Concurrent callers share one refresh and
/// its failure; the next call after a failure tries again.
pub async fn refresh_in<F, Fut>(
    path: &Path,
    provider: &str,
    stale: impl Fn(&Value) -> bool,
    refresh: F,
) -> Result<Option<Value>>
where
    F: FnOnce(Value) -> Fut,
    Fut: Future<Output = Result<Value>>,
{
    let Some(current) = read(path).get(provider).filter(|v| v.is_object()).cloned() else {
        return Ok(None);
    };
    if !stale(&current) {
        return Ok(Some(normalize(current)));
    }
    let key = (path.to_path_buf(), provider.to_string());
    let flight = flights()
        .lock()
        .unwrap()
        .entry(key.clone())
        .or_insert_with(|| Arc::new(tokio::sync::OnceCell::new()))
        .clone();
    let stale = &stale;
    let out = flight
        .get_or_init(|| async move {
            // Another caller may have refreshed it since it was read.
            let from = match read(path).get(provider).filter(|v| v.is_object()) {
                Some(latest) if *latest != current && !stale(latest) => return Ok(latest.clone()),
                Some(latest) => latest.clone(),
                None => current,
            };
            let fresh = refresh(from.clone()).await.map_err(|e| format!("{e:#}"))?;
            let save = || -> Result<Value> {
                let (_lock, mut m) = lock_and_read(path)?;
                match m.get(provider) {
                    // Changed meanwhile: keep what is there.
                    Some(now) if *now != from => Ok(now.clone()),
                    None => Ok(fresh.clone()),
                    Some(_) => {
                        m.insert(provider.to_string(), fresh.clone());
                        write(path, &m)?;
                        Ok(fresh.clone())
                    }
                }
            };
            save().map_err(|e| format!("{e:#}"))
        })
        .await
        .clone();
    {
        let mut all = flights().lock().unwrap();
        if all.get(&key).is_some_and(|f| Arc::ptr_eq(f, &flight)) {
            all.remove(&key);
        }
    }
    out.map(|v| Some(normalize(v))).map_err(|e| anyhow!(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oauth(access: &str, refresh: &str, expires: u64) -> Value {
        json!({"type": "oauth", "method": "oauth", "access": access, "refresh": refresh, "expires": expires})
    }

    fn key(k: &str) -> Value {
        json!({"type": "api", "key": k})
    }

    fn file() -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("auth.json");
        (d, p)
    }

    fn create(path: &Path, provider: &str, value: Value) -> Credential {
        create_in(
            path,
            New {
                provider: provider.into(),
                value,
                ..Default::default()
            },
        )
        .unwrap()
    }

    fn ids(path: &Path, provider: &str) -> Vec<String> {
        list_in(path, provider).into_iter().map(|c| c.id).collect()
    }

    // credential-refresh.test.ts: "saves the refreshed value"
    #[tokio::test]
    async fn oc_credential_refresh_saves_the_refreshed_value() {
        let (_d, p) = file();
        create(&p, "test", oauth("old-access", "old-refresh", 1));
        let value = oauth("new-access", "new-refresh", u64::MAX / 2);
        let v2 = value.clone();
        let got = refresh_in(&p, "test", |_| true, |_| async move { Ok(v2) })
            .await
            .unwrap();
        assert_eq!(got, Some(value.clone()));
        assert_eq!(crate::auth::entry_in(&p, "test"), Some(value));
    }

    // credential-refresh.test.ts: "returns the stored row when another caller already refreshed it"
    #[tokio::test]
    async fn oc_credential_refresh_returns_the_stored_row_when_not_stale() {
        let (_d, p) = file();
        create(&p, "test", oauth("old-access", "old-refresh", 1));
        let calls = std::sync::atomic::AtomicU32::new(0);
        let got = refresh_in(
            &p,
            "test",
            |_| false,
            |_| async {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(oauth("new-access", "new-refresh", 2))
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(got["access"], "old-access");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    // credential-refresh.test.ts: "keeps a row that changed while the refresh ran"
    #[tokio::test]
    async fn oc_credential_refresh_keeps_a_row_that_changed_while_it_ran() {
        let (_d, p) = file();
        let c = create(&p, "test", oauth("old-access", "old-refresh", 1));
        let replaced = oauth("other-access", "other-refresh", 3);
        let (p2, r2, id) = (p.clone(), replaced.clone(), c.id.clone());
        let got = refresh_in(
            &p,
            "test",
            |_| true,
            |_| async move {
                update_in(
                    &p2,
                    &id,
                    Update {
                        value: Some(r2),
                        ..Default::default()
                    },
                )?;
                Ok(oauth("new-access", "new-refresh", 2))
            },
        )
        .await
        .unwrap();
        assert_eq!(got, Some(replaced.clone()));
        assert_eq!(crate::auth::entry_in(&p, "test"), Some(replaced));
    }

    // credential-refresh.test.ts: "resolves undefined for a missing credential"
    #[tokio::test]
    async fn oc_credential_refresh_resolves_none_for_a_missing_credential() {
        let (_d, p) = file();
        let got = refresh_in(
            &p,
            "missing",
            |_| true,
            |_| async { unreachable!("no credential to refresh") },
        )
        .await
        .unwrap();
        assert_eq!(got, None);
    }

    // credential-refresh.test.ts: "shares failures but allows a later refresh"
    #[tokio::test(flavor = "multi_thread")]
    async fn oc_credential_refresh_shares_failures_but_allows_a_later_refresh() {
        let (_d, p) = file();
        create(&p, "test", oauth("old-access", "old-refresh", 1));
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let release = Arc::new(tokio::sync::Notify::new());
        let entered = Arc::new(tokio::sync::Notify::new());
        let failing = |calls: Arc<std::sync::atomic::AtomicU32>,
                       entered: Arc<tokio::sync::Notify>,
                       release: Arc<tokio::sync::Notify>| {
            move |_: Value| async move {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                entered.notify_one();
                release.notified().await;
                Err::<Value, _>(anyhow!("token endpoint unavailable"))
            }
        };
        let first = {
            let (p, f) = (
                p.clone(),
                failing(calls.clone(), entered.clone(), release.clone()),
            );
            tokio::spawn(async move { refresh_in(&p, "test", |_| true, f).await })
        };
        entered.notified().await;
        let second = {
            let (p, f) = (
                p.clone(),
                failing(calls.clone(), entered.clone(), release.clone()),
            );
            tokio::spawn(async move { refresh_in(&p, "test", |_| true, f).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        release.notify_one();
        let e1 = first.await.unwrap().unwrap_err().to_string();
        let e2 = second.await.unwrap().unwrap_err().to_string();
        assert_eq!(e1, "token endpoint unavailable");
        assert_eq!(e2, e1);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let value = oauth("new-access", "new-refresh", 2);
        let v2 = value.clone();
        assert_eq!(
            refresh_in(&p, "test", |_| true, |_| async move { Ok(v2) })
                .await
                .unwrap(),
            Some(value)
        );
    }

    // credential-refresh.test.ts: "is due within five minutes of expiry"
    #[test]
    fn oc_credential_is_due_within_five_minutes_of_expiry() {
        let now = 1_000_000_000;
        assert!(expiring(&oauth("a", "r", now + 4 * 60_000), now));
        assert!(!expiring(&oauth("a", "r", now + 6 * 60_000), now));
        assert!(!expiring(&key("k"), now));
    }

    // credential-refresh.test.ts: "never expires a token without a lifetime"
    #[test]
    fn oc_credential_never_expires_a_token_without_a_lifetime() {
        assert!(!expiring(&oauth("a", "r", 0), 1_000_000_000));
    }

    // credential.test.ts: "stores, updates, lists, and removes credentials"
    #[test]
    fn oc_credential_stores_updates_lists_and_removes() {
        let (_d, p) = file();
        let created = create_in(
            &p,
            New {
                provider: "openai".into(),
                label: Some("Work".into()),
                value: key("secret"),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(list_in(&p, "openai"), std::slice::from_ref(&created));
        update_in(
            &p,
            &created.id,
            Update {
                label: Some("Personal".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(list_in(&p, "openai")[0].label.as_deref(), Some("Personal"));
        let additional = create_in(
            &p,
            New {
                provider: "openai".into(),
                label: Some("Additional".into()),
                value: key("additional"),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            ids(&p, "openai"),
            [created.id.clone(), additional.id.clone()]
        );
        assert_eq!(
            crate::auth::key_in(&p, "openai").as_deref(),
            Some("additional")
        );
        assert!(remove_in(&p, &additional.id).unwrap());
        let l = list_in(&p, "openai");
        assert_eq!(l.len(), 1);
        assert_eq!(
            (l[0].id.as_str(), l[0].label.as_deref()),
            (created.id.as_str(), Some("Personal"))
        );
        assert_eq!(crate::auth::key_in(&p, "openai").as_deref(), Some("secret"));
        // The bookkeeping is not a provider.
        assert_eq!(crate::auth::providers_in(&p), ["openai"]);
    }

    // credential.test.ts: "reads key form answers stored in configuration before v2.1 as metadata"
    #[test]
    fn oc_credential_reads_legacy_configuration_as_metadata() {
        let (_d, p) = file();
        std::fs::write(
            &p,
            json!({"azure": {"type": "api", "key": "secret",
                "metadata": {"imported": true, "resourceName": "older"},
                "configuration": {"resourceName": "answered"}}})
            .to_string(),
        )
        .unwrap();
        let l = list_in(&p, "azure");
        assert_eq!(l[0].id, LEGACY_ID);
        assert_eq!(
            l[0].value,
            json!({"type": "api", "key": "secret", "metadata": {"imported": true, "resourceName": "answered"}})
        );
    }

    // credential.test.ts: "promotes the newest remaining legacy credential when the effective selection is removed"
    #[test]
    fn oc_credential_promotes_the_newest_remaining_when_the_active_one_is_removed() {
        let (_d, p) = file();
        let oldest = create(&p, "openai", key("oldest"));
        let newer = create(&p, "openai", key("newer"));
        let newest = create(&p, "openai", key("newest"));
        assert!(activate_in(&p, &newest.id).unwrap());
        assert!(remove_in(&p, &oldest.id).unwrap());
        assert!(remove_in(&p, &newest.id).unwrap());
        assert_eq!(ids(&p, "openai"), std::slice::from_ref(&newer.id));
        assert!(list_in(&p, "openai")[0].active);
        assert_eq!(crate::auth::key_in(&p, "openai").as_deref(), Some("newer"));
    }

    // credential.test.ts: "activates older credentials without affecting other integrations"
    #[test]
    fn oc_credential_activates_older_credentials_without_affecting_others() {
        let (_d, p) = file();
        let older = create(&p, "openai", key("older"));
        let newer = create(&p, "openai", key("newer"));
        let other_older = create(&p, "anthropic", key("other-older"));
        let other_newer = create(&p, "anthropic", key("other-newer"));
        assert!(activate_in(&p, &older.id).unwrap());
        assert_eq!(ids(&p, "openai"), [newer.id.clone(), older.id.clone()]);
        assert_eq!(
            ids(&p, "anthropic"),
            [other_older.id.clone(), other_newer.id.clone()]
        );
        assert!(!activate_in(&p, "cred_missing").unwrap());
        assert_eq!(ids(&p, "openai").last(), Some(&older.id));
        assert!(activate_in(&p, &other_older.id).unwrap());
        assert_eq!(
            ids(&p, "anthropic"),
            [other_newer.id.clone(), other_older.id.clone()]
        );
        assert_eq!(ids(&p, "openai").last(), Some(&older.id));
        assert!(remove_in(&p, &older.id).unwrap());
        assert_eq!(ids(&p, "openai").last(), Some(&newer.id));
        assert_eq!(ids(&p, "anthropic").last(), Some(&other_older.id));
    }

    // credential.test.ts: "creates credentials with a requested ID without taking over the current selection"
    #[test]
    fn oc_credential_creates_with_a_requested_id_without_taking_over() {
        let (_d, p) = file();
        let first = create_in(
            &p,
            New {
                provider: "openai".into(),
                value: key("first"),
                id: Some("cred_0000imported".into()),
                inactive: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(first.id, "cred_0000imported");
        // Nothing else was active, so it is.
        assert_eq!(ids(&p, "openai").last(), Some(&first.id));
        let legacy = create(&p, "openai", key("legacy"));
        let imported = create_in(
            &p,
            New {
                provider: "openai".into(),
                value: key("imported"),
                inactive: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!imported.active);
        assert_eq!(ids(&p, "openai"), [first.id, imported.id, legacy.id]);
        assert_eq!(crate::auth::key_in(&p, "openai").as_deref(), Some("legacy"));
    }

    #[test]
    fn an_entry_saved_before_records_is_a_credential_of_its_own() {
        let (_d, p) = file();
        crate::auth::set_in(&p, "openai", "sk-old").unwrap();
        let added = create(&p, "openai", key("sk-new"));
        let l = list_in(&p, "openai");
        assert_eq!(
            l.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            [LEGACY_ID, added.id.as_str()]
        );
        assert!(activate_in(&p, LEGACY_ID).unwrap());
        assert_eq!(crate::auth::key_in(&p, "openai").as_deref(), Some("sk-old"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
