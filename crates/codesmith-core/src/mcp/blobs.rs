//! Binary content in MCP results (resource blobs, audio) that cannot go to
//! the model as an image or PDF: text decoded in place, anything else saved
//! to a file in the session's private tool-output folder, as OpenCode saves
//! it. The file's name is always CodeSmith's (`tool_<id>` and an extension
//! from a fixed table by media type): nothing a server sends (its URI, its
//! name for the content) goes into a path.

use std::io::Write;
use std::path::{Path, PathBuf};

/// A media type as a server named it, if it is one (`type/subtype` of
/// token characters), lowercased and without parameters.
pub fn mime(m: Option<&str>) -> Option<String> {
    let essence = m?.split(';').next()?.trim().to_ascii_lowercase();
    let (t, s) = essence.split_once('/')?;
    let token = |p: &str| {
        !p.is_empty()
            && p.len() <= 127
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$&-^_.+".contains(&b))
    };
    (token(t) && token(s)).then_some(essence)
}

/// Whether content of media type `m` is text to show as it is.
pub fn textual(m: &str) -> bool {
    m.starts_with("text/")
        || m.ends_with("+json")
        || m.ends_with("+xml")
        || matches!(
            m,
            "application/json"
                | "application/xml"
                | "application/javascript"
                | "application/ecmascript"
                | "application/x-javascript"
                | "application/yaml"
                | "application/x-yaml"
                | "application/toml"
                | "application/sql"
                | "application/graphql"
                | "application/x-sh"
                | "application/x-ndjson"
                | "application/csv"
        )
}

/// The file extension for media type `m` (with its dot), or none.
pub fn extension(m: &str) -> &'static str {
    match m {
        "application/zip" => ".zip",
        "application/gzip" | "application/x-gzip" => ".gz",
        "application/x-tar" => ".tar",
        "application/x-7z-compressed" => ".7z",
        "application/vnd.apache.parquet" | "application/x-parquet" => ".parquet",
        "application/vnd.sqlite3" | "application/x-sqlite3" => ".sqlite",
        "application/wasm" => ".wasm",
        "application/msword" => ".doc",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => ".docx",
        "application/vnd.ms-excel" => ".xls",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => ".xlsx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => ".pptx",
        "application/octet-stream" => ".bin",
        "audio/mpeg" => ".mp3",
        "audio/wav" | "audio/x-wav" | "audio/wave" => ".wav",
        "audio/ogg" => ".ogg",
        "audio/flac" => ".flac",
        "audio/aac" => ".aac",
        "audio/webm" | "video/webm" => ".webm",
        "audio/mp4" => ".m4a",
        "video/mp4" => ".mp4",
        "video/quicktime" => ".mov",
        "image/png" => ".png",
        "image/jpeg" => ".jpg",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        "image/svg+xml" => ".svg",
        "image/tiff" => ".tiff",
        "image/bmp" => ".bmp",
        "font/woff2" => ".woff2",
        _ => "",
    }
}

