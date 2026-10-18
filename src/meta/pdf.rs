//! PDF documents.
//!
//! Unlike the other formats, this one is not a hand-written container walk.
//! A PDF is a graph of numbered objects addressed through a cross-reference
//! table that may itself be a compressed stream, and rewriting it by hand
//! risks producing a file that no longer opens. `lopdf` parses the graph and
//! writes it back out.
//!
//! The file is **rebuilt**, never appended to. A PDF can be edited by tacking
//! an incremental update onto the end, which is far easier — but the old bytes
//! stay in the file, merely superseded. For a tool whose whole job is removal,
//! leaving the metadata physically present and recoverable would be a bug, so
//! the document is loaded and written afresh. That also drops any earlier
//! revisions the file was carrying.

use lopdf::{Dictionary, Document, Object, ObjectId};

use super::{Finding, StripResult};

/// Everything in the information dictionary goes.
///
/// This is deliberately harsher than the office-document path, where an
/// authored title is kept. A PDF's `/Title` is usually not typed by anyone: it
/// is filled in by the exporter, very often with the source document's file
/// name — "Microsoft Word - salary_review_draft3.docx" is a classic
/// disclosure. Stripping the dictionary wholesale is also what the established
/// tools do for this format.
fn info_label(key: &[u8]) -> Option<&'static str> {
    Some(match key {
        b"Author" => "Author",
        b"Creator" => "Written with",
        b"Producer" => "Exported by",
        b"Title" => "Title",
        b"Subject" => "Subject",
        b"Keywords" => "Keywords",
        b"CreationDate" => "Created",
        b"ModDate" => "Last modified",
        b"Trapped" => return None,
        _ => "Document property",
    })
}

pub fn strip(data: &[u8]) -> Result<StripResult, String> {
    let mut doc = Document::load_mem(data).map_err(|e| format!("could not read the PDF: {e}"))?;

    // An encrypted PDF would have to be written back out unencrypted, which
    // silently drops whatever restriction it was carrying — including the
    // common case of a document that opens freely but forbids printing or
    // copying. `was_encrypted` catches the ones lopdf decrypted on load.
    if doc.is_encrypted() || doc.was_encrypted() {
        return Err(
            "this PDF is encrypted — cleaning it would remove its protection, so rmMeta leaves it alone"
                .into(),
        );
    }

    let mut findings = Vec::new();

    // The file identifier persists across saves and ties copies of a document
    // together, so it is an identifier in its own right.
    if doc.trailer.get(b"ID").is_ok() {
        doc.trailer.remove(b"ID");
        findings.push(Finding::new(
            "File identifier",
            "a fixed id that links this file to other copies and revisions",
        ));
    }

    strip_info(&mut doc, &mut findings);
    strip_xmp(&mut doc, &mut findings);
    strip_annotations(&mut doc, &mut findings);
    strip_private_data(&mut doc, &mut findings);
    strip_embedded_images(&mut doc, &mut findings);

    // A linearised ("fast web view") file ends its first-page section with a
    // `%%EOF` of its own, so one of the markers is not a revision.
    let linearized = doc.objects.values().any(|o| dictionary_of(o).is_some_and(|d| d.has(b"Linearized")));

    // Must come last: the removals above orphan the objects they detach, and
    // this is what actually drops them.
    let orphans = prune_unreachable(&mut doc);
    let revisions = data.windows(5).filter(|w| *w == b"%%EOF").count().saturating_sub(usize::from(linearized));
    if !orphans.is_empty() {
        findings.push(Finding::new(
            "Earlier revisions",
            format!(
                "{} object{} left over from previous versions of this document",
                orphans.len(),
                if orphans.len() == 1 { "" } else { "s" }
            ),
        ));
    } else if revisions > 1 {
        findings.push(Finding::new(
            "Earlier revisions",
            format!(
                "{} previous version{} of this document folded into the file",
                revisions - 1,
                if revisions == 2 { "" } else { "s" }
            ),
        ));
    }

    let mut out = Vec::with_capacity(data.len());
    doc.save_to(&mut out).map_err(|e| format!("could not write the PDF: {e}"))?;

    Ok(StripResult::new(out, findings))
}

