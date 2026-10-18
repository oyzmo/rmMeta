//! TIFF, and the camera RAW formats built on it (CR2, NEF, ARW, DNG, ORF…).
//!
//! **Nothing moves.** A TIFF is a web of absolute file offsets: the image data
//! is found through `StripOffsets`/`TileOffsets`, sub-directories through their
//! own pointers, and a RAW file adds vendor structures that point into the file
//! by offset as well. Rebuilding the container the way the JPEG and PNG modules
//! do would mean recomputing every one of those, and getting a single one wrong
//! turns a photograph into an unopenable file.
//!
//! So the tags are excised in place. A directory entry is a fixed twelve
//! bytes: to drop one, the entries after it shift up inside the directory, the
//! entry count goes down, and the bytes the value occupied are overwritten with
//! zeroes. The file keeps its exact length and every offset in it stays valid.

use std::collections::HashSet;

use super::exif;
use super::{Finding, StripResult};

// Tags that identify a person, a place or a moment.
const REMOVE: &[u16] = &[
    0x010E, // ImageDescription
    0x0131, // Software
    0x0132, // DateTime
    0x013B, // Artist
    0x013C, // HostComputer
    0x02BC, // XMP
    0x83BB, // IPTC
    0x8298, // Copyright
    0x8649, // Photoshop image resources
    0x8825, // GPS directory pointer
    0x9003, // DateTimeOriginal
    0x9004, // DateTimeDigitized
    0x9286, // UserComment
    0x927C, // MakerNote — vendor block, routinely holds the body serial
    0x9C9B, // XPTitle
    0x9C9C, // XPComment
    0x9C9D, // XPAuthor
    0x9C9E, // XPKeywords
    0x9C9F, // XPSubject
    0xA420, // ImageUniqueID
    0xA430, // CameraOwnerName
    0xA431, // BodySerialNumber
    0xA435, // LensSerialNumber
    0xC62F, // CameraSerialNumber (DNG)
    0xC634, // DNGPrivateData — a copy of the original maker note
];

/// Tags whose removal is reported here because the Exif reader does not
/// describe them.
const SERIAL_TAGS: &[u16] = &[0xC62F, 0xC634];

/// What sits where TIFF has 42: TIFF itself, then Olympus ORF (`RO`, `RS`) and
/// Panasonic RW2 (`U`). All of them are TIFF directories from there on.
const MAGICS: &[u16] = &[42, 0x4F52, 0x5352, 0x0055];

pub fn detect(data: &[u8]) -> bool {
    let little = match data.get(..2) {
        Some(b"II") => true,
        Some(b"MM") => false,
        _ => return false,
    };
    read_u16(data, 2, little).is_some_and(|m| MAGICS.contains(&m))
}

/// Directory pointers to follow.
const SUB_DIRECTORY: &[u16] = &[
    0x014A, // SubIFDs
    0x8769, // Exif IFD
    0x8825, // GPS IFD
    0xA005, // Interoperability IFD
];

const GPS_POINTER: u16 = 0x8825;

/// The camera-private blocks: the maker note, and DNG's copy of it.
///
/// They can hold the body serial number, but a RAW converter may also need
/// them — Nikon keeps the linearisation curve of a compressed NEF there, and
/// several makers keep their white balance and colour data there — so by
/// default they are kept and said to be kept, and removing them is the user's
/// choice.
const MAKER_NOTES: &[u16] = &[0x927C, 0xC634];

