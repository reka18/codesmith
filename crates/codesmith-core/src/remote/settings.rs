//! The `remote.*` settings. They're read only from your own global config
//! file (`codesmith.json` or `codesmith.jsonc` in the global config
//! folder): a per-repo file, `$CODESMITH_CONFIG` or
//! `$CODESMITH_CONFIG_CONTENT` can't turn remote access on or change the
//! relay, and a `remote` key in one is ignored with a warning (see
//! [`crate::config::load_from`]).
//!
//! ```jsonc
//! "remote": {
//!   "enabled": false,     // set by /mobile, `codesmith remote pair` and `start`
//!   "relay": "default"    // "default" (Skeptic Dog's relays) | a relay URL | "none"
//! }
//! ```

use serde_json::Value;
use std::path::Path;

/// Skeptic Dog's relays: `"relay": "default"`. n0's public relays are never
/// in this list.
pub const DEFAULT_RELAYS: &[&str] = &["https://relay-us.codesmith.skepticdogsoftware.com"];
// When relay-eu exists, it goes here:
// "https://relay-eu.codesmith.skepticdogsoftware.com".

/// The UDP port the relays answer QUIC address discovery on.
pub const RELAY_QUIC_PORT: u16 = 7842;

/// Which relays the gateway uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Relay {
    /// Skeptic Dog's ([`DEFAULT_RELAYS`]).
    Default,
    /// Direct addresses only (LAN and tailnet).
    None,
    /// One relay of your own.
    Url(String),
    /// n0's public relays and address lookup, in development builds only.
    N0,
}

impl Relay {
    /// `"default"`, `"none"`, an `https://` URL (or `http://` to a loopback
    /// address, for tests), or in development builds `"n0"`.
    pub fn parse(s: &str) -> Result<Relay, String> {
        let t = s.trim();
        match t {
            "default" | "" => return Ok(Relay::Default),
            "none" => return Ok(Relay::None),
            "n0" if cfg!(debug_assertions) => return Ok(Relay::N0),
            _ => {}
        }
        let u = url::Url::parse(t)
            .map_err(|_| format!("remote.relay {t:?} is not \"default\", \"none\" or a URL"))?;
        let loopback = match u.host() {
            Some(url::Host::Ipv4(a)) => a.is_loopback(),
            Some(url::Host::Ipv6(a)) => a.is_loopback(),
            Some(url::Host::Domain(d)) => d == "localhost",
            None => false,
        };
        let ok = match u.scheme() {
            "https" => u.host().is_some(),
            "http" => loopback,
            _ => false,
        };
        if !ok || !u.username().is_empty() || u.password().is_some() {
            return Err(format!(
                "remote.relay {t:?} must be an https:// URL without a user name or password"
            ));
        }
        Ok(Relay::Url(u.to_string()))
    }

    /// The relay URLs this setting names (none for `"none"`, and for n0's
    /// preset, which brings its own).
    pub fn urls(&self) -> Vec<String> {
        match self {
            Relay::Default => DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect(),
            Relay::Url(u) => vec![u.clone()],
            Relay::None | Relay::N0 => vec![],
        }
    }
}

/// The settings the gateway and `codesmith remote` read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Off by default.
    pub enabled: bool,
    pub relay: Relay,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            enabled: false,
            relay: Relay::Default,
        }
    }
}

impl Settings {
    /// The settings in a config document's `remote` value. A value of the
    /// wrong type is an error, not a silent default, so a typo can't send
    /// traffic to a relay you didn't mean.
    pub fn from_value(v: Option<&Value>) -> Result<Settings, String> {
        let mut s = Settings::default();
        let Some(v) = v else {
            return Ok(s);
        };
        let Some(o) = v.as_object() else {
            return Err("remote must be an object".into());
        };
        if let Some(e) = o.get("enabled") {
            s.enabled = e.as_bool().ok_or("remote.enabled must be true or false")?;
        }
        if let Some(r) = o.get("relay") {
            let r = r.as_str().ok_or("remote.relay must be a string")?;
            s.relay = Relay::parse(r)?;
        }
        Ok(s)
    }
}

/// The global config file in `dir`, if there is one (`codesmith.json`
/// before `codesmith.jsonc`, as the config loader reads them).
pub fn global_files(dir: &Path) -> Vec<std::path::PathBuf> {
    crate::config::NAMES
        .iter()
        .map(|n| dir.join(n))
        .filter(|p| p.is_file())
        .collect()
}

