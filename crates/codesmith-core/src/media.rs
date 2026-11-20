//! Images, PDFs and video for the model: detection, resizing images to
//! provider limits, and what a model that cannot take them gets instead.
//!
//! Ported from OpenCode (MIT): `core/src/image/photon-normalize.ts` (resize
//! and transcode), `core/src/tool.ts` (`normalizeImages`),
//! `core/src/session/model-request.ts` (`unsupportedParts`, `boundImages`)
//! and `core/src/session/prompt.ts` (attachments). Resizing uses the pure
//! Rust `image` crate where OpenCode uses photon.

use base64::Engine;
use codesmith_ai::{Media, MediaKind, Message, Part};
use image::{DynamicImage, GenericImageView, ImageFormat};
use serde::Deserialize;
use std::path::Path;

/// Largest file `read` or an attachment takes as media (OpenCode's
/// `MAX_MEDIA_INGEST_BYTES` and `MAX_ATTACHMENT_BYTES`).
pub const MAX_INGEST_BYTES: u64 = 20 * 1024 * 1024;

/// What images are fitted to before they are sent (`media.image` in the
/// config; OpenCode's `Image.Limits`).
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(default)]
pub struct ImageLimits {
    /// Shrink and re-encode images that are too large; when off they are
    /// dropped instead.
    pub auto_resize: bool,
    pub max_width: u32,
    pub max_height: u32,
    /// The base64 size an image may have.
    pub max_base64_bytes: usize,
}

impl Default for ImageLimits {
    fn default() -> Self {
        ImageLimits {
            auto_resize: true,
            max_width: 2_000,
            max_height: 2_000,
            max_base64_bytes: 5 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct MediaConfig {
    pub image: ImageLimits,
}

/// How many leading bytes `detect` needs to tell every type apart (a WebM
/// file names its doctype a few dozen bytes in).
pub const SNIFF_BYTES: usize = 64;

/// The media type sent natively, detected from the bytes (OpenCode's
/// `Mime.detectMedia`, limited to its `MEDIA_MIMES`, plus the video its
/// models take: MP4 and QuickTime by their `ftyp` box, WebM by its EBML
/// doctype). AVIF, HEIC and M4A share MP4's container and are not media
/// here. Give it at least the first `SNIFF_BYTES` bytes.
pub fn detect(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        Some("image/png")
    } else if b.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if b.starts_with(b"%PDF-") {
        Some("application/pdf")
    } else if b.starts_with(b"RIFF") && b.len() >= 12 && &b[8..12] == b"WEBP" {
        Some("image/webp")
    } else if b.len() >= 12 && &b[4..8] == b"ftyp" {
        // The major brand tells the ISO media files apart (OpenCode's rule).
        match &b[8..12] {
            b"avif" | b"avis" | b"mif1" | b"msf1" | b"heic" | b"heix" | b"hevc" | b"hevx" => None,
            b"M4A " => None,
            b"qt  " => Some("video/quicktime"),
            _ => Some("video/mp4"),
        }
    } else if b.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        // Matroska and WebM share EBML; only WebM is taken.
        let head = &b[..b.len().min(SNIFF_BYTES)];
        head.windows(4)
            .any(|w| w == b"webm")
            .then_some("video/webm")
    } else {
        None
    }
}

/// What `read` (and an attachment read as if by `read`) tells the model
/// along with `m`, as OpenCode words it.
pub fn read_note(m: &Media) -> &'static str {
    match m.kind() {
        MediaKind::Pdf => "PDF read successfully",
        MediaKind::Video => "Video read successfully",
        _ => "Image read successfully",
    }
}

pub fn base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Base64 (standard alphabet, padded) decoded, or `None` when it is not.
pub fn unbase64(data: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(data).ok()
}

/// Why an image could not be sent.
#[derive(Debug, Clone, PartialEq)]
pub enum ImageError {
    Decode,
    Size {
        width: u32,
        height: u32,
        bytes: usize,
    },
}