fn strip_info(doc: &mut Document, findings: &mut Vec<Finding>) {
    let Ok(reference) = doc.trailer.get(b"Info") else { return };
    let id = match reference {
        Object::Reference(id) => Some(*id),
        _ => None,
    };

    let entries: Vec<(Vec<u8>, Object)> = match id.and_then(|id| doc.get_dictionary(id).ok()) {
        Some(dict) => dict.iter().map(|(k, v)| (k.to_vec(), v.clone())).collect(),
        None => return,
    };

    for (key, value) in entries {
        let Some(label) = info_label(&key) else { continue };
        let Some(text) = text_of(&value) else { continue };
        let detail = if key.ends_with(b"Date") { pretty_date(&text) } else { text };
        let label = if label == "Document property" {
            format!("Document property: {}", String::from_utf8_lossy(&key))
        } else {
            label.to_string()
        };
        findings.push(Finding::new(label, detail));
    }

    doc.trailer.remove(b"Info");
    if let Some(id) = id {
        doc.objects.remove(&id);
    }
}

/// XMP is a second, parallel copy of the metadata as an XML stream — removing
/// only the information dictionary leaves the author's name sitting in it.
fn strip_xmp(doc: &mut Document, findings: &mut Vec<Finding>) {
    let mut removed = Vec::new();
    let holders: Vec<ObjectId> = doc
        .objects
        .iter()
        .filter(|(_, object)| dictionary_of(object).is_some_and(|d| d.has(b"Metadata")))
        .map(|(id, _)| *id)
        .collect();

    for holder in holders {
        if let Some(dict) = doc.objects.get_mut(&holder).and_then(dictionary_of_mut) {
            if let Ok(Object::Reference(target)) = dict.get(b"Metadata") {
                removed.push(*target);
            }
            dict.remove(b"Metadata");
        }
    }

    for id in &removed {
        doc.objects.remove(id);
    }
    if !removed.is_empty() {
        findings.push(Finding::new(
            "XMP metadata",
            "a second copy of the author, dates and editing history as XML",
        ));
    }
}

/// Comments and highlights record who left them and when. The note's text is
/// content and stays; the name attached to it does not.
///
/// Annotations are found two ways, because `/Type /Annot` is optional: by that
/// type, and by being listed in a page's `/Annots` array — directly or by
/// reference.
fn strip_annotations(doc: &mut Document, findings: &mut Vec<Finding>) {
    let mut authors = Vec::new();

    let mut annotations: Vec<ObjectId> = Vec::new();
    let mut arrays: Vec<ObjectId> = Vec::new();
    for (id, object) in &doc.objects {
        let Some(dict) = dictionary_of(object) else { continue };
        if matches!(dict.get(b"Type"), Ok(Object::Name(name)) if name == b"Annot") {
            annotations.push(*id);
        }
        match dict.get(b"Annots") {
            Ok(Object::Array(items)) => annotations.extend(items.iter().filter_map(reference)),
            Ok(Object::Reference(array)) => arrays.push(*array),
            _ => {}
        }
    }
    for array in arrays {
        if let Some(Object::Array(items)) = doc.objects.get(&array) {
            annotations.extend(items.iter().filter_map(reference));
        }
    }
    annotations.sort();
    annotations.dedup();

    for id in annotations {
        if let Some(dict) = doc.objects.get_mut(&id).and_then(dictionary_of_mut) {
            scrub_annotation(dict, &mut authors);
        }
    }
    // An annotation may also sit in the array itself rather than behind a
    // reference.
    for object in doc.objects.values_mut() {
        let Some(dict) = dictionary_of_mut(object) else { continue };
        if let Ok(Object::Array(items)) = dict.get_mut(b"Annots") {
            for item in items {
                if let Object::Dictionary(annotation) = item {
                    scrub_annotation(annotation, &mut authors);
                }
            }
        }
    }

    authors.sort();
    authors.dedup();
    if !authors.is_empty() {
        findings.push(Finding::new("Comment authors", authors.join(", ")));
    }
}

