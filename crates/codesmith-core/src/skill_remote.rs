//! Skills downloaded from a URL (OpenCode's `SkillDiscovery`): an entry
//! of `skills` in config that is an `http(s)://` URL names a catalog whose
//! `index.json` lists skills as `{"skills": [{"name", "version", "files"}]}`.
//! Each skill's files (`<url>/<name>/<file>`, one of them `SKILL.md`) are
//! downloaded into the cache folder, and the skill folders join the skill
//! roots where the entry sits in `skills`.
//!
//! Names and file paths are checked before anything is fetched: a name is
//! one path segment, a file a relative path below the skill's folder on the
//! catalog's own origin; a skill with any unsafe entry is skipped whole. A
//! skill without a `version` downloads only the files it lacks; a new
//! `version` downloads every file into a staging folder that replaces the
//! cached skill only once all of them arrived (so a partial update keeps
//! the old copy, and files the new version dropped go). A catalog that
//! cannot be read adds no skills; the others still load.
//!
//! Downloads are capped: [`MAX_FILE`] per file (and for the index) and
//! [`MAX_SKILL`] for one skill's files together, counted as they stream; a
//! skill that goes over is not updated (a new version keeps the old copy).
//! Redirects are followed only on the same origin.
//!
//! Deliberate difference: single-file `<name>.md` skills are not
//! supported (as for local skills), so a skill needs a `SKILL.md`.

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The marker file holding a downloaded skill's version.
const VERSION_FILE: &str = ".codesmith-version";

/// The most one downloaded file (or a catalog's index) may hold.
pub const MAX_FILE: u64 = 1024 * 1024;
/// The most one skill's files may hold together.
pub const MAX_SKILL: u64 = 8 * 1024 * 1024;

#[derive(serde::Deserialize)]
struct Index {
    skills: Vec<IndexSkill>,
}

#[derive(serde::Deserialize)]
struct IndexSkill {
    name: String,
    version: Option<String>,
    files: Vec<String>,
}

/// Whether `s` is a config `skills` entry naming a catalog URL.
pub fn is_url(s: &str) -> bool {
    s.starts_with("https://") || s.starts_with("http://")
}

/// One path segment: not empty, `.` or `..`, and without separators.
pub fn is_safe_segment(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && !s.contains(['/', '\\', '\0'])
}

/// A relative path below a folder: no URL, absolute path, query, fragment,
/// backslash or unsafe (percent-decoded) segment.
pub fn is_safe_relative_path(s: &str) -> bool {
    let scheme = s
        .split_once(':')
        .is_some_and(|(head, _)| !head.is_empty() && !head.contains('/'));
    !s.is_empty()
        && !s.contains(['\\', '\0', '?', '#'])
        && !scheme
        && !s.starts_with('/')
        && s.split('/').all(|seg| match percent_decode(seg) {
            Some(d) => is_safe_segment(&d),
            None => false,
        })
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The downloaded skill folders of catalog `url`, pulled once per process
/// (later calls reuse the first pull's folders). Runs the download on its
/// own thread, so it can be called from anywhere.
pub fn pulled(url: &str) -> Vec<PathBuf> {
    static PULLED: Mutex<Option<HashMap<String, Vec<PathBuf>>>> = Mutex::new(None);
    if let Some(dirs) = PULLED
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .get(url)
    {
        return dirs.clone();
    }
    let owned = url.to_string();
    let cache = crate::paths::cache_dir();
    let dirs = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map(|rt| rt.block_on(pull(&owned, &cache)))
            .unwrap_or_default()
    })
    .join()
    .unwrap_or_default();
    PULLED
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(url.to_string(), dirs.clone());
    dirs
}

/// Download catalog `url`'s skills under `cache` (`<cache>/skills/<hash of
/// the URL>/<name>/`) and return the folders of those that have a
/// `SKILL.md`, in index order.
pub async fn pull(url: &str, cache: &Path) -> Vec<PathBuf> {
    let base = if url.ends_with('/') {
        url.to_string()
    } else {
        format!("{url}/")
    };
    let Ok(source) = url::Url::parse(&base) else {
        return Vec::new();
    };
    let Ok(index_url) = source.join("index.json") else {
        return Vec::new();
    };
    // Redirects stay on the origin they start from.
    let same_origin = reqwest::redirect::Policy::custom(|attempt| {
        let from = attempt.previous().first().map(|u| u.origin());
        if attempt.previous().len() < 10 && from == Some(attempt.url().origin()) {
            attempt.follow()
        } else {
            attempt.stop()
        }
    });
    let client = match reqwest::Client::builder()
        .redirect(same_origin)
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(60))
        .build()
    {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let Some(index) = get(&client, index_url.as_str(), MAX_FILE)
        .await
        .and_then(|b| serde_json::from_slice::<Index>(&b).ok())
    else {
        return Vec::new();
    };
    let hash = format!("{:x}", Sha256::digest(base.as_bytes()));
    let source_root = cache.join("skills").join(&hash[..16]);
    let mut out = Vec::new();
    for skill in index.skills {
        let Some(files) = plan(&source, &skill) else {
            continue;
        };
        let root = source_root.join(&skill.name);
        let current = std::fs::read_to_string(root.join(VERSION_FILE)).ok();
        match &skill.version {
            Some(v) if current.as_deref() != Some(v.as_str()) => {
                refresh(&client, &root, v, &files).await;
            }
            _ => {
                // What the skill already holds counts toward its cap.
                let mut left = MAX_SKILL.saturating_sub(
                    files
                        .iter()
                        .filter_map(|(_, f)| std::fs::metadata(root.join(f)).ok())
                        .map(|m| m.len())
                        .sum(),
                );
                for (url, file) in &files {
                    let dest = root.join(file);
                    if !dest.exists() {
                        match download(&client, url, &dest, left).await {
                            Some(n) => left -= n,
                            None if left == 0 => break,
                            None => {}
                        }
                    }
                }
            }
        }
        if root.join("SKILL.md").is_file() {
            out.push(root);
        }
    }
    out
}