impl ImageError {
    pub fn message(&self, resource: &str, limits: &ImageLimits) -> String {
        match self {
            ImageError::Decode => format!("Image could not be decoded: {resource}"),
            ImageError::Size {
                width,
                height,
                bytes,
            } => format!(
                "Image {resource} is {width}x{height} with base64 size {bytes}, exceeding configured limits {}x{}/{} bytes",
                limits.max_width, limits.max_height, limits.max_base64_bytes
            ),
        }
    }
}

/// Images that may be resized; others pass through untouched.
fn resizable(mime: &str) -> bool {
    matches!(
        mime,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    )
}

/// PNGs this small are kept as they are even when JPEG would be smaller.
const PNG_PASSTHROUGH_MAX_BASE64_BYTES: usize = 384 * 1024;
const SMALL_PNG_MAX_PIXELS: u64 = 256 * 256;
const JPEG_QUALITIES: [u8; 4] = [80, 70, 55, 40];

fn base64_len(n: usize) -> usize {
    n.div_ceil(3) * 4
}

/// More than 5% of an evenly spaced grid of up to 64x64 pixels is
/// noticeably transparent.
fn has_transparency(img: &DynamicImage) -> bool {
    if !img.color().has_alpha() {
        return false;
    }
    let (w, h) = img.dimensions();
    let (cols, rows) = (w.min(64), h.min(64));
    let mut transparent = 0u32;
    for row in 0..rows {
        let y = (((row as f64 + 0.5) * h as f64) / rows as f64) as u32;
        for col in 0..cols {
            let x = (((col as f64 + 0.5) * w as f64) / cols as f64) as u32;
            if img.get_pixel(x.min(w - 1), y.min(h - 1))[3] < 250 {
                transparent += 1;
            }
        }
    }
    transparent as f64 / (cols * rows) as f64 > 0.05
}

fn encode_png(img: &DynamicImage) -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    let _ = img.write_to(&mut out, ImageFormat::Png);
    out.into_inner()
}

fn encode_jpeg(img: &DynamicImage, quality: u8) -> Vec<u8> {
    let mut out = Vec::new();
    let rgb = img.to_rgb8();
    let _ =
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality).encode_image(&rgb);
    out
}

