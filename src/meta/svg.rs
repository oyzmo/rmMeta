//! SVG.
//!
//! The only text format here, and the only one where the metadata is written
//! in the same language as the picture. Vector editors leave a lot behind: an
//! RDF block naming the author, a `sodipodi:namedview` element recording the
//! window geometry and zoom of the machine it was drawn on, `inkscape:`
//! attributes on every layer, and a "Created with ..." comment.
//!
//! `<title>` and `<desc>` stay. They are what screen readers announce, so they
//! are content, not metadata.

use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, Writer};

use super::{Finding, StripResult};

/// Elements removed with everything inside them.
fn is_unwanted_element(local: &[u8]) -> Option<(&'static str, &'static str)> {
    match local {
        b"metadata" => Some(("Metadata", "author, licence and editing history as RDF")),
        b"namedview" => Some((
            "Editor state",
            "window size, zoom and cursor position from the machine it was drawn on",
        )),
        _ => None,
    }
}

/// Attribute prefixes that belong to an editor rather than to the drawing.
const EDITOR_PREFIXES: [&[u8]; 2] = [b"inkscape:", b"sodipodi:"];

/// Elements in an editor's namespace — `inkscape:perspective`,
/// `inkscape:path-effect` — are its working state; renderers ignore them. They
/// go with the namespace declarations: keeping one after its `xmlns:` is
/// removed leaves an unbound prefix, and the file no longer parses.
fn is_editor_element(qualified: &[u8]) -> Option<(&'static str, &'static str)> {
    EDITOR_PREFIXES.iter().any(|p| qualified.starts_with(p)).then_some((
        "Editor data",
        "path effects, perspectives and other drawing-program state",
    ))
}

pub fn detect(data: &[u8]) -> bool {
    // Skip a byte-order mark and any leading whitespace, then insist the file
    // actually starts as markup before looking for the root element.
    let text = data.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(data);
    let start = text.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(0);
    if text.get(start) != Some(&b'<') {
        return false;
    }
    let head = &text[start..text.len().min(start + 4096)];
    head.windows(4).any(|w| w == b"<svg")
        && !head.windows(5).any(|w| w.eq_ignore_ascii_case(b"<html"))
}

pub fn strip(data: &[u8], options: &super::Options) -> Result<StripResult, String> {
    let mut reader = Reader::from_reader(data);
    let mut writer = Writer::new(Vec::new());
    let mut findings: Vec<Finding> = Vec::new();
    let mut buf = Vec::new();

    let mut kept: Vec<Finding> = Vec::new();
    let mut skip_depth = 0usize;
    let mut editor_attributes = 0usize;

    loop {
        let event = reader
            .read_event_into(&mut buf)
            .map_err(|e| format!("could not parse the SVG: {e}"))?;

        match &event {
            Event::Eof => break,

            // Inside an element being dropped: swallow everything until it
            // closes, counting nesting so an inner </g> cannot end it early.
            _ if skip_depth > 0 => {
                match &event {
                    Event::Start(_) => skip_depth += 1,
                    Event::End(_) => skip_depth -= 1,
                    _ => {}
                }
                buf.clear();
                continue;
            }

            Event::Comment(text) => {
                let text = String::from_utf8_lossy(text.as_ref()).trim().to_string();
                if !text.is_empty() {
                    findings.push(Finding::new("Comment", summarise(&text)));
                }
                buf.clear();
                continue;
            }

            Event::Start(e) | Event::Empty(e) => {
                let local = local_name(e.name().as_ref()).to_vec();
                let unwanted =
                    is_unwanted_element(&local).or_else(|| is_editor_element(e.name().as_ref()));
                if let Some((label, detail)) = unwanted {
                    if !findings.iter().any(|f| f.label == label) {
                        findings.push(Finding::new(label, detail));
                    }
                    // An empty element has no matching end tag to wait for.
                    if matches!(event, Event::Start(_)) {
                        skip_depth = 1;
                    }
                    buf.clear();
                    continue;
                }

                let cleaned = clean_attributes(e, options);
                editor_attributes += cleaned.dropped;
                for found in &cleaned.images {
                    findings.push(super::embedded_finding("picture embedded in the drawing", found));
                    super::merge_kept(&mut kept, found);
                }
                // Untouched tags are written exactly as they were read.
                let Some(rebuilt) = cleaned.element else {
                    writer.write_event(event.borrow()).map_err(write_err)?;
                    buf.clear();
                    continue;
                };
                let event = if matches!(event, Event::Start(_)) {
                    Event::Start(rebuilt)
                } else {
                    Event::Empty(rebuilt)
                };
                writer.write_event(event).map_err(write_err)?;
                buf.clear();
                continue;
            }

            _ => {}
        }

        writer.write_event(event.borrow()).map_err(write_err)?;
        buf.clear();
    }

    if editor_attributes > 0 {
        findings.push(Finding::new(
            "Editor attributes",
            format!("{editor_attributes} layer and editor markers left by the drawing program"),
        ));
    }

    Ok(StripResult::new(writer.into_inner(), findings).keeping(kept))
}