/// A form field's widget is an annotation too, but its `/T` is the field's
/// *name*, which the form needs; only its modification date goes.
fn scrub_annotation(dict: &mut Dictionary, authors: &mut Vec<String>) {
    let widget = matches!(dict.get(b"Subtype"), Ok(Object::Name(name)) if name == b"Widget");
    if !widget {
        if let Some(name) = dict.get(b"T").ok().and_then(text_of) {
            authors.push(name);
        }
        dict.remove(b"T");
        dict.remove(b"CreationDate");
    }
    dict.remove(b"M");
}

fn reference(object: &Object) -> Option<ObjectId> {
    match object {
        Object::Reference(id) => Some(*id),
        _ => None,
    }
}

/// JPEG images are stored whole: a `/DCTDecode` stream *is* the JPEG file, Exif
/// and GPS included when the photo came straight from a camera. It is cleaned
/// the same way a JPEG on disk is, which never touches the compressed pixels.
fn strip_embedded_images(doc: &mut Document, findings: &mut Vec<Finding>) {
    let mut count = 0usize;
    let mut what: Vec<String> = Vec::new();

    for object in doc.objects.values_mut() {
        let Object::Stream(stream) = object else { continue };
        let only_dct = stream.filters().is_ok_and(|f| f == [b"DCTDecode".as_slice()]);
        if !only_dct {
            continue;
        }
        // Only JPEGs get here, so the TIFF-only options do not apply.
        let Some(found) = super::clean_embedded_image(&stream.content, &super::Options::default())
        else {
            continue;
        };
        count += 1;
        for f in &found.findings {
            if !what.contains(&f.label) {
                what.push(f.label.clone());
            }
        }
        stream.set_content(found.cleaned);
    }

    if count > 0 {
        findings.push(Finding::new(
            "Embedded image",
            format!(
                "{count} picture{} carrying {}",
                if count == 1 { "" } else { "s" },
                what.join(", ")
            ),
        ));
    }
}

/// `/PieceInfo` is a scratchpad where applications keep private data about the
/// document between editing sessions.
fn strip_private_data(doc: &mut Document, findings: &mut Vec<Finding>) {
    let holders: Vec<ObjectId> = doc
        .objects
        .iter()
        .filter(|(_, object)| dictionary_of(object).is_some_and(|d| d.has(b"PieceInfo")))
        .map(|(id, _)| *id)
        .collect();

    let found = !holders.is_empty();
    for id in holders {
        if let Some(dict) = doc.objects.get_mut(&id).and_then(dictionary_of_mut) {
            dict.remove(b"PieceInfo");
        }
    }
    if found {
        findings.push(Finding::new(
            "Application data",
            "private working data left behind by the editor",
        ));
    }
}