/// The settings in the global config folder `dir`: each global file's
/// `remote` object merged in the order the loader reads them. With the
/// global config turned off (`$CODESMITH_DISABLE_GLOBAL_CONFIG`), remote
/// access is off.
pub fn load_from(dir: &Path) -> Result<Settings, String> {
    if crate::config::global_disabled() {
        return Ok(Settings::default());
    }
    let mut merged = Value::Object(Default::default());
    for f in global_files(dir) {
        let text = std::fs::read_to_string(&f).map_err(|e| format!("{}: {e}", f.display()))?;
        let v = crate::config::parse(&text, dir).map_err(|e| format!("{}: {e:#}", f.display()))?;
        if let Some(r) = v.get("remote") {
            crate::config::merge(&mut merged, r.clone());
        }
    }
    let empty = merged.as_object().is_some_and(|o| o.is_empty());
    Settings::from_value((!empty).then_some(&merged))
}

/// [`load_from`] the global config folder.
pub fn load() -> Result<Settings, String> {
    load_from(&crate::paths::config_dir())
}

/// Set `remote.enabled` in the global config file in `dir` (codesmith.json,
/// or the codesmith.jsonc there), keeping everything else, comments
/// included. Returns the file written.
pub fn save_enabled(dir: &Path, on: bool) -> anyhow::Result<std::path::PathBuf> {
    use anyhow::anyhow;
    let path = global_files(dir)
        .into_iter()
        .next()
        .unwrap_or_else(|| dir.join(crate::config::NAMES[0]));
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(anyhow!("reading {}: {e}", path.display())),
    };
    let next = set_enabled_text(&text, on).map_err(|e| anyhow!("{}: {e}", path.display()))?;
    if next == text {
        return Ok(path);
    }
    crate::paths::private_dir(dir)?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("codesmith.json")
        .to_string();
    let tmp = dir.join(format!(".{name}.{}.tmp", crate::paths::random_token(6)));
    std::fs::write(&tmp, &next)?;
    match std::fs::metadata(&path) {
        Ok(meta) => {
            let _ = std::fs::set_permissions(&tmp, meta.permissions());
        }
        Err(_) => {
            let _ = crate::paths::tighten(&tmp);
        }
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(path)
}

/// `text` (a JSON or JSONC config, possibly empty) with `remote.enabled`
/// set to `on`, keeping everything else, comments included.
pub fn set_enabled_text(text: &str, on: bool) -> anyhow::Result<String> {
    use anyhow::anyhow;
    let value = if on { "true" } else { "false" };
    if text.trim().is_empty() {
        return Ok(format!("{{\n  \"remote\": {{\"enabled\": {value}}}\n}}\n"));
    }
    let b = text.as_bytes();
    let top = jsonc::skip(b, 0);
    if b.get(top) != Some(&b'{') {
        return Err(anyhow!("the config is not a JSON object"));
    }
    let out = match jsonc::member(text, top, "remote")? {
        None => {
            let empty = b.get(jsonc::skip(b, top + 1)) == Some(&b'}');
            let sep = if empty { "" } else { "," };
            format!(
                "{}\n  \"remote\": {{\"enabled\": {value}}}{sep}{}",
                &text[..top + 1],
                &text[top + 1..]
            )
        }
        Some((start, _)) if b[start] != b'{' => {
            return Err(anyhow!("\"remote\" is not an object"));
        }
        Some((start, _)) => match jsonc::member(text, start, "enabled")? {
            Some((vs, ve)) => format!("{}{value}{}", &text[..vs], &text[ve..]),
            None => {
                let empty = b.get(jsonc::skip(b, start + 1)) == Some(&b'}');
                let sep = if empty { "" } else { ", " };
                format!(
                    "{}\"enabled\": {value}{sep}{}",
                    &text[..start + 1],
                    &text[start + 1..]
                )
            }
        },
    };
    // Only a change that reads back as intended is written.
    let back: Value = serde_json::from_str(&crate::config::strip_jsonc(&out))
        .map_err(|e| anyhow!("the edit would not parse: {e}"))?;
    if back["remote"]["enabled"].as_bool() != Some(on) {
        return Err(anyhow!("the edit did not take"));
    }
    Ok(out)
}

/// Just enough JSONC scanning to find one member's value.
mod jsonc {
    use anyhow::{anyhow, Result};

    /// Past whitespace and comments from `i`.
    pub fn skip(b: &[u8], mut i: usize) -> usize {
        loop {
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if b[i.min(b.len())..].starts_with(b"//") {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            } else if b[i.min(b.len())..].starts_with(b"/*") {
                i += 2;
                while i < b.len() && !b[i..].starts_with(b"*/") {
                    i += 1;
                }
                i = (i + 2).min(b.len());
            } else {
                return i;
            }
        }
    }