/// Each file's URL and relative path, or `None` when anything is unsafe.
fn plan(source: &url::Url, skill: &IndexSkill) -> Option<Vec<(String, String)>> {
    if !is_safe_segment(&skill.name) || !skill.files.iter().any(|f| f == "SKILL.md") {
        return None;
    }
    let skill_url = source
        .join(&format!("{}/", url_segment(&skill.name)))
        .ok()?;
    skill
        .files
        .iter()
        .map(|f| {
            if !is_safe_relative_path(f) {
                return None;
            }
            let u = skill_url.join(f).ok()?;
            (u.origin() == source.origin() && u.as_str().starts_with(skill_url.as_str()))
                .then(|| (u.to_string(), f.clone()))
        })
        .collect()
}

fn url_segment(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Download every file into a staging folder; once all arrived, swap it in
/// for `root` with the new version recorded.
async fn refresh(client: &reqwest::Client, root: &Path, version: &str, files: &[(String, String)]) {
    let token = crate::paths::random_token(6);
    let name = root.file_name().unwrap_or_default().to_string_lossy();
    let staging = root.with_file_name(format!("{name}.tmp-{token}"));
    let backup = root.with_file_name(format!("{name}.old-{token}"));
    let mut ok = true;
    let mut left = MAX_SKILL;
    for (url, file) in files {
        match download(client, url, &staging.join(file), left).await {
            Some(n) => left -= n,
            None => {
                ok = false;
                break;
            }
        }
    }
    if ok
        && staging.join("SKILL.md").is_file()
        && std::fs::write(staging.join(VERSION_FILE), version).is_ok()
    {
        let cached = root.exists();
        if !cached || std::fs::rename(root, &backup).is_ok() {
            if std::fs::rename(&staging, root).is_err() && cached {
                let _ = std::fs::rename(&backup, root);
            }
            let _ = std::fs::remove_dir_all(&backup);
        }
    }
    let _ = std::fs::remove_dir_all(&staging);
}

/// GET `url`, retrying twice on errors and bad statuses; `None` for a
/// body over `cap` bytes (stopped as soon as it goes over) or a redirect
/// that was not followed.
async fn get(client: &reqwest::Client, url: &str, cap: u64) -> Option<Vec<u8>> {
    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(200 << attempt)).await;
        }
        let Ok(mut resp) = client.get(url).send().await else {
            continue;
        };
        let status = resp.status();
        if status.is_success() {
            if resp.content_length().is_some_and(|n| n > cap) {
                return None;
            }
            let mut body = Vec::new();
            let complete = loop {
                match resp.chunk().await {
                    Ok(Some(c)) => {
                        if body.len() as u64 + c.len() as u64 > cap {
                            return None;
                        }
                        body.extend_from_slice(&c);
                    }
                    Ok(None) => break true,
                    Err(_) => break false,
                }
            };
            if complete {
                return Some(body);
            }
        } else if status.is_redirection() || (status.is_client_error() && status.as_u16() != 429) {
            return None;
        }
    }
    None
}

/// Download `url` to `dest`, at most `left` bytes (and [`MAX_FILE`]): the
/// bytes written, or `None` when it failed or was too large.
async fn download(client: &reqwest::Client, url: &str, dest: &Path, left: u64) -> Option<u64> {
    let body = get(client, url, left.min(MAX_FILE)).await?;
    let written = dest
        .parent()
        .is_some_and(|p| std::fs::create_dir_all(p).is_ok())
        && std::fs::write(dest, &body).is_ok();
    written.then_some(body.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_paths() {
        for ok in ["SKILL.md", "references/guide.md", "a%20b.md"] {
            assert!(is_safe_relative_path(ok), "{ok}");
        }
        for bad in [
            "",
            "../outside.md",
            "/tmp/outside.md",
            "https://evil.example.test/x.md",
            "a/%2e%2e/b",
            "a/%2Fb",
            "a\\b",
            "a?b",
            "a#b",
            "a//b",
            "C:/x",
        ] {
            assert!(!is_safe_relative_path(bad), "{bad}");
        }
        assert!(is_safe_segment("deploy"));
        assert!(!is_safe_segment("../outside"));
    }
}
