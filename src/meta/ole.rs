//! Legacy Office: .doc, .xls and .ppt.
//!
//! These predate the ZIP-and-XML packages by a decade. The file is an OLE2
//! compound file — a small filesystem inside a file — and the properties live
//! in two streams whose names begin with a 0x05 byte:
//! `\x05SummaryInformation` and `\x05DocumentSummaryInformation`.
//!
//! The streams stay; the values in them are **zeroed in place**, the way
//! `tiff.rs` does it. Removing the streams would also take the title, subject
//! and keywords, which every other document path keeps because the author
//! typed them. A zeroed string still has its length and reads as empty, and
//! the stream keeps its size, so the bytes are overwritten where they lie
//! rather than freed with the name still in them. The user-defined section of
//! `DocumentSummaryInformation` is emptied whole, as custom properties are
//! elsewhere.
//!
//! Excel and PowerPoint each keep one more name outside the property streams:
//! the `WRITEACCESS` record at the top of the workbook, and the `Current User`
//! stream. Both are blanked with spaces, length unchanged.
//!
//! **Limitation worth knowing.** A .doc file also keeps a list of the last ten
//! people who saved it (`SttbSavedBy`) inside the `WordDocument`/`Table`
//! streams, and a fast-saved document can hold text the author deleted. Both
//! sit behind the File Information Block, well past what this module parses,
//! and neither is touched. See the `kept` note the strip returns.

use std::io::{Cursor, Read, Write};

use super::{Finding, StripResult};

pub const SIGNATURE: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

const SUMMARY: &str = "/\u{5}SummaryInformation";
const DOCUMENT_SUMMARY: &str = "/\u{5}DocumentSummaryInformation";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OleKind {
    Word,
    Excel,
    PowerPoint,
    Other,
}

impl OleKind {
    pub fn describe(self) -> &'static str {
        match self {
            OleKind::Word => "Word 97-2003 document",
            OleKind::Excel => "Excel 97-2003 workbook",
            OleKind::PowerPoint => "PowerPoint 97-2003 presentation",
            OleKind::Other => "OLE compound file",
        }
    }
}

pub fn detect(data: &[u8]) -> Option<OleKind> {
    let file = cfb::CompoundFile::open(Cursor::new(data)).ok()?;
    Some(if file.exists("/WordDocument") {
        OleKind::Word
    } else if file.exists("/Workbook") || file.exists("/Book") {
        OleKind::Excel
    } else if file.exists("/PowerPoint Document") {
        OleKind::PowerPoint
    } else {
        OleKind::Other
    })
}

pub fn strip(data: &[u8], kind: OleKind) -> Result<StripResult, String> {
    let mut file = cfb::CompoundFile::open(Cursor::new(data.to_vec()))
        .map_err(|e| format!("could not read the compound file: {e}"))?;

    let mut findings = Vec::new();
    let mut kept = Vec::new();

    for (path, catalogue) in [(SUMMARY, SUMMARY_PROPERTIES), (DOCUMENT_SUMMARY, DOCUMENT_PROPERTIES)]
    {
        if !file.exists(path) {
            continue;
        }
        let raw = read_stream(&mut file, path)?;
        let (found, scrubbed) = scrub_property_set(&raw, catalogue);
        findings.extend(found);
        if scrubbed != raw {
            overwrite_stream(&mut file, path, &scrubbed)?;
        }
    }

    match kind {
        OleKind::Excel if file.exists("/Workbook") => {
            let raw = read_stream(&mut file, "/Workbook")?;
            match scrub_write_access(&raw) {
                WriteAccess::Scrubbed(name, scrubbed) => {
                    findings.push(Finding::new("User name", name));
                    overwrite_stream(&mut file, "/Workbook", &scrubbed)?;
                }
                WriteAccess::Encrypted => kept.push(Finding::new(
                    "Excel user name",
                    "the workbook is encrypted, so the name Excel records in it cannot be reached",
                )),
                WriteAccess::Clean => {}
            }
        }
        OleKind::Excel => kept.push(Finding::new(
            "Excel user name",
            "this is an Excel 5 workbook, whose user-name record is not parsed",
        )),
        OleKind::PowerPoint if file.exists(CURRENT_USER) => {
            let raw = read_stream(&mut file, CURRENT_USER)?;
            if let Some((name, scrubbed)) = scrub_current_user(&raw) {
                findings.push(Finding::new("User name", name));
                overwrite_stream(&mut file, CURRENT_USER, &scrubbed)?;
            }
        }
        OleKind::Word => kept.push(Finding::new(
            "Word revision data",
            "the list of people who last saved this file, and any text left by a fast save, \
             live deep inside the document stream and are not removed",
        )),
        _ => {}
    }

    let cleaned = file.into_inner().into_inner();
    Ok(StripResult::new(cleaned, findings).keeping(kept))
}