    /// Past the closing quote of the string starting at `i`.
    fn string_end(b: &[u8], mut i: usize) -> Result<usize> {
        i += 1;
        while i < b.len() {
            match b[i] {
                b'\\' => i += 2,
                b'"' => return Ok(i + 1),
                _ => i += 1,
            }
        }
        Err(anyhow!("an unterminated string"))
    }

    /// Past the value starting at `i`.
    fn value_end(b: &[u8], i: usize) -> Result<usize> {
        match b.get(i) {
            Some(b'"') => string_end(b, i),
            Some(b'{') | Some(b'[') => {
                let mut depth = 0usize;
                let mut j = i;
                while j < b.len() {
                    j = skip(b, j);
                    match b.get(j) {
                        Some(b'"') => j = string_end(b, j)?,
                        Some(b'{') | Some(b'[') => {
                            depth += 1;
                            j += 1;
                        }
                        Some(b'}') | Some(b']') => {
                            depth -= 1;
                            j += 1;
                            if depth == 0 {
                                return Ok(j);
                            }
                        }
                        Some(_) => j += 1,
                        None => break,
                    }
                }
                Err(anyhow!("an unclosed object"))
            }
            Some(_) => {
                let mut j = i;
                while j < b.len() && !b",}] \t\r\n/".contains(&b[j]) {
                    j += 1;
                }
                Ok(j)
            }
            None => Err(anyhow!("a key without a value")),
        }
    }

