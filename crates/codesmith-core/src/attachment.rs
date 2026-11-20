//! Files attached to a prompt, as the user message carries them (after
//! OpenCode's `session/runner/to-llm-message.ts`): an image or PDF taken
//! from a local file is announced by its path (`Attached file: <path>`)
//! just before its media, so the model can refer to the file itself.

use codesmith_ai::{Media, MediaKind, Part};

/// A file attached to a prompt.
#[derive(Debug, Clone, PartialEq)]
pub struct Attached {
    pub media: Media,
    /// Where it came from: a `file://` URI or an absolute path for a local
    /// file, another URI (`https://…`) for a remote one, `None` for inline
    /// data (pasted, or sent as base64).
    pub source: Option<String>,
    /// The placeholder the prompt names it by (`[Image 1]`).
    pub label: Option<String>,
    /// What the user said it is.
    pub description: Option<String>,
}

impl Attached {
    /// Inline media, without a source or label.
    pub fn inline(media: Media) -> Attached {
        Attached {
            media,
            source: None,
            label: None,
            description: None,
        }
    }

    /// Media read from a local file at `path`.
    pub fn local(media: Media, path: &std::path::Path) -> Attached {
        Attached {
            source: Some(path.display().to_string()),
            ..Attached::inline(media)
        }
    }

    pub fn labeled(mut self, label: &str) -> Attached {
        self.label = Some(label.into());
        self
    }
}

/// The local path a source names: an absolute path, or a `file://` URI
/// that decodes to one. `None` for remote sources and for a URI that is not
/// a valid file path (an encoded `/` inside a segment).
pub fn local_path(source: &str) -> Option<String> {
    if std::path::Path::new(source).is_absolute() {
        return Some(source.to_string());
    }
    let u = url::Url::parse(source).ok()?;
    if u.scheme() != "file" {
        return None;
    }
    // OpenCode's fileURLToPath rejects an encoded separator.
    if u.path().to_ascii_lowercase().contains("%2f") {
        return None;
    }
    u.to_file_path().ok().map(|p| p.display().to_string())
}

fn announced(a: &Attached) -> bool {
    matches!(a.media.kind(), MediaKind::Image | MediaKind::Pdf)
}

/// The parts that follow the prompt's text: each attachment's media, a
/// local image or PDF preceded by `Attached file: <path>`.
pub fn parts(files: Vec<Attached>) -> Vec<Part> {
    let mut out = Vec::new();
    for f in files {
        if announced(&f) {
            if let Some(p) = f.source.as_deref().and_then(local_path) {
                out.push(Part::text(format!("Attached file: {p}")));
            }
        }
        out.push(Part::Media(f.media));
    }
    out
}
