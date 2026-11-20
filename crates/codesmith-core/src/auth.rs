//! API keys saved locally with `codesmith auth login`, in the data dir's
//! `auth.json` (owner-only permissions). Config and environment variables
//! take precedence over saved keys.

use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::path::Path;

fn read(path: &Path) -> Map<String, Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn write(path: &Path, map: &Map<String, Value>) -> Result<()> {
    crate::paths::write_private(path, serde_json::to_string_pretty(map)?.as_bytes())
        .with_context(|| format!("writing {}", path.display()))
}

pub fn key_in(path: &Path, provider: &str) -> Option<String> {
    read(path)
        .get(provider)?
        .get("key")?
        .as_str()
        .filter(|k| !k.is_empty())
        .map(str::to_string)
}

pub fn set_in(path: &Path, provider: &str, key: &str) -> Result<()> {
    let mut m = read(path);
    m.insert(provider.to_string(), json!({"type": "api", "key": key}));
    write(path, &m)
}

/// Remove a saved key; false when there was none.
pub fn remove_in(path: &Path, provider: &str) -> Result<bool> {
    let mut m = read(path);
    let had = m.remove(provider).is_some();
    if had {
        write(path, &m)?;
    }
    Ok(had)
}

pub fn providers_in(path: &Path) -> Vec<String> {
    read(path).keys().cloned().collect()
}

pub fn key(provider: &str) -> Option<String> {
    key_in(&crate::paths::auth_file(), provider)
}

/// `sk-ab…wxyz` style, for listing without revealing the key.
pub fn mask(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 10 {
        return "…".into();
    }
    format!(
        "{}…{}",
        chars[..4].iter().collect::<String>(),
        chars[chars.len() - 4..].iter().collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_read_remove() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("sub/auth.json");
        assert_eq!(key_in(&p, "openai"), None);
        set_in(&p, "openai", "sk-1234567890abcd").unwrap();
        set_in(&p, "groq", "gsk_x").unwrap();
        assert_eq!(key_in(&p, "openai").as_deref(), Some("sk-1234567890abcd"));
        assert_eq!(providers_in(&p), ["groq", "openai"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert!(remove_in(&p, "groq").unwrap());
        assert!(!remove_in(&p, "groq").unwrap());
        assert_eq!(providers_in(&p), ["openai"]);
    }

    #[test]
    fn masking() {
        assert_eq!(mask("sk-1234567890abcd"), "sk-1…abcd");
        assert_eq!(mask("short"), "…");
    }
}