/// Drop every object the document cannot reach from its trailer.
///
/// A PDF edited by incremental update keeps the objects of every earlier
/// revision in the file. Loading follows the cross-reference chain, so those
/// superseded objects are in memory too — and writing them back out would put
/// the old author, the old comment, the pre-redaction page straight into the
/// "cleaned" file. `lopdf` has no pruning of its own, so this walks the graph
/// from the trailer and keeps only what is still referenced.
/// Returns only the orphans that represent real leftover content. Everything
/// unreachable is removed either way.
fn prune_unreachable(doc: &mut Document) -> Vec<ObjectId> {
    let reachable: std::collections::HashSet<ObjectId> =
        doc.traverse_objects(|_| {}).into_iter().collect();

    let orphans: Vec<ObjectId> = doc
        .objects
        .keys()
        .filter(|id| !reachable.contains(id))
        .copied()
        .collect();

    // The cross-reference stream and any object streams are reached through
    // the file's byte offsets, not by reference, so they always land in this
    // list. They are pure structure — lopdf has already expanded the object
    // streams into real objects and rebuilds the cross-reference table on
    // save, so dropping them is right (it also discards the stale compressed
    // copies inside them). They are not evidence of an earlier revision,
    // though, and reporting them as such would mean every cleaned file
    // claimed to still hold one.
    // A linearised file adds two more of the same kind: the linearisation
    // dictionary and the hint stream, both located by byte offset. The hint
    // stream has no /Type; it is recognised by its required /S entry.
    let structural = |doc: &Document, id: &ObjectId| {
        let Some(object) = doc.objects.get(id) else { return false };
        let Some(d) = dictionary_of(object) else { return false };
        let typed = |t: &[u8]| matches!(d.get(b"Type"), Ok(Object::Name(n)) if n == t);
        typed(b"XRef")
            || typed(b"ObjStm")
            || d.has(b"Linearized")
            || (matches!(object, Object::Stream(_)) && d.has(b"S") && !d.has(b"Type"))
    };
    let substantive: Vec<ObjectId> =
        orphans.iter().filter(|id| !structural(doc, id)).copied().collect();

    for id in &orphans {
        doc.objects.remove(id);
    }
    substantive
}

fn dictionary_of(object: &Object) -> Option<&Dictionary> {
    match object {
        Object::Dictionary(d) => Some(d),
        Object::Stream(s) => Some(&s.dict),
        _ => None,
    }
}

fn dictionary_of_mut(object: &mut Object) -> Option<&mut Dictionary> {
    match object {
        Object::Dictionary(d) => Some(d),
        Object::Stream(s) => Some(&mut s.dict),
        _ => None,
    }
}