pub fn strip(data: &[u8], keep_maker_notes: bool) -> Result<StripResult, String> {
    let little = match data.get(..2) {
        Some(b"II") => true,
        Some(b"MM") => false,
        _ => return Err("not a TIFF file".into()),
    };
    if !detect(data) {
        return Err("not a TIFF file".into());
    }

    // What is in the file, described before anything is touched. The test
    // `nothing_the_reader_reported_survives` holds this honest.
    let described = exif::analyse(data);
    let mut findings: Vec<Finding> = described
        .findings
        .into_iter()
        // The camera make and model are kept, so they are not "removed". The
        // second directory is not removed either: in a JPEG's Exif block it is
        // a thumbnail, but here it is a RAW file's preview or a scan's second
        // page — picture, and walked for tags like any other directory.
        .filter(|f| f.label != "Camera" && f.label != "Embedded thumbnail")
        .filter(|f| !(keep_maker_notes && f.label == "Maker note"))
        .collect();

    let mut buf = data.to_vec();
    let first = read_u32(&buf, 4, little).ok_or("malformed TIFF: no first directory")? as usize;

    let mut walker = Walker {
        little,
        spare: if keep_maker_notes { MAKER_NOTES } else { &[] },
        visited: HashSet::new(),
        removed: Vec::new(),
        spared: Vec::new(),
    };
    walker.walk(&mut buf, first);
    let Walker { removed, spared, .. } = walker;

    if removed.contains(&0x02BC) && !findings.iter().any(|f| f.label == "XMP metadata") {
        findings.push(Finding::new(
            "XMP metadata",
            "creator, editing history and ratings as XML",
        ));
    }
    if removed.iter().any(|t| SERIAL_TAGS.contains(t)) {
        findings.push(Finding::new("Serial number", "the camera's serial number, as a DNG stores it"));
    }
    if removed.contains(&0xC634) {
        findings.push(Finding::new(
            "Maker note",
            "DNG's copy of the camera's private block, which often holds its serial number",
        ));
    }
    if removed.iter().any(|t| matches!(t, 0x83BB | 0x8649)) {
        findings.push(Finding::new(
            "IPTC / Photoshop",
            "captions, credits, keywords and Photoshop's own resources",
        ));
    }
    if findings.is_empty() && !removed.is_empty() {
        findings.push(Finding::new(
            "Camera metadata",
            format!("{} identifying tags", removed.len()),
        ));
    }

    // Stated plainly rather than left as a silent gap: a RAW converter picks
    // its sensor profile from these, and a file without them can be rejected
    // outright as an unknown camera.
    let mut kept = vec![Finding::new(
        "Camera make and model",
        "kept on purpose \u{2014} RAW converters need them to decode the image correctly",
    )];
    if !spared.is_empty() {
        kept.push(Finding::new(
            "Maker notes",
            "kept because a RAW converter may need them for colour and decoding, but they can \
             hold the camera's serial number \u{2014} turn off \u{201c}Keep Maker Notes in RAW \
             Files\u{201d} in the menu to remove them",
        ));
    }

    Ok(StripResult::new(buf, findings).keeping(kept))
}

struct Walker {
    little: bool,
    /// Tags on the removal list that this pass leaves alone.
    spare: &'static [u16],
    visited: HashSet<usize>,
    removed: Vec<u16>,
    /// Spared tags that were actually found.
    spared: Vec<u16>,
}

impl Walker {
    /// Walks a directory chain, following sub-directories, removing as it goes.
    fn walk(&mut self, buf: &mut [u8], at: usize) {
        let mut at = at;
        loop {
            if at == 0 || !self.visited.insert(at) {
                return;
            }
            let Some(next) = excise(self, buf, at) else { return };
            at = next as usize;
        }
    }
}

