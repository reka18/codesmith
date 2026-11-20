//! Ports of OpenCode's attachment lowering tests
//! (`session-runner-message.test.ts`): local source paths announced next
//! to provider media.

use codesmith_ai::{Media, Part};
use codesmith_core::attachment::{self, Attached};

const DATA: &str = "AAECAw==";

fn png(name: &str) -> Media {
    Media::base64("image/png", DATA).named(name)
}

fn with_source(m: Media, source: &str) -> Attached {
    Attached {
        source: Some(source.into()),
        ..Attached::inline(m)
    }
}

fn texts(parts: &[Part]) -> Vec<String> {
    parts
        .iter()
        .filter_map(|p| match p {
            Part::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect()
}

fn media(parts: &[Part]) -> usize {
    parts.iter().filter(|p| matches!(p, Part::Media(_))).count()
}

#[test]
fn oc_runner_message_exposes_admitted_local_image_source_paths_before_provider_media() {
    let parts = attachment::parts(vec![with_source(
        png("IMG_3480.JPG"),
        "file:///project/IMG_3480.JPG",
    )]);
    assert_eq!(parts.len(), 2);
    assert_eq!(texts(&parts), ["Attached file: /project/IMG_3480.JPG"]);
    assert!(matches!(&parts[1], Part::Media(m) if m.filename.as_deref() == Some("IMG_3480.JPG")));
    // An absolute path is a local source too.
    let parts = attachment::parts(vec![Attached::local(
        png("a.png"),
        std::path::Path::new("/project/a.png"),
    )]);
    assert_eq!(texts(&parts), ["Attached file: /project/a.png"]);
}

#[test]
fn oc_runner_message_falls_back_to_attachment_names_for_invalid_local_source_paths() {
    let parts = attachment::parts(vec![with_source(
        png("preview.png"),
        "file:///project/image%2Fpreview.png",
    )]);
    assert!(
        texts(&parts).is_empty(),
        "no location text for an invalid path"
    );
    assert!(matches!(&parts[0], Part::Media(m) if m.filename.as_deref() == Some("preview.png")));
    assert_eq!(attachment::local_path("file:///project/src%2Flib"), None);
    assert_eq!(
        attachment::local_path("file:///project/a%20b.png").as_deref(),
        Some("/project/a b.png")
    );
}

#[test]
fn oc_runner_message_does_not_add_attachment_location_text_for_non_local_provider_media() {
    let parts = attachment::parts(vec![with_source(
        png("image.png"),
        "https://example.com/image.png",
    )]);
    assert_eq!(parts.len(), 1);
    assert_eq!(media(&parts), 1);
}