/// Fit an image to `limits` (OpenCode's `PhotonNormalize.normalize`).
/// `Ok(None)` keeps it as it is; `Ok(Some((mime, base64)))` replaces it.
/// Images that fit are kept, except large opaque PNGs, which become JPEG
/// when that is smaller. Larger ones are scaled into the box and stepped
/// down by a quarter until an encoding (PNG first for transparent or small
/// images, else JPEG at falling quality) fits the byte limit.
pub fn normalize(
    mime: &str,
    data: &str,
    limits: &ImageLimits,
) -> Result<Option<(String, String)>, ImageError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|_| ImageError::Decode)?;
    let img = image::load_from_memory(&bytes).map_err(|_| ImageError::Decode)?;
    let (width, height) = img.dimensions();
    let size = data.len();
    let too_big = ImageError::Size {
        width,
        height,
        bytes: size,
    };
    let fits =
        width <= limits.max_width && height <= limits.max_height && size <= limits.max_base64_bytes;
    let small_png = size <= PNG_PASSTHROUGH_MAX_BASE64_BYTES
        || width as u64 * height as u64 <= SMALL_PNG_MAX_PIXELS;
    if fits && (!limits.auto_resize || mime != "image/png" || small_png) {
        return Ok(None);
    }
    if !limits.auto_resize {
        return Err(too_big);
    }
    let transparent = mime != "image/jpeg" && has_transparency(&img);
    if fits {
        if transparent {
            return Ok(None);
        }
        let jpeg = encode_jpeg(&img, JPEG_QUALITIES[0]);
        return Ok((base64_len(jpeg.len()) < size).then(|| ("image/jpeg".into(), base64(&jpeg))));
    }
    let scale = 1f64
        .min(limits.max_width as f64 / width as f64)
        .min(limits.max_height as f64 / height as f64);
    let mut sizes: Vec<(u32, u32)> = Vec::new();
    for _ in 0..32 {
        let next = match sizes.last() {
            None => (
                ((width as f64 * scale).round() as u32).max(1),
                ((height as f64 * scale).round() as u32).max(1),
            ),
            Some(&(w, h)) => (
                if w == 1 {
                    1
                } else {
                    ((w as f64 * 0.75).floor() as u32).max(1)
                },
                if h == 1 {
                    1
                } else {
                    ((h as f64 * 0.75).floor() as u32).max(1)
                },
            ),
        };
        if !sizes.contains(&next) {
            sizes.push(next);
        }
    }
    for (w, h) in sizes {
        let resized = if (w, h) == (width, height) {
            img.clone()
        } else {
            img.resize_exact(w, h, image::imageops::FilterType::Lanczos3)
        };
        let png_first = transparent || w as u64 * h as u64 <= SMALL_PNG_MAX_PIXELS;
        let mut order: Vec<Option<u8>> = JPEG_QUALITIES.iter().copied().map(Some).collect();
        if png_first {
            order.insert(0, None);
        } else {
            order.push(None);
        }
        for q in order {
            let (mime, encoded) = match q {
                None => ("image/png", encode_png(&resized)),
                Some(q) => ("image/jpeg", encode_jpeg(&resized, q)),
            };
            if base64_len(encoded.len()) <= limits.max_base64_bytes {
                return Ok(Some((mime.into(), base64(&encoded))));
            }
        }
    }
    Err(too_big)
}

/// Fit the images among a tool's media to `limits` (OpenCode's
/// `normalizeImages`). Images that cannot be decoded or fitted are dropped,
/// and a note for each reason says how many.
pub fn fit_tool_media(media: Vec<Media>, limits: &ImageLimits) -> (Vec<Media>, Vec<String>) {
    let (mut undecodable, mut oversized) = (0, 0);
    let mut kept = Vec::new();
    for mut m in media {
        let Some(data) = m.inline().filter(|_| resizable(&m.mime)) else {
            kept.push(m);
            continue;
        };
        match normalize(&m.mime, data, limits) {
            Ok(None) => kept.push(m),
            Ok(Some((mime, data))) => {
                m.mime = mime;
                m.source = codesmith_ai::Source::Base64(data);
                kept.push(m);
            }
            Err(ImageError::Decode) => undecodable += 1,
            Err(ImageError::Size { .. }) => oversized += 1,
        }
    }
    let note = |n: usize, why: &str| {
        (n > 0).then(|| {
            format!(
                "[{n} image{} omitted: {why}]",
                if n == 1 { "" } else { "s" }
            )
        })
    };
    let notes = [
        note(undecodable, "could not be decoded."),
        note(
            oversized,
            "could not be resized below the image size limit.",
        ),
    ]
    .into_iter()
    .flatten()
    .collect();
    (kept, notes)
}