const CURRENT_USER: &str = "/Current User";

type Compound = cfb::CompoundFile<Cursor<Vec<u8>>>;

fn read_stream(file: &mut Compound, path: &str) -> Result<Vec<u8>, String> {
    let mut raw = Vec::new();
    file.open_stream(path)
        .and_then(|mut stream| stream.read_to_end(&mut raw))
        .map_err(|e| format!("could not read {path}: {e}"))?;
    Ok(raw)
}

/// Writes `bytes`, which are exactly as long as the stream, over it. Same
/// length means the same sectors: the old values are overwritten where they
/// lie, and the container — sector chains, directory, and the root storage's
/// class id that says "this is a Word document" — stays as the office suite
/// wrote it.
fn overwrite_stream(file: &mut Compound, path: &str, bytes: &[u8]) -> Result<(), String> {
    file.open_stream(path)
        .and_then(|mut stream| {
            stream.write_all(bytes)?;
            stream.flush()
        })
        .map_err(|e| format!("could not overwrite {path}: {e}"))
}

/// Reports and zeroes the catalogued properties of the first section, and
/// empties a second (user-defined) section if there is one.
fn scrub_property_set(data: &[u8], catalogue: Catalogue) -> (Vec<Finding>, Vec<u8>) {
    let mut out = data.to_vec();
    let mut findings = read_property_set(data, catalogue);

    if let Some(section) = u32_at(data, 44).map(|s| s as usize) {
        for (id, at) in properties(data, section) {
            if catalogue.iter().any(|(pid, _)| *pid == id) {
                if let Some((start, len)) = value_extent(data, at) {
                    out[start..start + len].fill(0);
                }
            }
        }
    }

    // User-defined properties: a second section, listed at offset 64.
    let sections = u32_at(data, 24).unwrap_or(0);
    if sections >= 2 {
        if let Some(section) = u32_at(data, 64).map(|s| s as usize) {
            let custom = properties(data, section).filter(|(id, _)| *id > 1).count();
            let size = u32_at(data, section).unwrap_or(0) as usize;
            let end = section.saturating_add(size).min(data.len());
            if custom > 0 && section + 8 <= end {
                findings.push(Finding::new(
                    "Custom properties",
                    format!("{custom} user-defined propert{}", if custom == 1 { "y" } else { "ies" }),
                ));
                // No properties, and nothing left of the old ones.
                out[section + 4..section + 8].fill(0);
                out[section + 8..end].fill(0);
            }
        }
    }

    (findings, out)
}

/// `(property id, absolute offset of its value)` for each entry in a section.
fn properties(data: &[u8], section: usize) -> impl Iterator<Item = (u32, usize)> + '_ {
    let count = u32_at(data, section + 4).unwrap_or(0).min(256);
    (0..count).map_while(move |i| {
        let entry = section + 8 + i as usize * 8;
        let id = u32_at(data, entry)?;
        let offset = u32_at(data, entry + 4)?;
        Some((id, section + offset as usize))
    })
}

/// The bytes holding a value, not counting its type or length fields, so that
/// zeroing them leaves an empty value of the same shape.
fn value_extent(data: &[u8], at: usize) -> Option<(usize, usize)> {
    let body = at + 4;
    let (start, len) = match u32_at(data, at)? {
        2 => (body, 2),
        3 => (body, 4),
        30 => (body + 4, u32_at(data, body)? as usize),
        31 => (body + 4, u32_at(data, body)? as usize * 2),
        64 => (body, 8),
        _ => return None,
    };
    (start.checked_add(len)? <= data.len()).then_some((start, len))
}

enum WriteAccess {
    Scrubbed(String, Vec<u8>),
    Encrypted,
    Clean,
}