struct CleanedTag {
    /// `None` when nothing changed and the tag should be written as read.
    element: Option<BytesStart<'static>>,
    dropped: usize,
    images: Vec<super::Examined>,
}

/// Rebuilds the tag without the editor's own attributes, without the namespace
/// declarations only those used, and with any embedded picture cleaned.
fn clean_attributes(element: &BytesStart, options: &super::Options) -> CleanedTag {
    let name = String::from_utf8_lossy(element.name().as_ref()).into_owned();
    let mut rebuilt = BytesStart::new(name);
    let mut dropped = 0;
    let mut images = Vec::new();

    for attribute in element.attributes().flatten() {
        let key = attribute.key.as_ref();
        let is_editor = EDITOR_PREFIXES.iter().any(|p| key.starts_with(p))
            || key == b"xmlns:inkscape"
            || key == b"xmlns:sodipodi";
        if is_editor {
            dropped += 1;
            continue;
        }
        if local_name(key) == b"href" {
            let value = attribute.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok();
            if let Some((uri, found)) = value.and_then(|v| clean_data_uri(&v, options)) {
                rebuilt.push_attribute((key, uri.as_bytes()));
                images.push(found);
                continue;
            }
        }
        // The raw value is written back unescaped, always between double
        // quotes. A value that was single-quoted may hold a literal `"`, which
        // would end the attribute early; it is the one character to escape.
        let raw = attribute.value.as_ref();
        if raw.contains(&b'"') {
            let escaped: Vec<u8> = raw
                .iter()
                .flat_map(|&b| if b == b'"' { b"&quot;".to_vec() } else { vec![b] })
                .collect();
            rebuilt.push_attribute((key, escaped.as_slice()));
        } else {
            rebuilt.push_attribute((key, raw));
        }
    }

    let changed = dropped > 0 || !images.is_empty();
    CleanedTag { element: changed.then_some(rebuilt), dropped, images }
}