/// A file attached to a prompt, as media when its bytes say it is an image,
/// PDF or video; `Ok(None)` for anything else (attached as text). Images
/// are fitted to `limits`; one that cannot be is an error, as in OpenCode.
pub fn attachment(path: &Path, name: &str, limits: &ImageLimits) -> Result<Option<Media>, String> {
    let meta =
        std::fs::metadata(path).map_err(|e| format!("Unable to read attachment {name}: {e}"))?;
    let mut head = [0u8; SNIFF_BYTES];
    let n = {
        use std::io::Read;
        let mut f = std::fs::File::open(path)
            .map_err(|e| format!("Unable to read attachment {name}: {e}"))?;
        f.read(&mut head).unwrap_or(0)
    };
    let Some(mime) = detect(&head[..n]) else {
        return Ok(None);
    };
    if meta.len() > MAX_INGEST_BYTES {
        return Err(format!(
            "Attachment exceeds the {MAX_INGEST_BYTES} byte limit: {name}"
        ));
    }
    let bytes =
        std::fs::read(path).map_err(|e| format!("Unable to read attachment {name}: {e}"))?;
    let mut m = Media::base64(mime, base64(&bytes)).named(name);
    if resizable(mime) {
        match normalize(mime, m.inline().unwrap_or_default(), limits) {
            Ok(None) => {}
            Ok(Some((mime, data))) => m = Media::base64(mime, data).named(name),
            Err(e) => return Err(e.message(name, limits)),
        }
    }
    Ok(Some(m))
}

/// The input modality a media type needs (OpenCode's `mimeToModality`).
fn modality(m: &Media) -> Option<&'static str> {
    match m.kind() {
        MediaKind::Image => Some("image"),
        MediaKind::Pdf => Some("pdf"),
        MediaKind::Video => Some("video"),
        MediaKind::Audio => Some("audio"),
        MediaKind::Other => None,
    }
}

/// The note that replaces media a model cannot take, or `None` when it can.
/// `input` is the model's input modalities; `None` means unknown, which is
/// taken as able for images, PDFs and audio. Video goes only to models
/// whose modalities list it: no common model takes it, and a provider that
/// refuses it would fail every later turn.
pub fn unsupported_note(m: &Media, input: Option<&[String]>) -> Option<String> {
    let modality = modality(m)?;
    let what = match &m.filename {
        Some(n) => format!("\"{n}\""),
        None => modality.to_string(),
    };
    let Some(input) = input else {
        return (modality == "video").then(|| {
            format!(
                "ERROR: Cannot read {what} (this model is not known to support video input; list \"video\" in its modalities.input to send it). Inform the user."
            )
        });
    };
    if input.iter().any(|i| i.starts_with(modality)) {
        return None;
    }
    Some(format!(
        "ERROR: Cannot read {what} (this model does not support {modality} input). Inform the user."
    ))
}

/// Shown where an old image was dropped to keep requests small.
pub const IMAGE_REMOVED: &str = "[This image was removed to reduce the request size and is no longer visible. Do not make claims about its contents from memory. If needed, retrieve it again with an available tool or ask the user to attach it again.]";
const IMAGE_BYTES_TRIGGER: usize = 25 * 1024 * 1024;
const IMAGE_BYTES_TARGET: usize = 15 * 1024 * 1024;

/// Replace media for which `replace` returns text: user attachments become
/// text parts, tool-result media is removed and its text appended to the
/// result. Media is visited in order.
fn replace_media(
    messages: &[Message],
    mut replace: impl FnMut(&Media) -> Option<String>,
) -> Vec<Message> {
    messages
        .iter()
        .map(|m| Message {
            role: m.role.clone(),
            parts: m
                .parts
                .iter()
                .map(|p| match p {
                    Part::Media(media) => replace(media).map_or_else(|| p.clone(), Part::text),
                    Part::ToolResult {
                        id,
                        name,
                        content,
                        is_error,
                        media,
                    } if !media.is_empty() => {
                        let mut content = content.clone();
                        let mut kept = Vec::new();
                        for item in media {
                            match replace(item) {
                                Some(note) => {
                                    if !content.is_empty() {
                                        content.push('\n');
                                    }
                                    content.push_str(&note);
                                }
                                None => kept.push(item.clone()),
                            }
                        }
                        Part::ToolResult {
                            id: id.clone(),
                            name: name.clone(),
                            content,
                            is_error: *is_error,
                            media: kept,
                        }
                    }
                    other => other.clone(),
                })
                .collect(),
        })
        .collect()
}