/// Excel writes the user name from its options into a `WRITEACCESS` record in
/// the workbook globals, whoever the document's author is.
fn scrub_write_access(data: &[u8]) -> WriteAccess {
    const EOF: u16 = 0x000A;
    const FILEPASS: u16 = 0x002F;
    const WRITEACCESS: u16 = 0x005C;

    let mut pos = 0;
    while let (Some(kind), Some(len)) = (u16_at(data, pos), u16_at(data, pos + 2)) {
        let body = pos + 4;
        let end = body + len as usize;
        match kind {
            // Record bodies after this are encrypted.
            FILEPASS => return WriteAccess::Encrypted,
            EOF => break,
            WRITEACCESS if end <= data.len() => {
                // XLUnicodeString: character count, flags, characters.
                let Some(count) = u16_at(data, body).map(|c| c as usize) else { break };
                let wide = data.get(body + 2).is_some_and(|f| f & 1 != 0);
                let start = body + 3;
                let bytes = if wide { count * 2 } else { count };
                let Some(chars) = data.get(start..(start + bytes).min(end)) else { break };
                let name = if wide {
                    let units: Vec<u16> =
                        chars.as_chunks::<2>().0.iter().map(|p| u16::from_le_bytes(*p)).collect();
                    String::from_utf16_lossy(&units)
                } else {
                    chars.iter().map(|&b| b as char).collect()
                };
                let name = name.trim_matches(|c: char| c.is_whitespace() || c == '\0').to_string();
                if name.is_empty() {
                    return WriteAccess::Clean;
                }
                let mut out = data.to_vec();
                blank_text(&mut out[start..start + chars.len()], wide);
                return WriteAccess::Scrubbed(name, out);
            }
            _ => {}
        }
        pos = end;
    }
    WriteAccess::Clean
}

/// PowerPoint's `Current User` stream names whoever last had the file open,
/// once in ANSI and once in UTF-16.
fn scrub_current_user(data: &[u8]) -> Option<(String, Vec<u8>)> {
    const CURRENT_USER_ATOM: u16 = 0x0FF6;
    if u16_at(data, 2)? != CURRENT_USER_ATOM {
        return None;
    }
    let len = u16_at(data, 20)? as usize;
    let ansi = 28..28 + len;
    let name: String = data.get(ansi.clone())?.iter().map(|&b| b as char).collect();
    let name = name.trim_matches(|c: char| c.is_whitespace() || c == '\0').to_string();
    if name.is_empty() {
        return None;
    }
    let mut out = data.to_vec();
    blank_text(&mut out[ansi.clone()], false);
    // After the ANSI name: a four-byte version, then the same name in UTF-16.
    let wide = ansi.end + 4..ansi.end + 4 + len * 2;
    if wide.end <= out.len() {
        blank_text(&mut out[wide], true);
    }
    Some((name, out))
}

/// Spaces rather than zeroes: both formats pad these names with spaces, and a
/// reader that shows the name shows nothing.
fn blank_text(bytes: &mut [u8], wide: bool) {
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = if wide && i % 2 == 1 { 0 } else { b' ' };
    }
}

fn u16_at(data: &[u8], at: usize) -> Option<u16> {
    let b = data.get(at..at + 2)?;
    Some(u16::from_le_bytes([b[0], b[1]]))
}

/// Property id to the name it is shown under. `None` means "leave it alone" —
/// these are the fields the office-document path also keeps, because the
/// author typed them on purpose.
type Catalogue = &'static [(u32, &'static str)];

const SUMMARY_PROPERTIES: Catalogue = &[
    (4, "Author"),
    (7, "Template"),
    (8, "Last saved by"),
    (9, "Times saved"),
    (10, "Total editing time"),
    (11, "Last printed"),
    (12, "Created"),
    (13, "Last modified"),
    (18, "Software"),
];

const DOCUMENT_PROPERTIES: Catalogue = &[(14, "Manager"), (15, "Company")];

fn read_property_set(data: &[u8], catalogue: Catalogue) -> Vec<Finding> {
    let mut findings = Vec::new();
    let Some(section) = u32_at(data, 44) else { return findings };
    let section = section as usize;

    let Some(count) = u32_at(data, section + 4) else { return findings };

    for i in 0..count.min(256) {
        let entry = section + 8 + i as usize * 8;
        let (Some(id), Some(offset)) = (u32_at(data, entry), u32_at(data, entry + 4)) else {
            break;
        };
        let Some(&(_, label)) = catalogue.iter().find(|(pid, _)| *pid == id) else { continue };
        let at = section + offset as usize;
        if let Some(value) = read_value(data, at, id) {
            findings.push(Finding::new(label, value));
        }
    }

    findings
}

