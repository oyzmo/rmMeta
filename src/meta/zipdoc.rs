//! Office documents: OOXML (.docx/.xlsx/.pptx) and ODF (.odt/.ods/.odp).
//!
//! Both are ZIP packages whose properties live in a couple of XML parts, so
//! one pass over the archive handles every office format. Two things leak
//! here, not one: the property parts, and the per-entry modification times the
//! ZIP format records for every file inside. Both are dealt with.

use std::io::{Cursor, Read, Write};

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter};

use super::xml::{scrub_attributes, strip_elements};
use super::{Finding, StripResult};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DocKind {
    Ooxml,
    Odf,
}

impl DocKind {
    pub fn describe(self) -> &'static str {
        match self {
            DocKind::Ooxml => "Office document",
            DocKind::Odf => "OpenDocument file",
        }
    }
}

/// Identifies the package from its contents rather than its extension, so a
/// renamed file is still handled correctly.
pub fn detect(data: &[u8]) -> Option<DocKind> {
    let mut zip = ZipArchive::new(Cursor::new(data)).ok()?;
    if zip.by_name("[Content_Types].xml").is_ok() {
        return Some(DocKind::Ooxml);
    }
    let mut mimetype = String::new();
    if let Ok(mut f) = zip.by_name("mimetype") {
        f.read_to_string(&mut mimetype).ok()?;
        if mimetype.starts_with("application/vnd.oasis.opendocument") {
            return Some(DocKind::Odf);
        }
    }
    None
}

/// Author and company identity, plus the timestamps and revision counters an
/// office suite maintains behind the user's back.
///
/// Deliberately *not* stripped: title, subject, description, keywords and
/// category. Those are content the author typed on purpose, and silently
/// discarding them would be a surprise rather than a privacy win.
const OOXML_CORE: &[&str] = &[
    "dc:creator",
    "cp:lastModifiedBy",
    "cp:revision",
    "dcterms:created",
    "dcterms:modified",
    "cp:lastPrinted",
];

/// `Application` and `AppVersion` are here to match what the ODF side does to
/// `meta:generator`. They are not just "Word": a LibreOffice document names the
/// exact build and the operating system it ran on.
const OOXML_APP: &[&str] = &[
    "Company",
    "Manager",
    "TotalTime",
    "Template",
    "HyperlinkBase",
    "Application",
    "AppVersion",
];

const ODF_META: &[&str] = &[
    "meta:initial-creator",
    "dc:creator",
    "meta:creation-date",
    "dc:date",
    "meta:editing-cycles",
    "meta:editing-duration",
    "meta:generator",
    "meta:printed-by",
    "meta:print-date",
];

/// Who left each comment and made each tracked change, which the property
/// parts know nothing about. The parts: `(where, attributes to empty,
/// attributes to remove)`. Word's `w:date` is optional and goes; the comment
/// dates of the other formats are required, so they stay with a `kept` note.
fn ooxml_people(name: &str) -> Option<(&'static [&'static str], &'static [&'static str])> {
    let xml = name.ends_with(".xml");
    Some(match name {
        "word/people.xml" => (&["author", "userId"], &[]),
        n if xml && n.starts_with("word/") => (&["author", "initials"], &["date", "dateUtc"]),
        n if xml && n.starts_with("xl/persons/") => (&["displayName", "userId"], &[]),
        "ppt/commentAuthors.xml" => (&["name", "initials"], &[]),
        "ppt/authors.xml" => (&["name", "initials", "userId"], &[]),
        _ => return None,
    })
}

fn has_comment_dates(kind: DocKind, name: &str, body: &[u8]) -> bool {
    let has = |needle: &[u8]| body.windows(needle.len()).any(|w| w == needle);
    match kind {
        DocKind::Ooxml if name.starts_with("xl/threadedComments/") => has(b" dT=\""),
        DocKind::Ooxml if name.starts_with("ppt/comments/") => has(b" dt=\""),
        DocKind::Odf if matches!(name, "content.xml" | "styles.xml") => has(b"<dc:date>"),
        _ => false,
    }
}

/// Removed outright rather than emptied: what they disclose is in attributes.
/// See `xml::strip_elements`.
const ODF_DELETE: &[&str] = &["meta:template", "meta:user-defined"];

