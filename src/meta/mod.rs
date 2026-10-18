//! Format-independent entry points.
//!
//! Every format module exposes the same shape: given the original bytes, hand
//! back the cleaned bytes *and* a description of what came out. Examining and
//! cleaning are therefore the same operation — the list the window shows is
//! produced by the code that does the removal, so it can never promise to
//! strip something it then leaves behind.

pub mod exif;
pub mod gif;
pub mod isobmff;
pub mod jpeg;
pub mod ole;
pub mod pdf;
pub mod png;
pub mod svg;
pub mod tiff;
pub mod webp;
pub mod xml;
pub mod zipdoc;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use zipdoc::DocKind;

/// One piece of metadata found in a file, in the words shown to the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub label: String,
    pub detail: String,
}

impl Finding {
    pub fn new(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Finding { label: label.into(), detail: detail.into() }
    }
}

#[derive(Debug)]
pub struct StripResult {
    pub data: Vec<u8>,
    pub findings: Vec<Finding>,
    /// Metadata left in place on purpose, with the reason. Some formats
    /// cannot be fully cleaned without breaking them, and saying so is
    /// better than quietly leaving something behind.
    pub kept: Vec<Finding>,
}

impl StripResult {
    pub fn new(data: Vec<u8>, findings: Vec<Finding>) -> Self {
        StripResult { data, findings, kept: Vec::new() }
    }

    pub fn keeping(mut self, kept: Vec<Finding>) -> Self {
        self.kept = kept;
        self
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Jpeg,
    Png,
    Gif,
    WebP,
    /// TIFF and the camera RAW formats built on it.
    Tiff,
    /// HEIC, HEIF and AVIF: the same ISO base media container throughout.
    Heif,
    Svg,
    Pdf,
    Document(DocKind),
    Legacy(ole::OleKind),
}

impl Kind {
    pub fn describe(self) -> &'static str {
        match self {
            Kind::Jpeg => "JPEG image",
            Kind::Png => "PNG image",
            Kind::Gif => "GIF image",
            Kind::WebP => "WebP image",
            Kind::Tiff => "TIFF or camera RAW image",
            Kind::Heif => "HEIF image",
            Kind::Svg => "SVG image",
            Kind::Pdf => "PDF document",
            Kind::Document(d) => d.describe(),
            Kind::Legacy(k) => k.describe(),
        }
    }
}

/// Recognises files by their contents. Extensions are renamed, mistyped and
/// occasionally lied about; magic bytes are not.
pub fn detect(data: &[u8]) -> Option<Kind> {
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(Kind::Jpeg);
    }
    if data.starts_with(png::SIGNATURE) {
        return Some(Kind::Png);
    }
    if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        return Some(Kind::Gif);
    }
    if data.starts_with(b"RIFF") && data.get(8..12) == Some(b"WEBP") {
        return Some(Kind::WebP);
    }
    if tiff::detect(data) {
        return Some(Kind::Tiff);
    }
    if isobmff::detect(data) {
        return Some(Kind::Heif);
    }
    if data.starts_with(b"%PDF-") {
        return Some(Kind::Pdf);
    }
    if data.starts_with(&ole::SIGNATURE) {
        return ole::detect(data).map(Kind::Legacy);
    }
    if data.starts_with(b"PK") {
        return zipdoc::detect(data).map(Kind::Document);
    }
    if svg::detect(data) {
        return Some(Kind::Svg);
    }
    None
}

#[derive(Debug)]
pub struct Examined {
    pub kind: Kind,
    pub findings: Vec<Finding>,
    /// What was deliberately left alone, and why.
    pub kept: Vec<Finding>,
    /// Exactly the bytes that writing would produce.
    pub cleaned: Vec<u8>,
    pub original_len: usize,
}

impl Examined {
    /// How much smaller the cleaned file is. Saturating: preserving a rebuilt
    /// orientation tag can make a nearly-clean JPEG a few bytes larger.
    pub fn bytes_saved(&self) -> usize {
        self.original_len.saturating_sub(self.cleaned.len())
    }

    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }
}

/// Choices the user makes about what counts as removable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    /// Leave camera maker notes in TIFF-based RAW files, with a `kept` note.
    /// See `tiff::MAKER_NOTES` for why this is the default.
    pub keep_maker_notes: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options { keep_maker_notes: true }
    }
}

pub fn examine(data: &[u8]) -> Result<Examined, String> {
    examine_with(data, &Options::default())
}

pub fn examine_with(data: &[u8], options: &Options) -> Result<Examined, String> {
    let kind = detect(data).ok_or_else(|| {
        "unsupported file type".to_string()
    })?;

    let result = match kind {
        Kind::Jpeg => jpeg::strip(data)?,
        Kind::Png => png::strip(data)?,
        Kind::Gif => gif::strip(data)?,
        Kind::WebP => webp::strip(data)?,
        Kind::Tiff => tiff::strip(data, options.keep_maker_notes)?,
        Kind::Heif => isobmff::strip(data)?,
        Kind::Svg => svg::strip(data, options)?,
        Kind::Pdf => pdf::strip(data)?,
        Kind::Document(d) => zipdoc::strip(data, d, options)?,
        Kind::Legacy(k) => ole::strip(data, k)?,
    };

    Ok(Examined {
        kind,
        findings: result.findings,
        kept: result.kept,
        cleaned: result.data,
        original_len: data.len(),
    })
}

