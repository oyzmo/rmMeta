//! Blank out the text of named XML elements, leaving the document's shape
//! intact.
//!
//! Office documents are ZIPs full of cross-referenced XML, so *deleting* a
//! property element risks breaking a relationship or a content-type override
//! somewhere else in the package. Emptying the element instead is enough —
//! `<dc:creator/>` discloses nothing — and always leaves a file the office
//! suite still opens.

use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, Writer};

pub struct Stripped {
    pub xml: Vec<u8>,
    /// `(element name, the text that was removed)`, in document order.
    pub removed: Vec<(String, String)>,
}

/// Elements are matched on their qualified name (`dc:creator`) or, failing
/// that, their local name (`creator`), so a document that binds the namespaces
/// to different prefixes is still handled.
pub fn blank_elements(xml: &[u8], targets: &[&str]) -> Result<Stripped, String> {
    strip_elements(xml, targets, &[])
}

/// As `blank_elements`, and `delete` names elements that go entirely, tag and
/// attributes included. Only for parts nothing else refers to: ODF's
/// `meta:template` holds its disclosure — a file path, often with a user name
/// in it — in attributes on an empty element, and `meta:user-defined` names its
/// property in an attribute, so blanking the text would leave both behind.
pub fn strip_elements(xml: &[u8], targets: &[&str], delete: &[&str]) -> Result<Stripped, String> {
    let mut reader = Reader::from_reader(xml);
    let mut writer = Writer::new(Vec::new());
    let mut removed = Vec::new();
    let mut buf = Vec::new();

    // Non-zero while inside a target element: its own start counts as 1, and
    // nested elements push it higher so a matching End cannot close it early.
    let mut depth = 0usize;
    let mut deleting = false;
    let mut current = String::new();
    let mut label_attribute: Option<String> = None;
    let mut text = String::new();

    loop {
        let event = reader
            .read_event_into(&mut buf)
            .map_err(|e| format!("could not parse XML: {e}"))?;

        match &event {
            Event::Eof => break,
            Event::Empty(e) if depth == 0 && matches(&qname(e.name().as_ref()), delete) => {
                let (name, attributes) = attribute_text(e);
                let detail = name.or(attributes).unwrap_or_else(|| "present".into());
                removed.push((qname(e.name().as_ref()), detail));
                buf.clear();
                continue;
            }
            Event::Start(e) => {
                let name = qname(e.name().as_ref());
                if depth > 0 {
                    // Nested markup inside a target goes with it.
                    depth += 1;
                    buf.clear();
                    continue;
                } else if matches(&name, delete) {
                    depth = 1;
                    deleting = true;
                    label_attribute = attribute_text(e).0;
                    current = name;
                    text.clear();
                    buf.clear();
                    continue;
                } else if matches(&name, targets) {
                    depth = 1;
                    deleting = false;
                    current = name;
                    text.clear();
                    writer.write_event(event.borrow()).map_err(write_err)?;
                    buf.clear();
                    continue;
                }
            }
            Event::End(_) if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    let value = text.trim().to_string();
                    let value = match label_attribute.take() {
                        Some(name) if value.is_empty() => name,
                        Some(name) => format!("{name}: {value}"),
                        None => value,
                    };
                    if !value.is_empty() {
                        removed.push((current.clone(), value));
                    }
                    if !deleting {
                        writer.write_event(event.borrow()).map_err(write_err)?;
                    }
                }
                buf.clear();
                continue;
            }
            Event::Text(t) if depth > 0 => {
                if let Ok(s) = t.decode() {
                    text.push_str(&s);
                }
                buf.clear();
                continue;
            }
            // A value may be written partly or wholly as CDATA or as entity
            // and character references; it is removed either way, so it is
            // reported either way.
            Event::CData(t) if depth > 0 => {
                if let Ok(s) = t.decode() {
                    text.push_str(&s);
                }
                buf.clear();
                continue;
            }
            Event::GeneralRef(r) if depth > 0 => {
                match r.resolve_char_ref() {
                    Ok(Some(c)) => text.push(c),
                    _ => {
                        let name = r.decode().unwrap_or_default();
                        match quick_xml::escape::resolve_predefined_entity(&name) {
                            Some(s) => text.push_str(s),
                            None => text.push_str(&format!("&{name};")),
                        }
                    }
                }
                buf.clear();
                continue;
            }
            _ if depth > 0 => {
                // Comments and nested markup inside a target: dropped.
                buf.clear();
                continue;
            }
            _ => {}
        }

        writer.write_event(event.borrow()).map_err(write_err)?;
        buf.clear();
    }

    Ok(Stripped { xml: writer.into_inner(), removed })
}