pub fn strip(data: &[u8], kind: DocKind, options: &super::Options) -> Result<StripResult, String> {
    let mut zip =
        ZipArchive::new(Cursor::new(data)).map_err(|e| format!("could not read the archive: {e}"))?;

    let mut entries: Vec<Entry> = Vec::with_capacity(zip.len());
    for i in 0..zip.len() {
        let mut f = zip
            .by_index(i)
            .map_err(|e| format!("could not read entry {i}: {e}"))?;
        let name = f.name().to_string();
        let is_dir = f.is_dir();
        let mut body = Vec::new();
        if !is_dir {
            f.read_to_end(&mut body)
                .map_err(|e| format!("could not read {name}: {e}"))?;
        }
        entries.push(Entry { name, is_dir, body });
    }

    let mut findings = Vec::new();
    let mut kept = Vec::new();
    let mut had_timestamp = false;
    let mut authors: Vec<String> = Vec::new();
    let mut dated = 0usize;
    let mut kept_dates = false;

    for e in &mut entries {
        kept_dates |= has_comment_dates(kind, &e.name, &e.body);

        // Comment and tracked-change authors live in the content parts.
        let people: Option<Result<_, String>> = match (kind, e.name.as_str()) {
            (DocKind::Ooxml, n) if n.starts_with("xl/comments") && n.ends_with(".xml") => {
                Some(strip_elements(&e.body, &["author"], &[]))
            }
            (DocKind::Ooxml, n) => ooxml_people(n).map(|(b, d)| scrub_attributes(&e.body, b, d)),
            (DocKind::Odf, "content.xml" | "styles.xml") => {
                Some(strip_elements(&e.body, &["dc:creator", "meta:creator-initials"], &[]))
            }
            _ => None,
        };
        if let Some(stripped) = people {
            let stripped = stripped?;
            for (key, value) in stripped.removed {
                if key.ends_with("date") || key.ends_with("dateUtc") {
                    dated += 1;
                } else if !authors.contains(&value) {
                    authors.push(value);
                }
            }
            e.body = stripped.xml;
            continue;
        }

        let (targets, delete): (&[&str], &[&str]) = match (kind, e.name.as_str()) {
            (DocKind::Ooxml, "docProps/core.xml") => (OOXML_CORE, &[]),
            (DocKind::Ooxml, "docProps/app.xml") => (OOXML_APP, &[]),
            // The whole part is user-defined properties; empty the root.
            (DocKind::Ooxml, "docProps/custom.xml") => (&["Properties"], &[]),
            (DocKind::Odf, "meta.xml") => (ODF_META, ODF_DELETE),
            _ => {
                // A photo pasted into a document keeps its own Exif — camera,
                // date and GPS — whatever the document's properties say.
                if let Some(found) = super::clean_embedded_image(&e.body, options) {
                    findings.push(super::embedded_finding(&e.name, &found));
                    super::merge_kept(&mut kept, &found);
                    e.body = found.cleaned;
                } else if let Some(found) = super::clean_embedded_document(&e.body, options) {
                    // The workbook behind a chart has properties of its own.
                    findings.push(super::embedded_document_finding(&e.name, &found));
                    super::merge_kept(&mut kept, &found);
                    e.body = found.cleaned;
                }
                continue;
            }
        };

        let stripped = strip_elements(&e.body, targets, delete)?;
        for (element, value) in stripped.removed {
            findings.push(Finding::new(label_for(&element), value));
        }
        e.body = stripped.xml;
    }

    if !authors.is_empty() {
        findings.push(Finding::new("Comment and revision authors", authors.join(", ")));
    }
    if dated > 0 {
        findings.push(Finding::new(
            "Revision dates",
            format!(
                "{dated} timestamp{} on comments and tracked changes",
                if dated == 1 { "" } else { "s" }
            ),
        ));
    }
    if kept_dates {
        kept.push(Finding::new(
            "Comment dates",
            "when each comment or tracked change was made \u{2014} the format requires them",
        ));
    }

    // Every entry carries its own modification time, which maps out when the
    // document was worked on even after the XML properties are clean.
    let epoch = DateTime::default();
    for i in 0..zip.len() {
        if let Ok(f) = zip.by_index(i) {
            if f.last_modified().is_some_and(|t| t != epoch) {
                had_timestamp = true;
                break;
            }
        }
    }
    if had_timestamp {
        findings.push(Finding::new(
            "Internal timestamps",
            "modification times recorded for each part of the document",
        ));
    }

    Ok(StripResult::new(rebuild(&entries, kind, epoch)?, findings).keeping(kept))
}

struct Entry {
    name: String,
    is_dir: bool,
    body: Vec<u8>,
}