fn read_value(data: &[u8], at: usize, id: u32) -> Option<String> {
    let kind = u32_at(data, at)?;
    let body = at + 4;

    let text = match kind {
        // VT_I2 / VT_I4: the save counter.
        2 => u16::from_le_bytes([*data.get(body)?, *data.get(body + 1)?]).to_string(),
        3 => u32_at(data, body)?.to_string(),
        // VT_LPSTR: a byte string in the file's code page.
        30 => {
            let len = u32_at(data, body)? as usize;
            let bytes = data.get(body + 4..body + 4 + len)?;
            bytes.iter().take_while(|&&b| b != 0).map(|&b| b as char).collect()
        }
        // VT_LPWSTR: UTF-16LE.
        31 => {
            let len = u32_at(data, body)? as usize;
            let bytes = data.get(body + 4..body + 4 + len * 2)?;
            let units: Vec<u16> = bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_le_bytes(*pair))
                .take_while(|&u| u != 0)
                .collect();
            String::from_utf16_lossy(&units)
        }
        // VT_FILETIME: a moment, except for property 10, which is a duration.
        64 => {
            let low = u32_at(data, body)? as u64;
            let high = u32_at(data, body + 4)? as u64;
            let ticks = (high << 32) | low;
            if id == 10 {
                let minutes = ticks / 10_000_000 / 60;
                if minutes == 0 {
                    return None;
                }
                format!("{minutes} minutes")
            } else {
                filetime_to_date(ticks)?
            }
        }
        _ => return None,
    };

    let text = text.trim().to_string();
    (!text.is_empty() && text != "0").then_some(text)
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    let b = data.get(at..at + 4)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Windows FILETIME is 100-nanosecond ticks since 1601. Converted by hand so
/// the app does not carry a date library for the sake of four labels.
fn filetime_to_date(ticks: u64) -> Option<String> {
    if ticks == 0 {
        return None;
    }
    let unix = (ticks / 10_000_000) as i64 - 11_644_473_600;
    if unix < 0 {
        return None;
    }
    let (days, seconds) = (unix.div_euclid(86_400), unix.rem_euclid(86_400));
    let (year, month, day) = civil_from_days(days);
    Some(format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        seconds / 3600,
        (seconds % 3600) / 60
    ))
}