/// Empty the attributes named in `blank` and remove those named in `drop`, on
/// every element; names match as in `strip_elements`. This is for identity
/// carried in attributes on content — `w:author` on a tracked change or a
/// comment — where the element itself is the document and must stay. A tag
/// with nothing to change is written back exactly as read.
pub fn scrub_attributes(xml: &[u8], blank: &[&str], drop: &[&str]) -> Result<Stripped, String> {
    let mut reader = Reader::from_reader(xml);
    let mut writer = Writer::new(Vec::new());
    let mut removed = Vec::new();
    let mut buf = Vec::new();

    loop {
        let event = reader
            .read_event_into(&mut buf)
            .map_err(|e| format!("could not parse XML: {e}"))?;
        let rebuilt = match &event {
            Event::Eof => break,
            Event::Start(e) | Event::Empty(e) => scrub_tag(e, blank, drop, &mut removed),
            _ => None,
        };
        match (rebuilt, &event) {
            (Some(tag), Event::Start(_)) => writer.write_event(Event::Start(tag)),
            (Some(tag), _) => writer.write_event(Event::Empty(tag)),
            (None, _) => writer.write_event(event.borrow()),
        }
        .map_err(write_err)?;
        buf.clear();
    }

    Ok(Stripped { xml: writer.into_inner(), removed })
}

fn scrub_tag(
    element: &BytesStart,
    blank: &[&str],
    drop: &[&str],
    removed: &mut Vec<(String, String)>,
) -> Option<BytesStart<'static>> {
    let mut rebuilt = BytesStart::new(qname(element.name().as_ref()));
    let mut changed = false;
    for attribute in element.attributes().flatten() {
        let key = qname(attribute.key.as_ref());
        let is_blank = matches(&key, blank);
        if is_blank || matches(&key, drop) {
            let value = attribute
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .map(|v| v.trim().to_string())
                .unwrap_or_default();
            if value.is_empty() {
                // Already blank: keep as is, so a cleaned file stays clean.
                rebuilt.push_attribute((key.as_bytes(), attribute.value.as_ref()));
                continue;
            }
            removed.push((key.clone(), value));
            changed = true;
            if is_blank {
                rebuilt.push_attribute((key.as_bytes(), b"".as_slice()));
            }
            continue;
        }
        // Read raw and written back raw, always between double quotes, so a
        // `"` from a single-quoted value is the one character to escape.
        let raw = attribute.value.as_ref();
        if raw.contains(&b'"') {
            let escaped: Vec<u8> =
                raw.iter().flat_map(|&b| if b == b'"' { b"&quot;".to_vec() } else { vec![b] }).collect();
            rebuilt.push_attribute((key.as_bytes(), escaped.as_slice()));
        } else {
            rebuilt.push_attribute((key.as_bytes(), raw));
        }
    }
    changed.then_some(rebuilt)
}

/// A deleted element's `name` attribute, if it has one, and failing that its
/// other attribute values (a template's path and title), for the report.
/// Namespace declarations are neither.
fn attribute_text(element: &BytesStart) -> (Option<String>, Option<String>) {
    let mut name = None;
    let mut values = Vec::new();
    for attribute in element.attributes().flatten() {
        let key = qname(attribute.key.as_ref());
        if key == "xmlns" || key.starts_with("xmlns:") {
            continue;
        }
        let value = attribute
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .map(|v| v.trim().to_string())
            .unwrap_or_default();
        if value.is_empty() {
            continue;
        }
        if key.rsplit(':').next() == Some("name") {
            name = Some(value);
        } else {
            values.push(value);
        }
    }
    (name, (!values.is_empty()).then(|| values.join(", ")))
}

fn matches(name: &str, targets: &[&str]) -> bool {
    let local = name.rsplit(':').next().unwrap_or(name);
    targets
        .iter()
        .any(|t| t.eq_ignore_ascii_case(name) || t.rsplit(':').next().unwrap_or(t).eq_ignore_ascii_case(local))
}

fn qname(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw).into_owned()
}