fn rebuild(entries: &[Entry], kind: DocKind, epoch: DateTime) -> Result<Vec<u8>, String> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let deflated = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .last_modified_time(epoch);
    let stored = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Stored)
        .last_modified_time(epoch);

    let io = |e: std::io::Error| format!("could not write the archive: {e}");
    let zip_err = |e: zip::result::ZipError| format!("could not write the archive: {e}");

    // ODF requires an uncompressed `mimetype` as the very first entry; the file
    // is not recognised as an OpenDocument without it.
    if kind == DocKind::Odf {
        if let Some(m) = entries.iter().find(|e| e.name == "mimetype") {
            writer.start_file("mimetype", stored).map_err(zip_err)?;
            writer.write_all(&m.body).map_err(io)?;
        }
    }

    for e in entries {
        if kind == DocKind::Odf && e.name == "mimetype" {
            continue;
        }
        if e.is_dir {
            writer.add_directory(e.name.clone(), deflated).map_err(zip_err)?;
        } else {
            writer.start_file(e.name.clone(), deflated).map_err(zip_err)?;
            writer.write_all(&e.body).map_err(io)?;
        }
    }

    Ok(writer.finish().map_err(zip_err)?.into_inner())
}

fn label_for(element: &str) -> &'static str {
    match element.rsplit(':').next().unwrap_or(element) {
        "creator" => "Author",
        "lastModifiedBy" => "Last saved by",
        "initial-creator" => "Original author",
        "revision" | "editing-cycles" => "Times saved",
        "created" | "creation-date" => "Created",
        "modified" | "date" => "Last modified",
        "lastPrinted" | "print-date" => "Last printed",
        "printed-by" => "Printed by",
        "generator" | "Application" => "Software",
        "AppVersion" => "Software version",
        "Company" => "Company",
        "Manager" => "Manager",
        "TotalTime" | "editing-duration" => "Total editing time",
        "Template" | "template" => "Template",
        "HyperlinkBase" => "Hyperlink base",
        "Properties" | "user-defined" => "Custom properties",
        _ => "Document property",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip(data: &[u8], kind: DocKind) -> Result<StripResult, String> {
        super::strip(data, kind, &crate::meta::Options::default())
    }

    const CORE: &[u8] = br#"<?xml version="1.0"?><cp:coreProperties xmlns:cp="c" xmlns:dc="d" xmlns:dcterms="t"><dc:title>Budget</dc:title><dc:creator>Jane Doe</dc:creator><cp:lastModifiedBy>Bob</cp:lastModifiedBy><cp:revision>42</cp:revision><dcterms:created>2024-01-02T10:00:00Z</dcterms:created></cp:coreProperties>"#;

    fn docx() -> Vec<u8> {
        let mut w = ZipWriter::new(Cursor::new(Vec::new()));
        let opts = SimpleFileOptions::default()
            .last_modified_time(DateTime::from_date_and_time(2024, 5, 11, 9, 30, 0).unwrap());
        for (name, body) in [
            ("[Content_Types].xml", b"<Types/>".as_slice()),
            ("docProps/core.xml", CORE),
            ("docProps/app.xml", br#"<Properties><Company>ACME Ltd</Company><Application>Word</Application></Properties>"#),
            ("word/document.xml", b"<w:document>hello</w:document>"),
        ] {
            w.start_file(name, opts).unwrap();
            w.write_all(body).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    fn package(parts: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, body) in parts {
            w.start_file(*name, SimpleFileOptions::default()).unwrap();
            w.write_all(body).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    #[test]
    fn comment_and_revision_authors_go_from_every_format() {
        let docx = package(&[
            ("[Content_Types].xml", b"<Types/>"),
            ("word/document.xml", br#"<w:document xmlns:w="w"><w:ins w:id="1" w:author="Jane Doe" w:date="2024-01-02T10:00:00Z">x</w:ins></w:document>"#),
            ("word/comments.xml", br#"<w:comments xmlns:w="w"><w:comment w:id="0" w:author="Bob Roe" w:initials="BR">note</w:comment></w:comments>"#),
            ("word/people.xml", br#"<w15:people xmlns:w15="x"><w15:person w15:author="Bob Roe"><w15:presenceInfo w15:providerId="AD" w15:userId="bob@corp.example"/></w15:person></w15:people>"#),
        ]);
        let xlsx = package(&[
            ("[Content_Types].xml", b"<Types/>"),
            ("xl/comments1.xml", br#"<comments><authors><author>Jane Doe</author></authors></comments>"#),
            ("xl/persons/person.xml", br#"<personList><person displayName="Bob Roe" userId="bob@corp.example"/></personList>"#),
        ]);
        let pptx = package(&[
            ("[Content_Types].xml", b"<Types/>"),
            ("ppt/commentAuthors.xml", br#"<p:cmAuthorLst xmlns:p="p"><p:cmAuthor id="0" name="Jane Doe" initials="JD"/></p:cmAuthorLst>"#),
            ("ppt/comments/comment1.xml", br#"<p:cmLst xmlns:p="p"><p:cm authorId="0" dt="2024-01-02T10:00:00"/></p:cmLst>"#),
        ]);
        for file in [docx, xlsx, pptx] {
            let out = strip(&file, DocKind::Ooxml).unwrap();
            let mut zip = ZipArchive::new(Cursor::new(&out.data)).unwrap();
            for i in 0..zip.len() {
                let mut s = String::new();
                zip.by_index(i).unwrap().read_to_string(&mut s).unwrap();
                for name in ["Jane Doe", "Bob Roe", "bob@corp", "2024-01-02T10:00:00Z", "\"BR\"", "\"JD\""] {
                    assert!(!s.contains(name), "{name} survived in {s}");
                }
            }
            assert!(out.findings.iter().any(|f| f.label == "Comment and revision authors"), "{:?}", out.findings);
            let again = strip(&out.data, DocKind::Ooxml).unwrap();
            assert!(again.findings.is_empty(), "{:?}", again.findings);
        }
    }

    #[test]
    fn required_comment_dates_are_kept_and_said_so() {
        let pptx = package(&[
            ("[Content_Types].xml", b"<Types/>"),
            ("ppt/comments/comment1.xml", br#"<p:cmLst xmlns:p="p"><p:cm authorId="0" dt="2024-01-02T10:00:00"/></p:cmLst>"#),
        ]);
        let out = strip(&pptx, DocKind::Ooxml).unwrap();
        assert!(out.kept.iter().any(|k| k.label == "Comment dates"));
    }

    #[test]
    fn odf_annotation_authors_go() {
        let odt = package(&[
            ("mimetype", b"application/vnd.oasis.opendocument.text"),
            ("content.xml", br#"<office:document-content xmlns:office="o" xmlns:dc="d"><office:annotation><dc:creator>Jane Doe</dc:creator><dc:date>2024-01-02T10:00:00</dc:date><text:p>note</text:p></office:annotation></office:document-content>"#),
        ]);
        let out = strip(&odt, DocKind::Odf).unwrap();
        assert!(!read(&out.data, "content.xml").contains("Jane Doe"));
        assert!(read(&out.data, "content.xml").contains("note"));
        assert!(out.findings.iter().any(|f| f.detail.contains("Jane Doe")));
        assert!(out.kept.iter().any(|k| k.label == "Comment dates"));
    }

    #[test]
    fn embedded_workbooks_are_cleaned_too() {
        let inner = package(&[
            ("[Content_Types].xml", b"<Types/>"),
            ("docProps/core.xml", CORE),
        ]);
        let outer = package(&[
            ("[Content_Types].xml", b"<Types/>"),
            ("word/embeddings/Microsoft_Excel_Worksheet.xlsx", &inner),
        ]);
        let out = strip(&outer, DocKind::Ooxml).unwrap();
        assert!(out.findings.iter().any(|f| f.label == "Embedded document"), "{:?}", out.findings);
        let mut zip = ZipArchive::new(Cursor::new(&out.data)).unwrap();
        let mut cleaned = Vec::new();
        zip.by_name("word/embeddings/Microsoft_Excel_Worksheet.xlsx").unwrap().read_to_end(&mut cleaned).unwrap();
        assert!(!read(&cleaned, "docProps/core.xml").contains("Jane Doe"));
        assert!(strip(&out.data, DocKind::Ooxml).unwrap().findings.is_empty());
    }

    fn read(data: &[u8], name: &str) -> String {
        let mut zip = ZipArchive::new(Cursor::new(data)).unwrap();
        let mut s = String::new();
        zip.by_name(name).unwrap().read_to_string(&mut s).unwrap();
        s
    }

    #[test]
    fn detects_ooxml_from_content() {
        assert_eq!(detect(&docx()), Some(DocKind::Ooxml));
        assert_eq!(detect(b"not a zip"), None);
    }

    #[test]
    fn removes_author_company_and_revision_but_keeps_the_document() {
        let out = strip(&docx(), DocKind::Ooxml).unwrap();

        let core = read(&out.data, "docProps/core.xml");
        assert!(!core.contains("Jane Doe") && !core.contains("Bob") && !core.contains("42"));
        assert!(core.contains("<dc:title>Budget</dc:title>"), "authored title survives");

        assert!(!read(&out.data, "docProps/app.xml").contains("ACME"));
        assert_eq!(read(&out.data, "word/document.xml"), "<w:document>hello</w:document>");

        let labels: Vec<_> = out.findings.iter().map(|f| f.label.as_str()).collect();
        for expected in ["Author", "Last saved by", "Times saved", "Created", "Company"] {
            assert!(labels.contains(&expected), "missing {expected} in {labels:?}");
        }
    }

    #[test]
    fn resets_the_per_entry_timestamps() {
        let out = strip(&docx(), DocKind::Ooxml).unwrap();
        let mut zip = ZipArchive::new(Cursor::new(&out.data)).unwrap();
        for i in 0..zip.len() {
            let f = zip.by_index(i).unwrap();
            assert_eq!(f.last_modified(), Some(DateTime::default()), "{}", f.name());
        }
        assert!(out.findings.iter().any(|f| f.label == "Internal timestamps"));
    }

    #[test]
    fn odf_keeps_mimetype_first_and_uncompressed() {
        let mut w = ZipWriter::new(Cursor::new(Vec::new()));
        w.start_file(
            "mimetype",
            SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
        )
        .unwrap();
        w.write_all(b"application/vnd.oasis.opendocument.text").unwrap();
        let opts = SimpleFileOptions::default();
        w.start_file("meta.xml", opts).unwrap();
        w.write_all(br#"<office:document-meta xmlns:office="o" xmlns:meta="m" xmlns:dc="d"><office:meta><meta:generator>LibreOffice/7.6</meta:generator><dc:creator>Jane Doe</dc:creator></office:meta></office:document-meta>"#).unwrap();
        w.start_file("content.xml", opts).unwrap();
        w.write_all(b"<office:body/>").unwrap();
        let odt = w.finish().unwrap().into_inner();

        assert_eq!(detect(&odt), Some(DocKind::Odf));
        let out = strip(&odt, DocKind::Odf).unwrap();

        let mut zip = ZipArchive::new(Cursor::new(&out.data)).unwrap();
        let first = zip.by_index(0).unwrap();
        assert_eq!(first.name(), "mimetype");
        assert_eq!(first.compression(), CompressionMethod::Stored);
        drop(first);

        let meta = read(&out.data, "meta.xml");
        assert!(!meta.contains("Jane Doe") && !meta.contains("LibreOffice"));
        assert_eq!(read(&out.data, "content.xml"), "<office:body/>");
    }

    #[test]
    fn cleans_photos_embedded_in_the_package() {
        let photo = [
            &[0xFF, 0xD8, 0xFF, 0xFE, 0x00, 0x06][..],
            b"Jane",
            &[0xFF, 0xDA, 0x00, 0x04, 0x01, 0x00, 0x12, 0x34, 0xFF, 0xD9],
        ]
        .concat();
        let mut w = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, body) in [
            ("[Content_Types].xml", b"<Types/>".as_slice()),
            ("word/media/image1.jpeg", photo.as_slice()),
        ] {
            w.start_file(name, SimpleFileOptions::default()).unwrap();
            w.write_all(body).unwrap();
        }
        let docx = w.finish().unwrap().into_inner();

        let out = strip(&docx, DocKind::Ooxml).unwrap();
        let f = out.findings.iter().find(|f| f.label == "Embedded image").expect("reported");
        assert!(f.detail.contains("word/media/image1.jpeg"), "{f:?}");

        let mut zip = ZipArchive::new(Cursor::new(&out.data)).unwrap();
        let mut body = Vec::new();
        zip.by_name("word/media/image1.jpeg").unwrap().read_to_end(&mut body).unwrap();
        assert!(!body.windows(4).any(|w| w == b"Jane"));
        assert!(strip(&out.data, DocKind::Ooxml).unwrap().findings.is_empty());
    }

    #[test]
    fn a_clean_document_reports_nothing_to_remove() {
        let out = strip(&docx(), DocKind::Ooxml).unwrap();
        let again = strip(&out.data, DocKind::Ooxml).unwrap();
        assert!(again.findings.is_empty(), "{:?}", again.findings);
    }
}