/// History as sent to a model with `input` modalities (OpenCode's
/// `unsupportedParts` then `boundImages`): media the model cannot take
/// (see `unsupported_note`) becomes a note telling it to say so, and once the inline images pass
/// 25 MiB the oldest are dropped down to 15 MiB. The stored history keeps
/// everything, so switching to a model that can see them restores them.
pub fn sendable(messages: &[Message], input: Option<&[String]>) -> Vec<Message> {
    let has_media = messages.iter().any(|m| {
        m.parts.iter().any(|p| match p {
            Part::Media(_) => true,
            Part::ToolResult { media, .. } => !media.is_empty(),
            _ => false,
        })
    });
    if !has_media {
        return messages.to_vec();
    }
    let supported = replace_media(messages, |m| unsupported_note(m, input));
    bound_images(&supported)
}

/// Drop the oldest images while the inline images of a request exceed
/// 25 MiB, down to 15 MiB, so the cut (and the provider's prompt cache)
/// moves only every 10 MiB.
pub fn bound_images(messages: &[Message]) -> Vec<Message> {
    let mut sizes = Vec::new();
    for m in messages {
        for p in &m.parts {
            match p {
                Part::Media(media) if media.kind() == MediaKind::Image => {
                    sizes.push(media.inline_len())
                }
                Part::ToolResult { media, .. } => sizes.extend(
                    media
                        .iter()
                        .filter(|m| m.kind() == MediaKind::Image)
                        .map(Media::inline_len),
                ),
                _ => {}
            }
        }
    }
    let (mut cut, mut kept) = (0, 0);
    for size in &sizes {
        kept += size;
        if kept <= IMAGE_BYTES_TRIGGER {
            continue;
        }
        while kept > IMAGE_BYTES_TARGET {
            kept -= sizes[cut];
            cut += 1;
        }
    }
    if cut == 0 {
        return messages.to_vec();
    }
    let mut index = 0;
    replace_media(messages, |m| {
        if m.kind() != MediaKind::Image {
            return None;
        }
        index += 1;
        (index <= cut).then(|| IMAGE_REMOVED.to_string())
    })
}