/// PDF text strings are either UTF-16BE behind a byte-order mark or an
/// 8-bit encoding close enough to Latin-1 for display purposes.
fn text_of(object: &Object) -> Option<String> {
    let raw = match object {
        Object::String(bytes, _) => bytes.as_slice(),
        Object::Name(bytes) => bytes.as_slice(),
        _ => return None,
    };

    let text = if raw.starts_with(&[0xFE, 0xFF]) {
        let units: Vec<u16> = raw[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_be_bytes(*pair))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        raw.iter().map(|&b| b as char).collect()
    };

    let text: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// `D:20260902165303+02'00'` reads better as `2026-09-02 16:53`.
fn pretty_date(raw: &str) -> String {
    let digits: String = raw.trim_start_matches("D:").chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.len() < 12 {
        return raw.to_string();
    }
    format!(
        "{}-{}-{} {}:{}",
        &digits[0..4],
        &digits[4..6],
        &digits[6..8],
        &digits[8..10],
        &digits[10..12]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{dictionary, Stream, StringFormat};

    /// A one-page PDF carrying an Info dictionary and an XMP stream.
    fn build(with_xmp: bool) -> Vec<u8> {
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 200.into(), 200.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
            }),
        );
        let mut catalog = dictionary! { "Type" => "Catalog", "Pages" => pages_id };

        if with_xmp {
            let xmp = doc.add_object(Stream::new(
                dictionary! { "Type" => "Metadata", "Subtype" => "XML" },
                br#"<x:xmpmeta><dc:creator>Jane Doe</dc:creator></x:xmpmeta>"#.to_vec(),
            ));
            catalog.set("Metadata", xmp);
        }

        let catalog_id = doc.add_object(catalog);
        let info_id = doc.add_object(dictionary! {
            "Author" => Object::String(b"Jane Doe".to_vec(), StringFormat::Literal),
            "Producer" => Object::String(b"LibreOffice 26.2".to_vec(), StringFormat::Literal),
            "Title" => Object::String(b"Microsoft Word - draft3.docx".to_vec(), StringFormat::Literal),
            "CreationDate" => Object::String(b"D:20260902165303+02'00'".to_vec(), StringFormat::Literal),
        });
        doc.trailer.set("Root", catalog_id);
        doc.trailer.set("Info", info_id);
        doc.trailer.set(
            "ID",
            vec![
                Object::String(b"abc".to_vec(), StringFormat::Hexadecimal),
                Object::String(b"abc".to_vec(), StringFormat::Hexadecimal),
            ],
        );

        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    fn labels(findings: &[Finding]) -> Vec<&str> {
        findings.iter().map(|f| f.label.as_str()).collect()
    }

    #[test]
    fn removes_the_information_dictionary_including_the_title() {
        let result = strip(&build(false)).unwrap();
        let labels = labels(&result.findings);

        for expected in ["Author", "Exported by", "Title", "Created", "File identifier"] {
            assert!(labels.contains(&expected), "missing {expected} in {labels:?}");
        }
        assert!(!result.data.windows(8).any(|w| w == b"Jane Doe"));
        assert!(!result.data.windows(11).any(|w| w == b"draft3.docx"));

        // The date is shown in a form a person can read.
        let created = result.findings.iter().find(|f| f.label == "Created").unwrap();
        assert_eq!(created.detail, "2026-09-02 16:53");
    }

    #[test]
    fn removes_the_xmp_stream_as_well_as_the_info_dictionary() {
        let result = strip(&build(true)).unwrap();
        assert!(labels(&result.findings).contains(&"XMP metadata"));
        assert!(!result.data.windows(8).any(|w| w == b"Jane Doe"));
        assert!(!result.data.windows(7).any(|w| w == b"xmpmeta"));
    }

    #[test]
    fn the_result_still_parses_and_is_already_clean() {
        let once = strip(&build(true)).unwrap();
        let twice = strip(&once.data).unwrap();
        assert!(twice.findings.is_empty(), "{:?}", twice.findings);

        let doc = Document::load_mem(&once.data).unwrap();
        assert!(doc.trailer.get(b"Info").is_err());
        assert_eq!(doc.get_pages().len(), 1, "the page survived");
    }

    #[test]
    fn drops_objects_left_behind_by_an_earlier_revision() {
        let mut doc = Document::load_mem(&build(false)).unwrap();
        // Stand-in for the Info dictionary of a superseded revision: still in
        // the file, still in the older cross-reference section, no longer
        // pointed at by the trailer.
        doc.add_object(dictionary! {
            "Author" => Object::String(b"Ghost Author".to_vec(), StringFormat::Literal),
        });
        let mut with_orphan = Vec::new();
        doc.save_to(&mut with_orphan).unwrap();
        assert!(with_orphan.windows(12).any(|w| w == b"Ghost Author"));

        let result = strip(&with_orphan).unwrap();
        assert!(
            !result.data.windows(12).any(|w| w == b"Ghost Author"),
            "an unreferenced object from an earlier revision survived"
        );
        assert!(labels(&result.findings).contains(&"Earlier revisions"));
    }

    #[test]
    fn a_cleaned_file_does_not_claim_to_hold_earlier_revisions() {
        // The cross-reference stream is unreachable by reference too, so it
        // must not be mistaken for revision residue on every subsequent pass.
        let once = strip(&build(true)).unwrap();
        let twice = strip(&once.data).unwrap();
        assert!(!labels(&twice.findings).contains(&"Earlier revisions"), "{:?}", twice.findings);
    }

    /// One page with the given annotations in its /Annots array.
    fn with_annotations(annotations: Vec<Dictionary>, form: bool) -> Vec<u8> {
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let refs: Vec<Object> = annotations.into_iter().map(|a| doc.add_object(a).into()).collect();
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Annots" => refs.clone(),
            "MediaBox" => vec![0.into(), 0.into(), 200.into(), 200.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1 }),
        );
        let mut catalog = dictionary! { "Type" => "Catalog", "Pages" => pages_id };
        if form {
            catalog.set("AcroForm", dictionary! { "Fields" => refs });
        }
        let catalog_id = doc.add_object(catalog);
        doc.trailer.set("Root", catalog_id);
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    #[test]
    fn a_linearised_file_is_not_mistaken_for_a_revised_one() {
        // Assembled by hand: lopdf's writer leaves linearisation dictionaries
        // out. Object 1 is that dictionary and object 5 the hint stream, both
        // located by byte offset and so unreachable by reference; the first
        // `%%EOF` closes the first-page section.
        let objects = [
            "<< /Linearized 1 /L 0 /N 1 >>\nendobj\n%%EOF\n",
            "<< /Type /Catalog /Pages 3 0 R >>\nendobj\n",
            "<< /Type /Pages /Kids [4 0 R] /Count 1 >>\nendobj\n",
            "<< /Type /Page /Parent 3 0 R /MediaBox [0 0 10 10] >>\nendobj\n",
            "<< /S 4 /Length 4 >>\nstream\nabcd\nendstream\nendobj\n",
        ];
        let mut out = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{body}", i + 1).as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(b"xref\n0 6\n0000000000 65535 f \n");
        for at in offsets {
            out.extend_from_slice(format!("{at:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(format!("trailer\n<< /Size 6 /Root 2 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes());

        let result = strip(&out).unwrap();
        assert!(!labels(&result.findings).contains(&"Earlier revisions"), "{:?}", result.findings);
    }

    #[test]
    fn form_fields_keep_their_names() {
        let widget = dictionary! {
            "Type" => "Annot", "Subtype" => "Widget", "FT" => "Tx",
            "T" => Object::String(b"applicant_name".to_vec(), StringFormat::Literal),
        };
        let result = strip(&with_annotations(vec![widget], true)).unwrap();
        assert!(result.data.windows(14).any(|w| w == b"applicant_name"), "the field lost its name");
        assert!(!labels(&result.findings).contains(&"Comment authors"), "{:?}", result.findings);
    }

    #[test]
    fn comment_authors_go_even_without_an_annot_type() {
        // /Type is optional on annotations; this one is found through /Annots.
        let note = dictionary! {
            "Subtype" => "Text",
            "T" => Object::String(b"Jane Doe".to_vec(), StringFormat::Literal),
            "Contents" => Object::String(b"check this".to_vec(), StringFormat::Literal),
        };
        let result = strip(&with_annotations(vec![note], false)).unwrap();
        assert!(!result.data.windows(8).any(|w| w == b"Jane Doe"));
        assert!(result.data.windows(10).any(|w| w == b"check this"), "the comment is content");
        assert!(labels(&result.findings).contains(&"Comment authors"));
    }

    #[test]
    fn cleans_embedded_jpeg_images() {
        let photo = [
            &[0xFF, 0xD8, 0xFF, 0xFE, 0x00, 0x06][..],
            b"Jane",
            &[0xFF, 0xDA, 0x00, 0x04, 0x01, 0x00, 0x12, 0x34, 0xFF, 0xD9],
        ]
        .concat();
        let mut doc = Document::load_mem(&build(false)).unwrap();
        let image = doc.add_object(Stream::new(
            dictionary! { "Type" => "XObject", "Subtype" => "Image", "Filter" => "DCTDecode" },
            photo,
        ));
        let catalog = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
        doc.get_dictionary_mut(catalog).unwrap().set("Thumb", image);
        let mut with_image = Vec::new();
        doc.save_to(&mut with_image).unwrap();

        let result = strip(&with_image).unwrap();
        assert!(labels(&result.findings).contains(&"Embedded image"), "{:?}", result.findings);
        assert!(!result.data.windows(4).any(|w| w == b"Jane"));
        assert!(strip(&result.data).unwrap().findings.is_empty());
    }

    #[test]
    fn rejects_rubbish_without_panicking() {
        assert!(strip(b"%PDF-1.7 but not really").is_err());
        let full = build(true);
        for n in (0..full.len()).step_by(97) {
            let _ = strip(&full[..n]);
        }
    }
}