/// Save `bytes` in `dir` (created owner-only) as a new owner-only file
/// named by CodeSmith; it never replaces or follows an existing one.
pub fn save(dir: &Path, bytes: &[u8], mime: &str) -> std::io::Result<PathBuf> {
    crate::paths::private_dir(dir)?;
    let ext = extension(mime);
    for _ in 0..8 {
        let name = format!("tool_{}{ext}", crate::tool::unique_id());
        let file = dir.join(&name);
        // The name is ours, but check anyway that it stays a plain file
        // name in `dir`.
        if file.parent() != Some(dir) || file.file_name().and_then(|n| n.to_str()) != Some(&name) {
            return Err(std::io::Error::other("unexpected file name"));
        }
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        match o.open(&file) {
            Ok(mut f) => {
                f.write_all(bytes)?;
                return Ok(file);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("could not pick a file name"))
}

/// How a blob that is not media reads in a result: its text when it is
/// text, else where it was saved (with `dir`). `None` without `dir` for
/// binary content, which the caller notes as before.
pub fn describe(uri: &str, m: Option<&str>, data: &str, dir: Option<&Path>) -> Option<String> {
    let label = super::oauth::clean(uri, 300);
    let m = mime(m).unwrap_or_else(|| "application/octet-stream".into());
    let Some(bytes) = crate::media::unbase64(data.trim()) else {
        return Some(format!("[Resource {label} ({m}) omitted: invalid base64]"));
    };
    if textual(&m) {
        if let Ok(text) = String::from_utf8(bytes.clone()) {
            return Some(format!("[Resource {label} ({m})]\n{text}"));
        }
    }
    let dir = dir?;
    let n = bytes.len();
    Some(match save(dir, &bytes, &m) {
        Ok(file) => format!(
            "[Binary resource {label} ({m}, {n} bytes) saved to {}]",
            file.display()
        ),
        Err(e) => format!("[Binary resource {label} ({m}, {n} bytes) could not be saved: {e}]"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_types() {
        assert_eq!(
            mime(Some("Text/Markdown; charset=utf-8")).as_deref(),
            Some("text/markdown")
        );
        assert_eq!(
            mime(Some("application/vnd.apache.parquet")).as_deref(),
            Some("application/vnd.apache.parquet")
        );
        for bad in [
            "",
            "text",
            "/x",
            "x/",
            "a/b/c",
            "../../x",
            "a/b c",
            "ü/x",
            "text/\u{1b}[31m",
        ] {
            assert_eq!(mime(Some(bad)), None, "{bad:?}");
        }
        assert_eq!(mime(None), None);
        assert!(
            textual("text/plain") && textual("application/ld+json") && textual("image/svg+xml")
        );
        assert!(!textual("application/octet-stream") && !textual("audio/wav"));
        assert_eq!(extension("application/vnd.apache.parquet"), ".parquet");
        assert_eq!(extension("application/x-unknown"), "");
    }

    #[test]
    fn saved_files_are_private_and_named_by_codesmith() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("ses_x");
        let f = save(&dir, b"PAR1", "application/vnd.apache.parquet").unwrap();
        assert_eq!(f.parent(), Some(dir.as_path()));
        let name = f.file_name().unwrap().to_str().unwrap();
        assert!(
            name.starts_with("tool_") && name.ends_with(".parquet"),
            "{name}"
        );
        assert_eq!(std::fs::read(&f).unwrap(), b"PAR1");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        // Two saves never share a file.
        let g = save(&dir, b"", "application/x-unknown").unwrap();
        assert_ne!(f, g);
        assert!(!g.to_str().unwrap().ends_with('.'));
        assert_eq!(std::fs::read(&g).unwrap(), b"");
    }

    #[test]
    fn server_names_never_reach_the_path() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("out");
        for uri in [
            "file:///../../etc/passwd",
            "../../../../tmp/evil",
            "/etc/passwd",
            "x\u{0}y",
            "C:\\Windows\\evil.dll",
        ] {
            let text = describe(uri, Some("application/zip"), "UEsDBA==", Some(&dir)).unwrap();
            let saved = text
                .rsplit("saved to ")
                .next()
                .unwrap()
                .trim_end_matches(']');
            let p = Path::new(saved);
            assert_eq!(p.parent(), Some(dir.as_path()), "{uri}: {text}");
            assert!(p
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("tool_"));
            assert!(!text.contains('\u{0}'));
        }
        // A media type that tries to steer the extension is not one.
        let text = describe("x", Some("application/../../evil"), "AAEC", Some(&dir)).unwrap();
        assert!(
            text.contains("(application/octet-stream, 3 bytes)"),
            "{text}"
        );
        assert_eq!(
            std::fs::read_dir(d.path()).unwrap().count(),
            1,
            "only `out`"
        );
    }
}
