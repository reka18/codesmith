//! Ports of OpenCode's attachment lowering tests
//! (`session-runner-message.test.ts`): local source paths announced next
//! to provider media, and duplicate labeled images sent once.

mod support;

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

#[test]
fn oc_runner_message_deduplicates_provider_media_while_preserving_durable_attachment_references() {
    let described = Attached {
        description: Some("alternate use".into()),
        ..Attached::inline(png("image.png")).labeled("[Image 2]")
    };
    let parts = attachment::parts(vec![
        Attached::inline(png("image.png")).labeled("[Image 1]"),
        Attached::inline(png("image.png")).labeled("[Image 1]"),
        described,
    ]);
    assert_eq!(media(&parts), 2);
    assert!(texts(&parts).is_empty());
}

#[test]
fn oc_runner_message_preserves_provider_media_with_distinct_labels_or_uri_sources() {
    let parts = attachment::parts(vec![
        Attached::inline(png("image.png")).labeled("[Image 1]"),
        Attached::inline(png("image.png")).labeled("[Image 2]"),
        with_source(png("image.png"), "file:///project/image.png").labeled("[Image 1]"),
        Attached::inline(png("image.png")),
    ]);
    assert_eq!(media(&parts), 4);
}

#[test]
fn oc_runner_message_exposes_admitted_reference_directory_source_paths_in_model_context() {
    let location = std::path::Path::new("/references/harness-engineering");
    let parts = attachment::parts(vec![Attached::directory(location, "lib/\nindex.ts")]);
    assert_eq!(
        texts(&parts),
        ["Attached directory: /references/harness-engineering\n\nlib/\nindex.ts"]
    );
    assert_eq!(media(&parts), 0, "a directory is text, not media");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_directory_attached_by_path_is_listed_folders_first() {
    let h = support::H::new().await;
    let dir = h.root().join("docs");
    std::fs::create_dir_all(dir.join("lib")).unwrap();
    std::fs::write(dir.join("index.ts"), "").unwrap();
    let a = h.s.attach_local("docs").unwrap().unwrap();
    assert_eq!(
        texts(&attachment::parts(vec![a])),
        [format!(
            "Attached directory: {}\n\nlib/\nindex.ts",
            dir.display()
        )]
    );
}