fn write_err(e: std::io::Error) -> String {
    format!("could not write XML: {e}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empties_the_element_but_keeps_it() {
        let xml = br#"<?xml version="1.0"?><cp:coreProperties xmlns:cp="x" xmlns:dc="y"><dc:title>Report</dc:title><dc:creator>Jane Doe</dc:creator><cp:revision>17</cp:revision></cp:coreProperties>"#;
        let out = blank_elements(xml, &["dc:creator", "cp:revision"]).unwrap();
        let s = String::from_utf8(out.xml).unwrap();

        assert!(s.contains("<dc:creator></dc:creator>"));
        assert!(!s.contains("Jane Doe"));
        assert!(!s.contains(">17<"));
        // Untargeted content survives verbatim.
        assert!(s.contains("<dc:title>Report</dc:title>"));
        assert_eq!(
            out.removed,
            vec![
                ("dc:creator".to_string(), "Jane Doe".to_string()),
                ("cp:revision".to_string(), "17".to_string())
            ]
        );
    }

    #[test]
    fn matches_on_local_name_when_the_prefix_differs() {
        let xml = br#"<r><ns9:creator xmlns:ns9="y">Jane</ns9:creator></r>"#;
        let out = blank_elements(xml, &["dc:creator"]).unwrap();
        assert!(!String::from_utf8(out.xml).unwrap().contains("Jane"));
    }

    #[test]
    fn removes_nested_markup_inside_a_target() {
        let xml = br#"<r><custom><prop name="a">1</prop><prop name="b">2</prop></custom></r>"#;
        let out = blank_elements(xml, &["custom"]).unwrap();
        let s = String::from_utf8(out.xml).unwrap();
        assert_eq!(s, "<r><custom></custom></r>");
    }

    #[test]
    fn deleted_elements_take_their_attributes_with_them() {
        let xml = br#"<m xmlns:meta="m" xmlns:xlink="x"><meta:template xlink:href="file:///home/jane/t.ott" xlink:title="Letter"/><meta:user-defined meta:name="Client">ACME</meta:user-defined><keep/></m>"#;
        let out = strip_elements(xml, &[], &["meta:template", "meta:user-defined"]).unwrap();
        let s = String::from_utf8(out.xml).unwrap();
        assert_eq!(s, r#"<m xmlns:meta="m" xmlns:xlink="x"><keep/></m>"#);
        assert_eq!(
            out.removed,
            vec![
                ("meta:template".to_string(), "file:///home/jane/t.ott, Letter".to_string()),
                ("meta:user-defined".to_string(), "Client: ACME".to_string()),
            ]
        );
    }

    #[test]
    fn self_closing_targets_and_absent_targets_are_left_alone() {
        let xml = br#"<r><dc:creator xmlns:dc="y"/><keep>x</keep></r>"#;
        let out = blank_elements(xml, &["dc:creator", "nothing"]).unwrap();
        assert!(out.removed.is_empty());
        assert!(String::from_utf8(out.xml).unwrap().contains("<keep>x</keep>"));
    }

    #[test]
    fn scrubs_attributes_and_leaves_untouched_tags_alone() {
        let xml = br#"<w:body xmlns:w="w"><w:ins w:id="1" w:author="Jane &amp; Co" w:date="2024-01-02T10:00:00Z"><w:r a='x"y'/></w:ins><w:p w:author=""/></w:body>"#;
        let out = scrub_attributes(xml, &["w:author"], &["w:date"]).unwrap();
        let s = String::from_utf8(out.xml).unwrap();
        assert_eq!(
            s,
            r#"<w:body xmlns:w="w"><w:ins w:id="1" w:author=""><w:r a='x"y'/></w:ins><w:p w:author=""/></w:body>"#
        );
        assert_eq!(
            out.removed,
            vec![
                ("w:author".to_string(), "Jane & Co".to_string()),
                ("w:date".to_string(), "2024-01-02T10:00:00Z".to_string()),
            ]
        );
    }

    #[test]
    fn values_in_cdata_or_references_are_reported() {
        let xml = br#"<r xmlns:dc="y"><dc:creator>Smith &amp; Jones</dc:creator><dc:contributor><![CDATA[Jane]]></dc:contributor><dc:publisher>&#x5F20;&#x4E09;</dc:publisher></r>"#;
        let out = blank_elements(xml, &["dc:creator", "dc:contributor", "dc:publisher"]).unwrap();
        let values: Vec<&str> = out.removed.iter().map(|(_, v)| v.as_str()).collect();
        assert_eq!(values, ["Smith & Jones", "Jane", "张三"]);
        let s = String::from_utf8(out.xml).unwrap();
        assert!(!s.contains("Jane") && !s.contains("Smith") && !s.contains("#x5F20"), "{s}");
    }
}