/// Returns the offset of the next directory in the chain.
fn excise(w: &mut Walker, buf: &mut [u8], at: usize) -> Option<u32> {
    let little = w.little;
    let count = read_u16(buf, at, little)? as usize;
    let entries_at = at + 2;
    let next_at = entries_at + count * 12;
    // Reject a count that cannot fit, so a corrupt directory cannot make us
    // read — or worse, zero — arbitrary parts of the file.
    if next_at + 4 > buf.len() {
        return None;
    }
    let next = read_u32(buf, next_at, little)?;

    let mut keep: Vec<[u8; 12]> = Vec::with_capacity(count);
    let mut blank: Vec<(usize, usize)> = Vec::new();
    let mut descend: Vec<usize> = Vec::new();

    for i in 0..count {
        let entry = entries_at + i * 12;
        let (Some(tag), Some(kind), Some(items)) = (
            read_u16(buf, entry, little),
            read_u16(buf, entry + 2, little),
            read_u32(buf, entry + 4, little),
        ) else {
            return None;
        };

        let size = type_size(kind).unwrap_or(0) as u64 * items as u64;
        let value_at = (size > 4)
            .then(|| read_u32(buf, entry + 8, little))
            .flatten()
            .map(|o| o as usize);

        if SUB_DIRECTORY.contains(&tag) && matches!(kind, 4 | 13) {
            // One pointer sits in the entry itself. Several — a DNG's raw
            // image plus its previews, say — sit in an array the entry points
            // at, and that array is not a directory.
            let pointers = if items <= 1 {
                Some(entry + 8)
            } else {
                read_u32(buf, entry + 8, little).map(|o| o as usize)
            };
            if let Some(pointers) = pointers {
                for k in 0..items.min(64) as usize {
                    if let Some(target) = read_u32(buf, pointers + k * 4, little) {
                        descend.push(target as usize);
                    }
                }
            }
        }

        if w.spare.contains(&tag) {
            w.spared.push(tag);
            keep.push(buf[entry..entry + 12].try_into().ok()?);
        } else if REMOVE.contains(&tag) {
            w.removed.push(tag);
            if let Some(start) = value_at {
                if start.saturating_add(size as usize) <= buf.len() {
                    blank.push((start, size as usize));
                }
            }
        } else {
            keep.push(buf[entry..entry + 12].try_into().ok()?);
        }
    }

    // Sub-directories first: the GPS directory is itself a removal target, and
    // its contents have to be wiped before its pointer disappears.
    for target in descend {
        w.walk(buf, target);
    }
    if w.removed.contains(&GPS_POINTER) {
        // The pointer is gone from the parent, but the directory it named is
        // still sitting in the file with the coordinates in it.
        for i in 0..count {
            let entry = entries_at + i * 12;
            if read_u16(buf, entry, little) == Some(GPS_POINTER) {
                if let Some(target) = read_u32(buf, entry + 8, little) {
                    blank_directory(buf, target as usize, little, &mut blank);
                }
            }
        }
    }

    if keep.len() == count {
        return Some(next);
    }

    // Compact: the surviving entries, then the next-directory pointer, then
    // zeroes over the twelve bytes each removed entry used to occupy. The
    // directory shrinks without the file changing length.
    write_u16(buf, at, keep.len() as u16, little);
    let mut cursor = entries_at;
    for entry in &keep {
        buf[cursor..cursor + 12].copy_from_slice(entry);
        cursor += 12;
    }
    let next_bytes = if little { next.to_le_bytes() } else { next.to_be_bytes() };
    buf[cursor..cursor + 4].copy_from_slice(&next_bytes);
    cursor += 4;
    buf[cursor..next_at + 4].fill(0);

    for (start, len) in blank {
        buf[start..start + len].fill(0);
    }

    Some(next)
}

/// Queues a whole directory — its entries and their out-of-line values — for
/// zeroing.
fn blank_directory(buf: &[u8], at: usize, little: bool, blank: &mut Vec<(usize, usize)>) {
    let Some(count) = read_u16(buf, at, little).map(|c| c as usize) else { return };
    let end = at + 2 + count * 12 + 4;
    if end > buf.len() {
        return;
    }
    for i in 0..count {
        let entry = at + 2 + i * 12;
        let (Some(kind), Some(items)) =
            (read_u16(buf, entry + 2, little), read_u32(buf, entry + 4, little))
        else {
            continue;
        };
        let size = type_size(kind).unwrap_or(0) as u64 * items as u64;
        if size > 4 {
            if let Some(start) = read_u32(buf, entry + 8, little).map(|o| o as usize) {
                if start.saturating_add(size as usize) <= buf.len() {
                    blank.push((start, size as usize));
                }
            }
        }
    }
    blank.push((at, end - at));
}

fn type_size(kind: u16) -> Option<u32> {
    Some(match kind {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 | 13 => 4,
        5 | 10 | 12 => 8,
        _ => return None,
    })
}