/// Cleans a picture found inside another file — a photo in a Word document, a
/// JPEG in a PDF, a data URI in an SVG. It carries its own Exif, GPS included,
/// whatever the container's properties say. `None` when it is not a picture
/// this app reads, or when there is nothing in it to remove.
pub(crate) fn clean_embedded_image(data: &[u8], options: &Options) -> Option<Examined> {
    let kind = detect(data)?;
    if !matches!(kind, Kind::Jpeg | Kind::Png | Kind::Gif | Kind::WebP | Kind::Tiff | Kind::Heif) {
        return None;
    }
    let found = examine_with(data, options).ok()?;
    (!found.is_clean()).then_some(found)
}

/// One line for a cleaned embedded picture, naming where it was found.
pub(crate) fn embedded_finding(place: &str, found: &Examined) -> Finding {
    let mut what: Vec<&str> = found.findings.iter().map(|f| f.label.as_str()).collect();
    what.dedup();
    Finding::new("Embedded image", format!("{place}: {}", what.join(", ")))
}

/// The embedded picture's own limitations, added to the container's without
/// repeating one that is already there.
pub(crate) fn merge_kept(kept: &mut Vec<Finding>, found: &Examined) {
    for note in &found.kept {
        if !kept.contains(note) {
            kept.push(note.clone());
        }
    }
}

/// Where a cleaned copy goes: `holiday.jpg` becomes `holiday-clean.jpg`, next
/// to the original. Never silently overwrites — if the name is taken, counts
/// up until it finds one that is free.
pub fn copy_destination(source: &Path) -> PathBuf {
    copy_destination_in(source, source.parent().unwrap_or(Path::new(".")))
}

/// As `copy_destination`, but in `directory` rather than beside the original.
pub fn copy_destination_in(source: &Path, directory: &Path) -> PathBuf {
    let stem = source.file_stem().unwrap_or_default();
    let extension = source.extension();

    let build = |suffix: &str| -> PathBuf {
        let mut name = OsString::from(stem);
        name.push(suffix);
        if let Some(ext) = extension {
            name.push(".");
            name.push(ext);
        }
        directory.join(name)
    };

    let mut candidate = build("-clean");
    let mut n = 2;
    while candidate.exists() {
        candidate = build(&format!("-clean-{n}"));
        n += 1;
    }
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_files_are_reported_not_mangled() {
        let err = examine(b"OggS\x00\x02\x00\x00").unwrap_err();
        assert!(err.contains("unsupported"), "{err}");
        assert!(detect(b"").is_none());
        assert!(detect(b"just some prose in a text file").is_none());
        // A ZIP that is not an office package, and an MP4 that shares the
        // HEIF box structure but is not an image.
        assert!(detect(b"\x1f\x8b\x08\x00gzip").is_none());
    }

    #[test]
    fn a_broken_file_of_a_known_type_reports_the_type_not_a_guess() {
        // Claimed by the PDF branch on its magic bytes, then found to be
        // unreadable. The error has to say so rather than fall back to
        // "unsupported", which would send the user looking for the wrong fix.
        let err = examine(b"%PDF-1.7 truncated before anything useful").unwrap_err();
        assert!(err.contains("PDF"), "{err}");
        assert!(!err.contains("unsupported"), "{err}");
    }

    #[test]
    fn a_zip_that_is_not_an_office_document_is_not_claimed() {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        w.start_file("readme.txt", zip::write::SimpleFileOptions::default()).unwrap();
        std::io::Write::write_all(&mut w, b"hi").unwrap();
        let plain_zip = w.finish().unwrap().into_inner();
        assert!(detect(&plain_zip).is_none());
    }

    #[test]
    fn copy_destination_sits_beside_the_original() {
        let p = copy_destination(Path::new("/tmp/holiday.jpg"));
        assert_eq!(p, Path::new("/tmp/holiday-clean.jpg"));
        assert_eq!(
            copy_destination(Path::new("/tmp/notes")),
            Path::new("/tmp/notes-clean")
        );
        assert_eq!(
            copy_destination_in(Path::new("/tmp/holiday.jpg"), Path::new("/srv/out")),
            Path::new("/srv/out/holiday-clean.jpg")
        );
    }

    #[test]
    fn the_tiff_based_raw_variants_are_recognised() {
        // Olympus ORF and Panasonic RW2 put their own magic where TIFF has 42.
        for magic in [&b"IIRO"[..], b"IIRS", b"MMOR", b"IIU\0"] {
            let mut file = magic.to_vec();
            file.extend_from_slice(&[8, 0, 0, 0]);
            assert_eq!(detect(&file), Some(Kind::Tiff), "{magic:?}");
        }
    }
}