    /// The byte range of member `key`'s value in the object starting at
    /// `open` (its `{`), if it has one.
    pub fn member(text: &str, open: usize, key: &str) -> Result<Option<(usize, usize)>> {
        let b = text.as_bytes();
        let mut i = open + 1;
        loop {
            i = skip(b, i);
            match b.get(i) {
                Some(b'}') | None => return Ok(None),
                Some(b',') => {
                    i += 1;
                    continue;
                }
                Some(b'"') => {}
                Some(_) => return Err(anyhow!("a member without a quoted key")),
            }
            let end = string_end(b, i)?;
            let k: String = serde_json::from_str(&text[i..end]).unwrap_or_default();
            let colon = skip(b, end);
            if b.get(colon) != Some(&b':') {
                return Err(anyhow!("a key without a value"));
            }
            let v = skip(b, colon + 1);
            let ve = value_end(b, v)?;
            if k == key {
                return Ok(Some((v, ve)));
            }
            i = ve;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_default_is_off_with_skeptic_dogs_relays_only() {
        let s = Settings::from_value(None).unwrap();
        assert!(!s.enabled);
        assert_eq!(s.relay, Relay::Default);
        let urls = s.relay.urls();
        assert_eq!(urls, ["https://relay-us.codesmith.skepticdogsoftware.com"]);
        for u in DEFAULT_RELAYS {
            assert!(!u.contains("n0") && !u.contains("iroh.link"), "{u}");
        }
    }

    #[test]
    fn relay_values() {
        assert_eq!(Relay::parse("none").unwrap(), Relay::None);
        assert_eq!(Relay::parse("default").unwrap(), Relay::Default);
        assert_eq!(
            Relay::parse("https://relay.example.com").unwrap(),
            Relay::Url("https://relay.example.com/".into())
        );
        assert!(Relay::parse("http://127.0.0.1:3340").is_ok());
        for bad in [
            "http://relay.example.com",
            "ftp://x",
            "relay.example.com",
            "https://user:pw@relay.example.com",
            "file:///etc/passwd",
        ] {
            assert!(Relay::parse(bad).is_err(), "{bad}");
        }
        // n0's preset only in development builds.
        assert_eq!(Relay::parse("n0").is_ok(), cfg!(debug_assertions));
    }

    #[test]
    fn wrong_types_are_errors_not_defaults() {
        assert!(Settings::from_value(Some(&json!(true))).is_err());
        assert!(Settings::from_value(Some(&json!({"enabled": "yes"}))).is_err());
        assert!(Settings::from_value(Some(&json!({"relay": 3}))).is_err());
        let s = Settings::from_value(Some(&json!({"enabled": true, "relay": "none"}))).unwrap();
        assert!(s.enabled);
        assert_eq!(s.relay, Relay::None);
    }

    #[test]
    fn only_the_global_files_are_read() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(load_from(d.path()).unwrap(), Settings::default());
        std::fs::write(
            d.path().join("codesmith.json"),
            r#"{"remote": {"enabled": true}}"#,
        )
        .unwrap();
        std::fs::write(
            d.path().join("codesmith.jsonc"),
            "// mine\n{\"remote\": {\"relay\": \"none\"}}",
        )
        .unwrap();
        // A per-repo file in the global folder is not read.
        std::fs::create_dir_all(d.path().join("projects")).unwrap();
        std::fs::write(
            d.path().join("projects/x.json"),
            r#"{"remote": {"relay": "https://evil.example.com"}}"#,
        )
        .unwrap();
        let s = load_from(d.path()).unwrap();
        assert!(s.enabled);
        assert_eq!(s.relay, Relay::None);
    }

    #[test]
    fn enabling_keeps_the_rest_of_the_file() {
        let cases = [
            ("", true),
            ("{}", true),
            ("{\n  // comment\n  \"model\": \"a/b\"\n}", true),
            ("{\"remote\": {}}", true),
            ("{\"remote\": {\"relay\": \"none\"}}", true),
            (
                "{\"remote\": {\"enabled\": false, \"relay\": \"none\"}}",
                true,
            ),
            ("{\"remote\": {\"enabled\": true /* on */}}", false),
            (
                "{\"a\": [1, {\"remote\": 2}], \"remote\": {\"x\": {\"enabled\": 1}}}",
                true,
            ),
        ];
        for (text, on) in cases {
            let out = set_enabled_text(text, on).unwrap();
            let v: Value = serde_json::from_str(&crate::config::strip_jsonc(&out)).unwrap();
            assert_eq!(v["remote"]["enabled"], json!(on), "{text} -> {out}");
            if text.contains("comment") {
                assert!(
                    out.contains("// comment") && out.contains("\"a/b\""),
                    "{out}"
                );
            }
            if text.contains("\"x\"") {
                assert_eq!(v["remote"]["x"]["enabled"], json!(1), "{out}");
                assert_eq!(v["a"][1]["remote"], json!(2), "{out}");
            }
            if text.contains("relay") {
                assert_eq!(v["remote"]["relay"], json!("none"), "{out}");
            }
        }
        assert!(set_enabled_text("{\"remote\": true}", true).is_err());
        assert!(set_enabled_text("[1]", true).is_err());
        assert!(set_enabled_text("{\"remote\": {\"enabled\": ", true).is_err());
    }

    #[test]
    fn save_enabled_writes_the_global_file() {
        let d = tempfile::tempdir().unwrap();
        let p = save_enabled(d.path(), true).unwrap();
        assert_eq!(p, d.path().join("codesmith.json"));
        assert!(load_from(d.path()).unwrap().enabled);
        save_enabled(d.path(), false).unwrap();
        assert!(!load_from(d.path()).unwrap().enabled);
    }

    /// Bounded property test: whatever members surround it, the edit sets
    /// `remote.enabled` and changes nothing else.
    #[test]
    fn enabling_any_small_config_changes_only_enabled() {
        use rand::{Rng, SeedableRng};
        let mut rng = rand::rngs::StdRng::seed_from_u64(7);
        let values = [
            json!(1),
            json!("s"),
            json!(null),
            json!([1, "}"]),
            json!({"enabled": "x"}),
            json!({"remote": {"enabled": false}}),
        ];
        for _ in 0..300 {
            let mut o = serde_json::Map::new();
            for k in 0..rng.gen_range(0..5) {
                o.insert(
                    format!("k{k}"),
                    values[rng.gen_range(0..values.len())].clone(),
                );
            }
            if rng.gen_bool(0.5) {
                let mut r = serde_json::Map::new();
                if rng.gen_bool(0.5) {
                    r.insert("enabled".into(), json!(rng.gen_bool(0.5)));
                }
                if rng.gen_bool(0.5) {
                    r.insert("relay".into(), json!("none"));
                }
                o.insert("remote".into(), Value::Object(r));
            }
            let before = Value::Object(o);
            let text = if rng.gen_bool(0.5) {
                serde_json::to_string_pretty(&before).unwrap()
            } else {
                serde_json::to_string(&before).unwrap()
            };
            let on = rng.gen_bool(0.5);
            let out = set_enabled_text(&text, on).unwrap();
            let mut after: Value = serde_json::from_str(&out).unwrap();
            assert_eq!(after["remote"]["enabled"], json!(on));
            after["remote"].as_object_mut().unwrap().remove("enabled");
            let mut want = before.clone();
            match want.get_mut("remote").and_then(Value::as_object_mut) {
                Some(r) => {
                    r.remove("enabled");
                }
                None => {
                    want["remote"] = json!({});
                }
            }
            assert_eq!(after, want, "{text}");
        }
    }
}