/// `data:image/jpeg;base64,...` with its picture cleaned, re-encoded without
/// the line breaks editors wrap it in. `None` when it is not an embedded
/// picture or there was nothing to remove.
fn clean_data_uri(value: &str, options: &super::Options) -> Option<(String, super::Examined)> {
    let value = value.trim();
    let rest = value.get(..5).filter(|p| p.eq_ignore_ascii_case("data:")).map(|_| &value[5..])?;
    let (header, payload) = rest.split_once(',')?;
    if !header.to_ascii_lowercase().ends_with(";base64") {
        return None;
    }
    let bytes = base64_decode(payload)?;
    let found = super::clean_embedded_image(&bytes, options)?;
    let uri = format!("data:{header},{}", base64_encode(&found.cleaned));
    Some((uri, found))
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0;
    for c in text.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            c if c.is_ascii_whitespace() => continue,
            _ => return None,
        };
        // Fewer than fourteen bits are ever pending; keep it from overflowing.
        acc = ((acc << 6) | v as u32) & 0xFFFF;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, &b)| n | (b as u32) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(BASE64[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn local_name(qualified: &[u8]) -> &[u8] {
    match qualified.iter().rposition(|&b| b == b':') {
        Some(i) => &qualified[i + 1..],
        None => qualified,
    }
}

fn summarise(text: &str) -> String {
    let flat: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(80) {
        Some((i, _)) => format!("{}\u{2026}", &flat[..i]),
        None => flat,
    }
}

fn write_err(e: std::io::Error) -> String {
    format!("could not write the SVG: {e}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip(data: &[u8]) -> Result<StripResult, String> {
        super::strip(data, &crate::meta::Options::default())
    }

    const INKSCAPE: &[u8] = br##"<?xml version="1.0"?>
<!-- Created with Inkscape (http://www.inkscape.org/) -->
<svg xmlns="http://www.w3.org/2000/svg" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:inkscape="http://www.inkscape.org/namespaces/inkscape" xmlns:sodipodi="http://sodipodi.sourceforge.net/DTD/sodipodi-0.dtd" width="100" height="100">
  <title>A green square</title>
  <desc>For the test suite</desc>
  <sodipodi:namedview inkscape:zoom="1.4" inkscape:cx="50" inkscape:window-maximized="1" inkscape:current-layer="layer1"/>
  <metadata><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><cc:Work><dc:creator><cc:Agent><dc:title>Jane Doe</dc:title></cc:Agent></dc:creator></cc:Work></rdf:RDF></metadata>
  <g inkscape:label="Layer 1" inkscape:groupmode="layer" id="layer1">
    <rect width="80" height="80" x="10" y="10" fill="#7FA07A"/>
  </g>
</svg>"##;

    fn cleaned() -> String {
        String::from_utf8(strip(INKSCAPE).unwrap().data).unwrap()
    }

    #[test]
    fn detects_svg_without_claiming_every_xml_file() {
        assert!(detect(INKSCAPE));
        assert!(detect(b"  <svg xmlns='http://www.w3.org/2000/svg'/>"));
        assert!(!detect(b"<?xml version='1.0'?><rss><channel/></rss>"));
        assert!(!detect(b"<!DOCTYPE html><html><body><svg/></body></html>"));
        assert!(!detect(b"not markup at all"));
    }

    #[test]
    fn removes_the_author_and_the_editor_traces() {
        let out = cleaned();
        assert!(!out.contains("Jane Doe"));
        assert!(!out.contains("<metadata"));
        assert!(!out.contains("namedview"));
        assert!(!out.contains("inkscape:"));
        assert!(!out.contains("sodipodi:"));
        assert!(!out.contains("Created with Inkscape"));
    }

    #[test]
    fn keeps_the_drawing_and_its_accessible_text() {
        let out = cleaned();
        assert!(out.contains(r##"<rect width="80" height="80" x="10" y="10" fill="#7FA07A"/>"##));
        assert!(out.contains("<title>A green square</title>"), "screen-reader title survives");
        assert!(out.contains("<desc>For the test suite</desc>"));
        // The layer group itself stays; only the editor's markers on it go.
        assert!(out.contains(r#"<g id="layer1">"#), "{out}");
    }

    #[test]
    fn reports_what_it_took_out() {
        let result = strip(INKSCAPE).unwrap();
        let labels: Vec<&str> = result.findings.iter().map(|f| f.label.as_str()).collect();
        for expected in ["Comment", "Editor state", "Metadata", "Editor attributes"] {
            assert!(labels.contains(&expected), "missing {expected} in {labels:?}");
        }
    }

    #[test]
    fn a_single_quoted_value_holding_a_double_quote_stays_well_formed() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:inkscape="i"><text inkscape:label="t" font-family='"Open Sans"'>hi</text></svg>"#;
        let out = String::from_utf8(strip(svg).unwrap().data).unwrap();
        assert!(out.contains(r#"font-family="&quot;Open Sans&quot;""#), "{out}");
    }

    #[test]
    fn untouched_tags_are_written_as_they_were() {
        let svg = b"<svg xmlns='http://www.w3.org/2000/svg'><rect  width='1'/></svg>";
        assert_eq!(strip(svg).unwrap().data, svg);
    }

    #[test]
    fn editor_elements_go_with_their_namespace() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:inkscape="http://www.inkscape.org/namespaces/inkscape"><defs><inkscape:perspective id="p"/><inkscape:path-effect id="e"></inkscape:path-effect></defs></svg>"#;
        let out = String::from_utf8(strip(svg).unwrap().data).unwrap();
        assert!(!out.contains("inkscape"), "an unbound prefix is left: {out}");
    }

    #[test]
    fn cleans_a_picture_embedded_as_a_data_uri() {
        let jpeg = [
            &[0xFF, 0xD8, 0xFF, 0xFE, 0x00, 0x06][..],
            b"Jane",
            &[0xFF, 0xDA, 0x00, 0x04, 0x01, 0x00, 0x12, 0x34, 0xFF, 0xD9],
        ]
        .concat();
        let encoded = base64_encode(&jpeg);
        // Wrapped the way Inkscape wraps it.
        let wrapped = format!("{}&#10;{}", &encoded[..8], &encoded[8..]);
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg"><image href="data:image/jpeg;base64,{wrapped}"/></svg>"#
        );
        let result = strip(svg.as_bytes()).unwrap();
        assert!(result.findings.iter().any(|f| f.label == "Embedded image"), "{:?}", result.findings);

        let out = String::from_utf8(result.data.clone()).unwrap();
        let payload = out.split("base64,").nth(1).unwrap().split('"').next().unwrap();
        let back = base64_decode(payload).unwrap();
        assert!(!back.windows(4).any(|w| w == b"Jane"));
        assert!(strip(&result.data).unwrap().findings.is_empty());
    }

    #[test]
    fn base64_round_trips() {
        for n in 0..8 {
            let data: Vec<u8> = (0..n).map(|i| (i * 37) as u8).collect();
            assert_eq!(base64_decode(&base64_encode(&data)).unwrap(), data);
        }
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"Ma"), "TWE=");
    }

    #[test]
    fn cleaning_twice_finds_nothing_the_second_time() {
        let once = strip(INKSCAPE).unwrap();
        let twice = strip(&once.data).unwrap();
        assert!(twice.findings.is_empty(), "{:?}", twice.findings);
    }
}