/// Howard Hinnant's days-to-civil conversion.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = (z - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year =
        day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A SummaryInformation stream holding a title, an author and a save count.
    fn summary_stream() -> Vec<u8> {
        let properties: [(u32, u32, Vec<u8>); 3] = [
            (2, 30, {
                let title = b"Budget\0\0";
                let mut v = 30u32.to_le_bytes().to_vec();
                v.extend_from_slice(&(title.len() as u32).to_le_bytes());
                v.extend_from_slice(title);
                v
            }),
            (4, 30, {
                let name = b"Jane Doe\0";
                let mut v = 30u32.to_le_bytes().to_vec();
                v.extend_from_slice(&(name.len() as u32).to_le_bytes());
                v.extend_from_slice(name);
                v
            }),
            (9, 3, {
                let mut v = 3u32.to_le_bytes().to_vec();
                v.extend_from_slice(&17u32.to_le_bytes());
                v
            }),
        ];

        let mut section = Vec::new();
        let table_len = 8 + properties.len() * 8;
        let mut table = Vec::new();
        let mut values = Vec::new();
        for (id, _, encoded) in &properties {
            table.extend_from_slice(&id.to_le_bytes());
            table.extend_from_slice(&((table_len + values.len()) as u32).to_le_bytes());
            values.extend_from_slice(encoded);
        }
        section.extend_from_slice(&((table_len + values.len()) as u32).to_le_bytes());
        section.extend_from_slice(&(properties.len() as u32).to_le_bytes());
        section.extend_from_slice(&table);
        section.extend_from_slice(&values);

        let mut stream = Vec::new();
        stream.extend_from_slice(&0xFFFEu16.to_le_bytes());
        stream.extend_from_slice(&0u16.to_le_bytes());
        stream.extend_from_slice(&0u32.to_le_bytes());
        stream.extend_from_slice(&[0u8; 16]); // CLSID
        stream.extend_from_slice(&1u32.to_le_bytes()); // one section
        stream.extend_from_slice(&[0u8; 16]); // FMTID
        stream.extend_from_slice(&48u32.to_le_bytes()); // section offset
        assert_eq!(stream.len(), 48);
        stream.extend_from_slice(&section);
        stream
    }

    fn legacy_doc() -> Vec<u8> {
        let mut file = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
        file.create_stream("/WordDocument")
            .unwrap()
            .write_all(b"Quarterly figures. Nothing confidential here.")
            .unwrap();
        file.create_stream(SUMMARY).unwrap().write_all(&summary_stream()).unwrap();
        file.into_inner().into_inner()
    }

    #[test]
    fn identifies_the_application_from_its_streams() {
        assert_eq!(detect(&legacy_doc()), Some(OleKind::Word));
        assert_eq!(detect(b"not a compound file"), None);
    }

    #[test]
    fn empties_the_properties_and_reports_what_was_in_them() {
        let result = strip(&legacy_doc(), OleKind::Word).unwrap();

        let mut file = cfb::CompoundFile::open(Cursor::new(result.data.clone())).unwrap();
        let mut summary = Vec::new();
        file.open_stream(SUMMARY).unwrap().read_to_end(&mut summary).unwrap();
        assert_eq!(summary.len(), summary_stream().len(), "the stream keeps its shape");
        // The title is content the author typed, as on every other path.
        let found = read_property_set(&summary, &[(2, "Title"), (4, "Author"), (9, "Times saved")]);
        assert_eq!(found, vec![Finding::new("Title", "Budget")]);

        let mut body = Vec::new();
        file.open_stream("/WordDocument").unwrap().read_to_end(&mut body).unwrap();
        assert_eq!(body, b"Quarterly figures. Nothing confidential here.");

        let labels: Vec<&str> = result.findings.iter().map(|f| f.label.as_str()).collect();
        assert!(labels.contains(&"Author"), "{labels:?}");
        assert!(labels.contains(&"Times saved"), "{labels:?}");
        assert_eq!(result.findings.iter().find(|f| f.label == "Author").unwrap().detail, "Jane Doe");
    }

    #[test]
    fn the_authors_name_is_gone_from_the_bytes_not_just_unlinked() {
        // Zeroed where it lies: a freed or superseded sector would keep its
        // contents, and the name would survive a raw search.
        let result = strip(&legacy_doc(), OleKind::Word).unwrap();
        assert!(
            !result.data.windows(8).any(|w| w == b"Jane Doe"),
            "the removed stream's bytes are still in the file"
        );
    }

    #[test]
    fn says_plainly_what_it_cannot_reach_in_a_word_file() {
        let result = strip(&legacy_doc(), OleKind::Word).unwrap();
        assert!(result.kept.iter().any(|f| f.label == "Word revision data"));
        // Only Word has that structure.
        assert!(strip(&legacy_doc(), OleKind::PowerPoint).unwrap().kept.is_empty());
    }

    fn compound(streams: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut file = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
        for (path, body) in streams {
            file.create_stream(path).unwrap().write_all(body).unwrap();
        }
        file.into_inner().into_inner()
    }

    fn stream(data: &[u8], path: &str) -> Vec<u8> {
        let mut file = cfb::CompoundFile::open(Cursor::new(data.to_vec())).unwrap();
        let mut body = Vec::new();
        file.open_stream(path).unwrap().read_to_end(&mut body).unwrap();
        body
    }

    #[test]
    fn empties_the_user_defined_section() {
        // Header, two section pointers, an empty first section, then a
        // user-defined one with a code page and one property.
        let mut s = vec![0xFE, 0xFF, 0, 0, 0, 0, 0, 0];
        s.extend_from_slice(&[0; 16]);
        s.extend_from_slice(&2u32.to_le_bytes());
        s.extend_from_slice(&[0; 16]);
        s.extend_from_slice(&68u32.to_le_bytes());
        s.extend_from_slice(&[0; 16]);
        s.extend_from_slice(&76u32.to_le_bytes());
        assert_eq!(s.len(), 68);
        s.extend_from_slice(&8u32.to_le_bytes());
        s.extend_from_slice(&0u32.to_le_bytes());
        let section = [
            &44u32.to_le_bytes()[..],
            &2u32.to_le_bytes(),
            &1u32.to_le_bytes(), &24u32.to_le_bytes(),
            &2u32.to_le_bytes(), &32u32.to_le_bytes(),
            &2u32.to_le_bytes(), &[0xE4, 0x04, 0, 0],
            &30u32.to_le_bytes(), &4u32.to_le_bytes(),
        ]
        .concat();
        s.extend_from_slice(&section);
        s.extend_from_slice(b"ACME");

        let (found, out) = scrub_property_set(&s, DOCUMENT_PROPERTIES);
        assert_eq!(found, vec![Finding::new("Custom properties", "1 user-defined property")]);
        assert_eq!(out.len(), s.len());
        assert!(!out.windows(4).any(|w| w == b"ACME"));
        assert!(scrub_property_set(&out, DOCUMENT_PROPERTIES).0.is_empty());
    }

    #[test]
    fn blanks_the_user_name_excel_records_in_the_workbook() {
        let record = |kind: u16, body: &[u8]| {
            let mut v = kind.to_le_bytes().to_vec();
            v.extend_from_slice(&(body.len() as u16).to_le_bytes());
            v.extend_from_slice(body);
            v
        };
        let mut access = 8u16.to_le_bytes().to_vec();
        access.push(0); // 8-bit characters
        access.extend_from_slice(b"Jane Doe");
        access.resize(112, b' ');
        let workbook = [
            record(0x0809, &[0; 16]), // BOF
            record(0x005C, &access),
            record(0x000A, &[]), // EOF
        ]
        .concat();
        let xls = compound(&[("/Workbook", workbook.clone())]);

        let result = strip(&xls, OleKind::Excel).unwrap();
        assert!(result.findings.contains(&Finding::new("User name", "Jane Doe")));
        let cleaned = stream(&result.data, "/Workbook");
        assert_eq!(cleaned.len(), workbook.len());
        assert!(!cleaned.windows(8).any(|w| w == b"Jane Doe"));
        assert!(strip(&result.data, OleKind::Excel).unwrap().findings.is_empty());
    }

    #[test]
    fn blanks_the_user_name_powerpoint_keeps_in_current_user() {
        let name = b"Jane";
        let mut atom = vec![0, 0];
        atom.extend_from_slice(&0x0FF6u16.to_le_bytes());
        atom.extend_from_slice(&0u32.to_le_bytes()); // record length, unchecked
        atom.extend_from_slice(&0x14u32.to_le_bytes());
        atom.extend_from_slice(&0xE391C05Fu32.to_le_bytes());
        atom.extend_from_slice(&0u32.to_le_bytes());
        atom.extend_from_slice(&(name.len() as u16).to_le_bytes());
        atom.extend_from_slice(&[0xF4, 0x03, 3, 0, 0, 0]);
        atom.extend_from_slice(name);
        atom.extend_from_slice(&8u32.to_le_bytes());
        for &c in name {
            atom.extend_from_slice(&[c, 0]);
        }
        let ppt = compound(&[("/PowerPoint Document", vec![0; 8]), (CURRENT_USER, atom)]);

        let result = strip(&ppt, OleKind::PowerPoint).unwrap();
        assert!(result.findings.contains(&Finding::new("User name", "Jane")));
        let cleaned = stream(&result.data, CURRENT_USER);
        assert!(!cleaned.windows(4).any(|w| w == b"Jane"));
        assert!(!cleaned.windows(8).any(|w| w == b"J\0a\0n\0e\0"));
        assert!(strip(&result.data, OleKind::PowerPoint).unwrap().findings.is_empty());
    }

    #[test]
    fn cleaning_twice_finds_nothing_the_second_time() {
        let once = strip(&legacy_doc(), OleKind::Word).unwrap();
        let twice = strip(&once.data, OleKind::Word).unwrap();
        assert!(twice.findings.is_empty(), "{:?}", twice.findings);
    }

    #[test]
    fn converts_a_filetime_to_a_readable_date() {
        // 2024-05-11 09:30:00 UTC
        let ticks = (1_715_419_800u64 + 11_644_473_600) * 10_000_000;
        assert_eq!(filetime_to_date(ticks).unwrap(), "2024-05-11 09:30");
        assert!(filetime_to_date(0).is_none());
    }
}