/// A short label for media in transcripts and summaries.
pub fn label(m: &Media) -> String {
    let what = match m.kind() {
        MediaKind::Image => "image",
        MediaKind::Pdf => "PDF",
        MediaKind::Video => "video",
        MediaKind::Audio => "audio",
        MediaKind::Other => "file",
    };
    match &m.filename {
        Some(n) => format!("[{what}: {n}]"),
        None => format!("[{what}]"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codesmith_ai::Role;
    use image::{Rgba, RgbaImage};

    fn png_of(img: RgbaImage) -> String {
        base64(&encode_png(&DynamicImage::ImageRgba8(img)))
    }

    fn dims(data: &str) -> (u32, u32) {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .unwrap();
        image::load_from_memory(&bytes).unwrap().dimensions()
    }

    /// A white 16x1 PNG, OpenCode's "wide" fixture.
    fn wide() -> String {
        png_of(RgbaImage::from_pixel(16, 1, Rgba([255, 255, 255, 255])))
    }

    #[test]
    fn detects_media_by_bytes() {
        assert_eq!(detect(b"\x89PNG\r\n\x1a\n...."), Some("image/png"));
        assert_eq!(detect(b"\xff\xd8\xff\xe0"), Some("image/jpeg"));
        assert_eq!(detect(b"GIF89a"), Some("image/gif"));
        assert_eq!(detect(b"RIFF\x10\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(detect(b"%PDF-1.7"), Some("application/pdf"));
        assert_eq!(detect(b"plain text"), None);
    }

    /// An ISO media file's leading `ftyp` box with `brand`.
    fn ftyp(brand: &[u8; 4]) -> Vec<u8> {
        let mut b = vec![0, 0, 0, 0x18];
        b.extend(b"ftyp");
        b.extend(brand);
        b.extend([0, 0, 2, 0]);
        b.extend(b"isommp41");
        b
    }

    #[test]
    fn detects_video_by_bytes() {
        assert_eq!(detect(&ftyp(b"isom")), Some("video/mp4"));
        assert_eq!(detect(&ftyp(b"mp42")), Some("video/mp4"));
        assert_eq!(detect(&ftyp(b"qt  ")), Some("video/quicktime"));
        // Images and audio in the same container are not video.
        for brand in [b"avif", b"heic", b"mif1", b"M4A "] {
            assert_eq!(detect(&ftyp(brand)), None, "{brand:?}");
        }
        // EBML with the webm doctype; Matroska is not taken.
        let ebml = |doctype: &[u8]| {
            let mut b = vec![0x1a, 0x45, 0xdf, 0xa3, 0x9f, 0x42, 0x86, 0x81, 0x01];
            b.extend([
                0x42,
                0xf7,
                0x81,
                0x01,
                0x42,
                0x82,
                0x80 | doctype.len() as u8,
            ]);
            b.extend(doctype);
            b
        };
        assert_eq!(detect(&ebml(b"webm")), Some("video/webm"));
        assert_eq!(detect(&ebml(b"matroska")), None);
        assert_eq!(detect(b"\0\0\0\x18ftyp"), None, "too short for a brand");
    }

    #[test]
    fn video_goes_only_to_models_that_list_it() {
        let v = Media::base64("video/mp4", "AAAA").named("clip.mp4");
        let msgs = vec![Message {
            role: Role::User,
            parts: vec![Part::Media(v.clone())],
        }];
        // Unknown capabilities: images are sent, video is not.
        let out = sendable(&msgs, None);
        assert_eq!(
            out[0].parts[0],
            Part::Text("ERROR: Cannot read \"clip.mp4\" (this model is not known to support video input; list \"video\" in its modalities.input to send it). Inform the user.".into())
        );
        let vision = vec!["text".to_string(), "image".to_string()];
        assert_eq!(
            sendable(&msgs, Some(&vision))[0].parts[0],
            Part::Text("ERROR: Cannot read \"clip.mp4\" (this model does not support video input). Inform the user.".into())
        );
        let video = vec!["text".to_string(), "video".to_string()];
        assert_eq!(sendable(&msgs, Some(&video)), msgs);
    }

    #[test]
    fn video_attachments_are_taken_whole_within_the_ingest_limit() {
        let d = tempfile::tempdir().unwrap();
        let mov = d.path().join("clip.mov");
        let mut bytes = ftyp(b"qt  ");
        bytes.extend([0u8; 100]);
        std::fs::write(&mov, &bytes).unwrap();
        let m = attachment(&mov, "clip.mov", &ImageLimits::default())
            .unwrap()
            .unwrap();
        assert_eq!(m.mime, "video/quicktime");
        assert_eq!(m.inline(), Some(base64(&bytes).as_str()));
        assert_eq!(read_note(&m), "Video read successfully");
        let big = d.path().join("big.mp4");
        let f = std::fs::File::create(&big).unwrap();
        use std::io::Write;
        (&f).write_all(&ftyp(b"isom")).unwrap();
        f.set_len(MAX_INGEST_BYTES + 1).unwrap();
        assert_eq!(
            attachment(&big, "big.mp4", &ImageLimits::default()).unwrap_err(),
            format!("Attachment exceeds the {MAX_INGEST_BYTES} byte limit: big.mp4")
        );
    }

    // Ported from OpenCode's packages/core/test/tool-read.test.ts (MIT):
    // the Image.Service cases that call `normalize` directly.

    #[test]
    fn oc_image_keeps_fitting_pngs_that_are_small_transparent_or_smaller_than_their_jpeg() {
        let mut seed: u32 = 1;
        let blocks: Vec<u8> = (0..500 * 375)
            .map(|_| {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                ((seed >> 31) * 255) as u8
            })
            .collect();
        let transparent = png_of(RgbaImage::from_fn(600, 600, |x, y| {
            let c = |ch: u32| ((((y * 600 + x) * 4 + ch) * 31 + x) % 256) as u8;
            Rgba([c(0), c(1), c(2), if y < 120 { 0 } else { 255 }])
        }));
        let small = png_of(RgbaImage::from_fn(1000, 1000, |_, y| {
            let v = (y % 256) as u8;
            Rgba([v, v, v, 255])
        }));
        let diagram = png_of(RgbaImage::from_fn(2000, 1500, |x, y| {
            let v = blocks[(y / 4 * 500 + x / 4) as usize];
            Rgba([v, v, v, 255])
        }));
        assert!(transparent.len() > 384 * 1024);
        assert!(small.len() <= 384 * 1024);
        assert!(diagram.len() > 384 * 1024);
        let limits = ImageLimits::default();
        for data in [transparent, small, diagram] {
            assert_eq!(normalize("image/png", &data, &limits), Ok(None));
        }
    }

    #[test]
    fn oc_image_accepts_png_candidates_at_the_exact_base64_limit_and_skips_them_one_byte_below() {
        // Adapted: the padding of each candidate depends on the PNG encoder,
        // so it is not asserted.
        let data = wide();
        for max_width in [4, 5, 6] {
            let mut limits = ImageLimits {
                max_width,
                max_base64_bytes: 1_024,
                ..Default::default()
            };
            let (mime, candidate) = normalize("image/png", &data, &limits).unwrap().unwrap();
            assert_eq!(mime, "image/png");
            assert!(dims(&candidate).0 <= max_width);
            limits.max_base64_bytes = candidate.len();
            assert_eq!(
                normalize("image/png", &data, &limits).unwrap().unwrap().1,
                candidate
            );
            limits.max_base64_bytes = candidate.len() - 1;
            let (mime, smaller) = normalize("image/png", &data, &limits).unwrap().unwrap();
            assert_eq!(mime, "image/png");
            assert!(smaller.len() < candidate.len());
        }
    }

    #[test]
    fn oc_image_resizes_to_configured_dimensions() {
        let limits = ImageLimits {
            max_width: 4,
            ..Default::default()
        };
        let (_, out) = normalize("image/png", &wide(), &limits).unwrap().unwrap();
        let (w, h) = dims(&out);
        assert!(w <= 4 && h <= 2_000);
    }

    #[test]
    fn oc_image_drops_oversized_images_when_resizing_is_disabled() {
        let limits = ImageLimits {
            auto_resize: false,
            max_width: 4,
            ..Default::default()
        };
        let m = vec![Media::base64("image/png", wide())];
        let (kept, notes) = fit_tool_media(m, &limits);
        assert!(kept.is_empty());
        assert_eq!(
            notes,
            ["[1 image omitted: could not be resized below the image size limit.]"]
        );
    }

    #[test]
    fn oc_image_stores_large_opaque_png_screenshots_as_jpeg() {
        let (w, h) = (2000, 1125);
        let data = png_of(RgbaImage::from_fn(w, h, |x, y| {
            let i = (y * w + x) * 4;
            let c = |ch: u32| (((i + ch) * 31 + ((i + ch) >> 7)) % 256) as u8;
            Rgba([c(0), c(1), c(2), 255])
        }));
        let (mime, out) = normalize("image/png", &data, &ImageLimits::default())
            .unwrap()
            .unwrap();
        assert_eq!(mime, "image/jpeg");
        assert!(out.len() < data.len());
        assert_eq!(dims(&out), (w, h));
    }

    #[test]
    fn undecodable_images_are_dropped_with_a_note() {
        let m = vec![
            Media::base64("image/png", "iVBORw0KGgo="),
            Media::base64("application/pdf", "JVBERi0="),
        ];
        let (kept, notes) = fit_tool_media(m, &ImageLimits::default());
        assert_eq!(kept.len(), 1, "the PDF is kept");
        assert_eq!(notes, ["[1 image omitted: could not be decoded.]"]);
    }

    #[test]
    fn unsupported_media_becomes_a_note() {
        let png = Media::base64("image/png", "AAEC").named("a.png");
        let msgs = vec![
            Message {
                role: Role::User,
                parts: vec![Part::Text("look".into()), Part::Media(png.clone())],
            },
            Message {
                role: Role::User,
                parts: vec![Part::ToolResult {
                    id: "c".into(),
                    name: "read".into(),
                    content: "Image read successfully".into(),
                    is_error: false,
                    media: vec![
                        Media::base64("application/pdf", "JVBERi0=").named("r.pdf"),
                        png.clone(),
                    ],
                }],
            },
        ];
        // Unknown capabilities send everything.
        assert_eq!(sendable(&msgs, None), msgs);
        let text_only = vec!["text".to_string()];
        let out = sendable(&msgs, Some(&text_only));
        assert_eq!(
            out[0].parts[1],
            Part::Text("ERROR: Cannot read \"a.png\" (this model does not support image input). Inform the user.".into())
        );
        let Part::ToolResult { content, media, .. } = &out[1].parts[0] else {
            panic!()
        };
        assert!(media.is_empty());
        assert_eq!(content, "Image read successfully\nERROR: Cannot read \"r.pdf\" (this model does not support pdf input). Inform the user.\nERROR: Cannot read \"a.png\" (this model does not support image input). Inform the user.");
        // Images but not PDFs.
        let vision = vec!["text".to_string(), "image".to_string()];
        let out = sendable(&msgs, Some(&vision));
        assert_eq!(out[0], msgs[0]);
        let Part::ToolResult { media, .. } = &out[1].parts[0] else {
            panic!()
        };
        assert_eq!(media, &vec![png]);
    }

    #[test]
    fn oldest_images_are_dropped_past_the_budget() {
        let big = |n: u8| {
            Media::base64(
                "image/png",
                (n as char).to_string().repeat(10 * 1024 * 1024),
            )
        };
        let msgs: Vec<Message> = (0..3)
            .map(|i| Message {
                role: Role::User,
                parts: vec![Part::Media(big(b'a' + i))],
            })
            .collect();
        // 30 MiB passes the 25 MiB trigger: drop down to 15 MiB (two go).
        let out = bound_images(&msgs);
        assert_eq!(out[0].parts[0], Part::Text(IMAGE_REMOVED.into()));
        assert_eq!(out[1].parts[0], Part::Text(IMAGE_REMOVED.into()));
        assert_eq!(out[2], msgs[2]);
        assert_eq!(bound_images(&msgs[..2]), msgs[..2].to_vec());
    }

    #[test]
    fn attachments_are_detected_by_content() {
        let d = tempfile::tempdir().unwrap();
        let png = d.path().join("shot.bin");
        std::fs::write(
            &png,
            base64::engine::general_purpose::STANDARD
                .decode(wide())
                .unwrap(),
        )
        .unwrap();
        let m = attachment(&png, "shot.bin", &ImageLimits::default())
            .unwrap()
            .unwrap();
        assert_eq!(
            (m.mime.as_str(), m.filename.as_deref()),
            ("image/png", Some("shot.bin"))
        );
        let txt = d.path().join("a.png");
        std::fs::write(&txt, "not an image").unwrap();
        assert_eq!(
            attachment(&txt, "a.png", &ImageLimits::default()).unwrap(),
            None
        );
        let bad = d.path().join("bad.png");
        std::fs::write(&bad, b"\x89PNG\r\n\x1a\n").unwrap();
        assert!(attachment(&bad, "bad.png", &ImageLimits::default())
            .unwrap_err()
            .contains("could not be decoded"));
    }
}