fn read_u16(buf: &[u8], at: usize, little: bool) -> Option<u16> {
    let b = buf.get(at..at + 2)?;
    let b = [b[0], b[1]];
    Some(if little { u16::from_le_bytes(b) } else { u16::from_be_bytes(b) })
}

fn read_u32(buf: &[u8], at: usize, little: bool) -> Option<u32> {
    let b = buf.get(at..at + 4)?;
    let b = [b[0], b[1], b[2], b[3]];
    Some(if little { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) })
}

fn write_u16(buf: &mut [u8], at: usize, value: u16, little: bool) {
    let bytes = if little { value.to_le_bytes() } else { value.to_be_bytes() };
    buf[at..at + 2].copy_from_slice(&bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tests below predate the option and exercise the removal path.
    fn strip(data: &[u8]) -> Result<StripResult, String> {
        super::strip(data, false)
    }

    /// IFD0 pointing at an Exif directory that holds a maker note.
    fn with_maker_note() -> Vec<u8> {
        let exif_at = 8 + 2 + 12 + 4;
        let note_at = exif_at + 2 + 12 + 4;
        let mut t = b"II\x2a\x00".to_vec();
        t.extend_from_slice(&8u32.to_le_bytes());
        t.extend_from_slice(&1u16.to_le_bytes());
        t.extend_from_slice(&entry(0x8769, 4, 1, exif_at as u32));
        t.extend_from_slice(&0u32.to_le_bytes());
        t.extend_from_slice(&1u16.to_le_bytes());
        t.extend_from_slice(&entry(0x927C, 7, 8, note_at as u32));
        t.extend_from_slice(&0u32.to_le_bytes());
        t.extend_from_slice(b"SN-98765");
        t
    }

    #[test]
    fn the_maker_note_is_kept_and_said_to_be_kept_by_default() {
        let original = with_maker_note();
        let result = super::strip(&original, true).unwrap();
        assert_eq!(result.data, original, "nothing else in the file to remove");
        assert!(result.findings.is_empty(), "{:?}", result.findings);
        assert!(result.kept.iter().any(|f| f.label == "Maker notes"), "{:?}", result.kept);
    }

    #[test]
    fn the_maker_note_goes_when_asked() {
        let result = strip(&with_maker_note()).unwrap();
        assert!(result.findings.iter().any(|f| f.label == "Maker note"), "{:?}", result.findings);
        assert!(!result.data.windows(8).any(|w| w == b"SN-98765"));
        assert!(result.kept.iter().all(|f| f.label != "Maker notes"));
    }

    #[test]
    fn keeps_the_file_exactly_the_same_length() {
        let original = fixture();
        let result = strip(&original).unwrap();
        assert_eq!(
            result.data.len(),
            original.len(),
            "excision must not move a single byte"
        );
    }

    #[test]
    fn nothing_the_reader_reported_survives() {
        // Whatever the Exif reader could describe in the original must be gone
        // afterwards, apart from the make and model that are kept on purpose.
        let original = fixture();
        let before = exif::analyse(&original);
        assert!(before.findings.len() > 1, "the fixture has to carry something");

        let result = strip(&original).unwrap();
        let after = exif::analyse(&result.data);
        let leftovers: Vec<&str> =
            after.findings.iter().map(|f| f.label.as_str()).filter(|l| *l != "Camera").collect();
        assert!(leftovers.is_empty(), "still readable: {leftovers:?}");
    }

    #[test]
    fn the_camera_model_is_kept_and_said_to_be_kept() {
        let result = strip(&fixture()).unwrap();
        let after = exif::analyse(&result.data);
        assert!(
            after.findings.iter().any(|f| f.label == "Camera"),
            "a RAW converter needs the model"
        );
        assert!(result.kept.iter().any(|f| f.label == "Camera make and model"));
    }

    #[test]
    fn the_image_data_is_untouched() {
        let original = fixture();
        let result = strip(&original).unwrap();
        // The pixels live at the end of the fixture, past every directory.
        let pixels = original.len() - PIXELS.len();
        assert_eq!(&result.data[pixels..], PIXELS);
    }

    #[test]
    fn cleaning_twice_changes_nothing_further() {
        let once = strip(&fixture()).unwrap();
        let twice = strip(&once.data).unwrap();
        assert_eq!(once.data, twice.data);
    }

    #[test]
    fn rejects_non_tiff_and_survives_truncation() {
        assert!(strip(b"not a tiff").is_err());
        assert!(strip(b"II\x2b\x00").is_err(), "BigTIFF is a different format");
        let full = fixture();
        for n in 0..full.len() {
            let _ = strip(&full[..n]); // must not panic
        }
    }

    fn entry(tag: u16, kind: u16, count: u32, value: u32) -> Vec<u8> {
        let mut v = tag.to_le_bytes().to_vec();
        v.extend_from_slice(&kind.to_le_bytes());
        v.extend_from_slice(&count.to_le_bytes());
        v.extend_from_slice(&value.to_le_bytes());
        v
    }

    /// IFD0 with two SubIFDs, whose pointers therefore live in an array
    /// rather than in the entry. Each sub-directory names an artist inline.
    fn with_two_sub_directories() -> (Vec<u8>, std::ops::Range<usize>) {
        let array_at = 8 + 2 + 12 + 4;
        let sub1 = array_at + 8;
        let sub2 = sub1 + 2 + 12 + 4;
        let mut out = b"II\x2a\x00".to_vec();
        out.extend_from_slice(&8u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&entry(0x014A, 4, 2, array_at as u32));
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(sub1 as u32).to_le_bytes());
        out.extend_from_slice(&(sub2 as u32).to_le_bytes());
        for _ in 0..2 {
            out.extend_from_slice(&1u16.to_le_bytes());
            out.extend_from_slice(&entry(0x013B, 2, 4, u32::from_le_bytes(*b"Jan\0")));
            out.extend_from_slice(&0u32.to_le_bytes());
        }
        (out, array_at..array_at + 8)
    }

    #[test]
    fn follows_every_sub_directory_and_leaves_the_pointer_array_alone() {
        let (original, array) = with_two_sub_directories();
        let result = strip(&original).unwrap();
        assert_eq!(result.data[array.clone()], original[array], "the pointer array was rewritten");
        assert!(!result.data.windows(3).any(|w| w == b"Jan"), "a sub-directory was skipped");
    }

    #[test]
    fn a_second_directory_is_picture_not_a_finding() {
        // IFD0 with an artist, chained to a second page.
        let ifd1 = 8 + 2 + 24 + 4;
        let mut t = b"II\x2a\x00".to_vec();
        t.extend_from_slice(&8u32.to_le_bytes());
        t.extend_from_slice(&2u16.to_le_bytes());
        t.extend_from_slice(&entry(0x0100, 4, 1, 1));
        t.extend_from_slice(&entry(0x013B, 2, 4, u32::from_le_bytes(*b"Jan\0")));
        t.extend_from_slice(&(ifd1 as u32).to_le_bytes());
        t.extend_from_slice(&1u16.to_le_bytes());
        t.extend_from_slice(&entry(0x0100, 4, 1, 1));
        t.extend_from_slice(&0u32.to_le_bytes());

        let once = strip(&t).unwrap();
        assert!(once.findings.iter().all(|f| f.label != "Embedded thumbnail"));
        let twice = strip(&once.data).unwrap();
        assert!(twice.findings.is_empty(), "{:?}", twice.findings);
    }

    #[test]
    fn the_dng_serial_number_goes() {
        let mut t = b"II\x2a\x00".to_vec();
        t.extend_from_slice(&8u32.to_le_bytes());
        t.extend_from_slice(&1u16.to_le_bytes());
        t.extend_from_slice(&entry(0xC62F, 2, 4, u32::from_le_bytes(*b"123\0")));
        t.extend_from_slice(&0u32.to_le_bytes());
        let result = strip(&t).unwrap();
        assert!(result.findings.iter().any(|f| f.label == "Serial number"));
        assert!(!result.data.windows(3).any(|w| w == b"123"));
    }

    const PIXELS: &[u8] = b"\x7f\xa0\x7a\x7f\xa0\x7a";

    /// Little-endian TIFF: IFD0 with make, model, artist, a GPS pointer and an
    /// Exif pointer; a GPS directory with coordinates; an Exif directory with
    /// a body serial number; then the pixels.
    fn fixture() -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(b"II");
        out.extend_from_slice(&42u16.to_le_bytes());
        out.extend_from_slice(&8u32.to_le_bytes());

        // Reserve space, fill offsets in once the layout is known.
        let ifd0_at = 8usize;
        let ifd0_entries = 5usize;
        let ifd0_len = 2 + ifd0_entries * 12 + 4;
        let gps_at = ifd0_at + ifd0_len;
        let gps_entries = 4usize;
        let gps_len = 2 + gps_entries * 12 + 4;
        let exif_at = gps_at + gps_len;
        let exif_entries = 1usize;
        let exif_len = 2 + exif_entries * 12 + 4;
        let values_at = exif_at + exif_len;

        let mut values: Vec<u8> = Vec::new();
        let place = |values: &mut Vec<u8>, bytes: &[u8]| -> u32 {
            let at = values_at + values.len();
            values.extend_from_slice(bytes);
            at as u32
        };

        let make = place(&mut values, b"Canon\0");
        let model = place(&mut values, b"EOS R6\0");
        let artist = place(&mut values, b"Jane Doe\0");
        let serial = place(&mut values, b"SN-12345678\0");
        let rational = |values: &mut Vec<u8>, triple: [(u32, u32); 3]| -> u32 {
            let at = values_at + values.len();
            for (n, d) in triple {
                values.extend_from_slice(&n.to_le_bytes());
                values.extend_from_slice(&d.to_le_bytes());
            }
            at as u32
        };
        let latitude = rational(&mut values, [(59, 1), (54, 1), (50, 1)]);
        let longitude = rational(&mut values, [(10, 1), (45, 1), (9, 1)]);

        let entry = |tag: u16, kind: u16, count: u32, value: u32| -> Vec<u8> {
            let mut v = tag.to_le_bytes().to_vec();
            v.extend_from_slice(&kind.to_le_bytes());
            v.extend_from_slice(&count.to_le_bytes());
            v.extend_from_slice(&value.to_le_bytes());
            v
        };

        // IFD0 — entries must be in ascending tag order.
        out.extend_from_slice(&(ifd0_entries as u16).to_le_bytes());
        out.extend_from_slice(&entry(0x010F, 2, 6, make));
        out.extend_from_slice(&entry(0x0110, 2, 7, model));
        out.extend_from_slice(&entry(0x013B, 2, 9, artist));
        out.extend_from_slice(&entry(0x8769, 4, 1, exif_at as u32));
        out.extend_from_slice(&entry(0x8825, 4, 1, gps_at as u32));
        out.extend_from_slice(&0u32.to_le_bytes());

        // GPS directory.
        out.extend_from_slice(&(gps_entries as u16).to_le_bytes());
        out.extend_from_slice(&entry(1, 2, 2, u32::from_le_bytes(*b"N\0\0\0")));
        out.extend_from_slice(&entry(2, 5, 3, latitude));
        out.extend_from_slice(&entry(3, 2, 2, u32::from_le_bytes(*b"E\0\0\0")));
        out.extend_from_slice(&entry(4, 5, 3, longitude));
        out.extend_from_slice(&0u32.to_le_bytes());

        // Exif directory.
        out.extend_from_slice(&(exif_entries as u16).to_le_bytes());
        out.extend_from_slice(&entry(0xA431, 2, 12, serial));
        out.extend_from_slice(&0u32.to_le_bytes());

        assert_eq!(out.len(), values_at);
        out.extend_from_slice(&values);
        out.extend_from_slice(PIXELS);
        out
    }
}
