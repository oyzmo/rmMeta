//! HEIC, HEIF and AVIF — one container, three names.
//!
//! The picture and its metadata are both stored as *items*. `iinf` says what
//! each item is, `iloc` says where in the file its bytes are, and the bytes
//! themselves sit in `mdat` alongside the image data.
//!
//! Those `iloc` offsets are absolute positions in the file. Removing an Exif
//! item properly would mean deleting its entry from `iinf`, its entry from
//! `iloc`, its associations from `iref` and `iprp`, cutting the bytes out of
//! `mdat`, and then correcting every remaining offset — including the ones
//! pointing at the picture. One mistake there and the photograph is gone.
//!
//! Instead the metadata payload is **overwritten with zeroes where it lies**.
//! Not a byte moves, every offset stays true, and the data is destroyed rather
//! than merely unlinked. The item declaration survives as an empty husk, which
//! is why `strip` reports that in `kept`: the file will still say it has an
//! Exif item, and that item will contain nothing.

use super::exif;
use super::{Finding, StripResult};

/// Brands that mean "this is a HEIF-family image".
const BRANDS: [&[u8; 4]; 12] = [
    b"heic", b"heix", b"heim", b"heis", b"hevc", b"hevm", b"hevs", b"mif1", b"msf1", b"avif",
    b"avis", b"mia1",
];

pub fn detect(data: &[u8]) -> bool {
    if data.get(4..8) != Some(b"ftyp") {
        return false;
    }
    let Some(size) = read_u32(data, 0) else { return false };
    let end = (size as usize).clamp(8, data.len());
    // The major brand, then the compatible-brands list that follows it.
    data.get(8..12).is_some_and(|b| BRANDS.iter().any(|k| *k == b))
        || data
            .get(16..end)
            .is_some_and(|rest| rest.as_chunks::<4>().0.iter().any(|b| BRANDS.contains(&b)))
}

pub fn strip(data: &[u8]) -> Result<StripResult, String> {
    let meta = find_box(data, 0, data.len(), b"meta")
        .ok_or("this HEIF file has no metadata box to clean")?;
    // `meta` is a full box: a version byte and three flag bytes come first.
    let children = (meta.0 + 4).min(meta.1);

    let items = read_item_info(data, children, meta.1);
    // Small items may be stored inside `meta` itself, in an `idat` box, rather
    // than in `mdat`. Those extents are legitimately inside the structure.
    let idat = find_box(data, children, meta.1, b"idat");
    let locations = read_item_locations(data, children, meta.1, idat);

    let mut buf = data.to_vec();
    let mut findings = Vec::new();
    let mut wiped = 0usize;

    // An item's payload lives in a data box, never inside the structures that
    // describe it. Anything claiming otherwise is a malformed — or hostile —
    // file, and following it would mean zeroing the header or the picture.
    // This is not hypothetical: a fixture with a mis-written offset had this
    // code blank the first 378 bytes of a real image.
    let forbidden = [(0usize, header_end(data)), (meta.0.saturating_sub(8), meta.1)];
    let overlaps_structure = |start: usize, end: usize| {
        let in_idat = idat.is_some_and(|(from, to)| from <= start && end <= to);
        !in_idat && forbidden.iter().any(|&(from, to)| start < to && from < end)
    };

    for item in &items {
        let Some(extents) = locations.get(&item.id) else { continue };
        let mut wiped_this_item = false;

        for &(start, length) in extents {
            let Some(end) = start.checked_add(length).filter(|e| *e <= buf.len()) else { continue };
            if overlaps_structure(start, end) {
                return Err(
                    "this HEIF file points its metadata at the wrong part of itself; \
                     rmMeta will not risk rewriting it"
                        .into(),
                );
            }
            // An item this tool has already emptied is a husk, not metadata.
            // Without this check a cleaned file reports its own empty slots
            // as findings for ever.
            if buf[start..end].iter().all(|&b| b == 0) {
                continue;
            }
            if item.is_exif {
                describe_exif(&buf[start..end], &mut findings);
            }
            buf[start..end].fill(0);
            wiped += length;
            wiped_this_item = true;
        }

        if wiped_this_item && !item.is_exif {
            findings.push(Finding::new(
                "XMP metadata",
                "creator, editing history and ratings as XML",
            ));
        }
    }

    if wiped == 0 {
        return Ok(StripResult::new(buf, findings));
    }

    let kept = vec![Finding::new(
        "Empty metadata slots",
        "the file still declares its Exif and XMP items, but they now hold nothing \u{2014} \
         removing the declarations would mean moving the image data",
    )];

    Ok(StripResult::new(buf, findings).keeping(kept))
}

/// The Exif item payload begins with an offset to the TIFF header rather than
/// the header itself.
fn describe_exif(payload: &[u8], findings: &mut Vec<Finding>) {
    let tiff = payload
        .windows(4)
        .position(|w| w == b"II\x2a\x00" || w == b"MM\x00\x2a")
        .map(|at| &payload[at..])
        .unwrap_or(payload);

    let info = exif::analyse(tiff);
    if info.findings.is_empty() {
        findings.push(Finding::new("Exif", "camera and capture data"));
    } else {
        findings.extend(info.findings);
    }
}

struct Item {
    id: u32,
    is_exif: bool,
}

/// `iinf` lists the items; each `infe` inside names one.
fn read_item_info(data: &[u8], from: usize, to: usize) -> Vec<Item> {
    let Some((start, end)) = find_box(data, from, to, b"iinf") else { return Vec::new() };
    let version = *data.get(start).unwrap_or(&0);
    // Entry count is 16 bits in version 0 and 32 in later ones.
    let entries_at = start + 4 + if version == 0 { 2 } else { 4 };

    let mut items = Vec::new();
    let mut cursor = entries_at;
    while let Some((body, body_end)) = next_box(data, cursor, end) {
        if data.get(cursor + 4..cursor + 8) == Some(b"infe") {
            if let Some(item) = read_item_entry(data, body, body_end) {
                items.push(item);
            }
        }
        cursor = body_end;
    }
    items
}

fn read_item_entry(data: &[u8], start: usize, end: usize) -> Option<Item> {
    let version = *data.get(start)?;
    let mut at = start + 4;

    let id = if version >= 3 {
        let v = read_u32(data, at)?;
        at += 4;
        v
    } else {
        let v = read_u16(data, at)? as u32;
        at += 2;
        v
    };
    at += 2; // protection index
    let kind = data.get(at..at + 4)?;

    if kind == b"Exif" {
        return Some(Item { id, is_exif: true });
    }
    if kind == b"mime" {
        // For a mime item the content type follows the name, and only the RDF
        // one is XMP; the others are things like a thumbnail's description.
        let rest = data.get(at + 4..end)?;
        let text = String::from_utf8_lossy(rest).to_ascii_lowercase();
        if text.contains("rdf+xml") || text.contains("xmp") {
            return Some(Item { id, is_exif: false });
        }
    }
    None
}

/// `iloc` maps an item id to the byte ranges holding its data, returned as
/// absolute file positions whichever way the file expresses them.
fn read_item_locations(
    data: &[u8],
    from: usize,
    to: usize,
    idat: Option<(usize, usize)>,
) -> std::collections::HashMap<u32, Vec<(usize, usize)>> {
    let mut map = std::collections::HashMap::new();
    let Some((start, end)) = find_box(data, from, to, b"iloc") else { return map };

    let version = match data.get(start) {
        Some(v) => *v,
        None => return map,
    };
    let mut at = start + 4;

    let Some(sizes) = data.get(at) else { return map };
    let offset_size = (sizes >> 4) as usize;
    let length_size = (sizes & 0xF) as usize;
    let Some(more) = data.get(at + 1) else { return map };
    let base_size = (more >> 4) as usize;
    let index_size = if version >= 1 { (more & 0xF) as usize } else { 0 };
    at += 2;

    let count = if version < 2 {
        let Some(c) = read_u16(data, at) else { return map };
        at += 2;
        c as u32
    } else {
        let Some(c) = read_u32(data, at) else { return map };
        at += 4;
        c
    };

    for _ in 0..count {
        let id = if version < 2 {
            let Some(v) = read_u16(data, at) else { return map };
            at += 2;
            v as u32
        } else {
            let Some(v) = read_u32(data, at) else { return map };
            at += 4;
            v
        };

        let construction = if version >= 1 {
            let Some(v) = read_u16(data, at) else { return map };
            at += 2;
            v & 0xF
        } else {
            0
        };

        at += 2; // data reference index
        let Some(base) = read_sized(data, at, base_size) else { return map };
        at += base_size;
        let Some(extent_count) = read_u16(data, at) else { return map };
        at += 2;

        let mut extents = Vec::new();
        for _ in 0..extent_count {
            at += index_size;
            let (Some(offset), Some(length)) = (
                read_sized(data, at, offset_size),
                read_sized(data, at + offset_size, length_size),
            ) else {
                return map;
            };
            at += offset_size + length_size;
            // Method 0 is a file offset; method 1 is an offset into `idat`'s
            // payload. Method 2 points into another item, whose own extents
            // are wiped where they lie.
            let Some(relative) = base.checked_add(offset).map(|o| o as usize) else { return map };
            let absolute = match (construction, idat) {
                (0, _) => Some(relative),
                (1, Some((payload, end))) => {
                    payload.checked_add(relative).filter(|s| s.saturating_add(length as usize) <= end)
                }
                _ => None,
            };
            if let Some(start) = absolute.filter(|_| length > 0) {
                extents.push((start, length as usize));
            }
        }
        if at > end {
            return map;
        }
        if !extents.is_empty() {
            map.insert(id, extents);
        }
    }

    map
}

/// Returns the payload range of the first box of this type.
fn find_box(data: &[u8], from: usize, to: usize, kind: &[u8; 4]) -> Option<(usize, usize)> {
    let mut cursor = from;
    while let Some((body, end)) = next_box(data, cursor, to) {
        if data.get(cursor + 4..cursor + 8) == Some(kind.as_slice()) {
            return Some((body, end));
        }
        cursor = end;
    }
    None
}

/// `(payload start, box end)` for the box at `cursor`.
fn next_box(data: &[u8], cursor: usize, limit: usize) -> Option<(usize, usize)> {
    if cursor + 8 > limit || cursor + 8 > data.len() {
        return None;
    }
    let size = read_u32(data, cursor)? as usize;
    let (body, end) = match size {
        // 0 means "runs to the end of the file".
        0 => (cursor + 8, limit),
        1 => {
            let large = read_u64(data, cursor + 8)? as usize;
            (cursor + 16, cursor.checked_add(large)?)
        }
        n if n < 8 => return None,
        n => (cursor + 8, cursor.checked_add(n)?),
    };
    (body <= end && end <= limit).then_some((body, end))
}

/// End of the `ftyp` box, i.e. the last byte that is unquestionably header.
fn header_end(data: &[u8]) -> usize {
    read_u32(data, 0).map(|n| n as usize).filter(|n| *n >= 8).unwrap_or(8).min(data.len())
}

fn read_u16(data: &[u8], at: usize) -> Option<u16> {
    let b = data.get(at..at + 2)?;
    Some(u16::from_be_bytes([b[0], b[1]]))
}

fn read_u32(data: &[u8], at: usize) -> Option<u32> {
    let b = data.get(at..at + 4)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn read_u64(data: &[u8], at: usize) -> Option<u64> {
    let b = data.get(at..at + 8)?;
    Some(u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
}

/// A big-endian integer of 0, 4 or 8 bytes, as `iloc` fields are sized.
fn read_sized(data: &[u8], at: usize, size: usize) -> Option<u64> {
    match size {
        0 => Some(0),
        4 => read_u32(data, at).map(|v| v as u64),
        8 => read_u64(data, at),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(payload);
        v
    }

    fn exif_payload() -> Vec<u8> {
        let mut v = 0u32.to_be_bytes().to_vec(); // TIFF header offset
        v.extend_from_slice(b"II");
        v.extend_from_slice(&42u16.to_le_bytes());
        v.extend_from_slice(&8u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&0x010Fu16.to_le_bytes()); // Make
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(&6u32.to_le_bytes());
        v.extend_from_slice(&26u32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(b"Canon\0");
        v
    }

    /// A HEIF file with one Exif item pointing into mdat.
    fn heif() -> Vec<u8> {
        let payload = exif_payload();

        let infe = {
            let mut v = vec![2, 0, 0, 0]; // version 2
            v.extend_from_slice(&1u16.to_be_bytes()); // item id
            v.extend_from_slice(&0u16.to_be_bytes()); // protection
            v.extend_from_slice(b"Exif");
            v.push(0); // item name
            boxed(b"infe", &v)
        };
        let mut iinf_payload = vec![0, 0, 0, 0];
        iinf_payload.extend_from_slice(&1u16.to_be_bytes());
        iinf_payload.extend_from_slice(&infe);
        let iinf = boxed(b"iinf", &iinf_payload);

        // iloc version 1, 4-byte offsets and lengths, no base offset.
        let mut iloc_payload = vec![1, 0, 0, 0];
        iloc_payload.push(0x44); // offset_size 4, length_size 4
        iloc_payload.push(0x00); // base_offset_size 0, index_size 0
        iloc_payload.extend_from_slice(&1u16.to_be_bytes()); // item count
        iloc_payload.extend_from_slice(&1u16.to_be_bytes()); // item id
        iloc_payload.extend_from_slice(&0u16.to_be_bytes()); // construction 0
        iloc_payload.extend_from_slice(&0u16.to_be_bytes()); // data ref
        iloc_payload.extend_from_slice(&1u16.to_be_bytes()); // extent count
        let offset_at = iloc_payload.len();
        iloc_payload.extend_from_slice(&0u32.to_be_bytes()); // patched below
        iloc_payload.extend_from_slice(&(payload.len() as u32).to_be_bytes());

        let ftyp = boxed(b"ftyp", b"heic\0\0\0\0mif1heic");
        let meta_prefix_len = {
            let mut meta_payload = vec![0, 0, 0, 0];
            meta_payload.extend_from_slice(&iinf);
            meta_payload.extend_from_slice(&boxed(b"iloc", &iloc_payload));
            ftyp.len() + 8 + meta_payload.len() + 8
        };
        // The payload sits right after the mdat header.
        let absolute = meta_prefix_len as u32;
        iloc_payload[offset_at..offset_at + 4].copy_from_slice(&absolute.to_be_bytes());

        let mut meta_payload = vec![0, 0, 0, 0];
        meta_payload.extend_from_slice(&iinf);
        meta_payload.extend_from_slice(&boxed(b"iloc", &iloc_payload));

        let mut file = ftyp;
        file.extend_from_slice(&boxed(b"meta", &meta_payload));
        file.extend_from_slice(&boxed(b"mdat", &payload));
        file
    }

    #[test]
    fn detects_the_heif_family_by_brand() {
        assert!(detect(&heif()));
        assert!(detect(&boxed(b"ftyp", b"avif\0\0\0\0avifmif1")));
        assert!(!detect(&boxed(b"ftyp", b"mp42\0\0\0\0mp42isom")));
        assert!(!detect(b"not a box at all"));
    }

    #[test]
    fn wipes_the_exif_payload_where_it_lies() {
        let original = heif();
        let result = strip(&original).unwrap();

        assert_eq!(result.data.len(), original.len(), "nothing may move");
        assert!(!result.data.windows(5).any(|w| w == b"Canon"));
        assert!(result.findings.iter().any(|f| f.detail.contains("Canon")));
    }

    #[test]
    fn says_that_the_empty_item_declaration_remains() {
        let result = strip(&heif()).unwrap();
        assert!(result.kept.iter().any(|f| f.label == "Empty metadata slots"));
        // The declaration really is still there, which is what `kept` warns of.
        assert!(result.data.windows(4).any(|w| w == b"Exif"));
    }

    #[test]
    fn cleaning_twice_finds_nothing_the_second_time() {
        let once = strip(&heif()).unwrap();
        let twice = strip(&once.data).unwrap();
        assert!(twice.findings.is_empty(), "{:?}", twice.findings);
        assert_eq!(once.data, twice.data);
    }

    #[test]
    fn refuses_a_file_that_points_its_metadata_at_the_header() {
        // A real fixture once claimed offset 0, length 378 for its Exif item.
        // Trusting that blanked the ftyp box and the picture with it, so this
        // is now refused outright rather than "cleaned".
        let mut file = heif();
        let iloc = file.windows(4).position(|w| w == b"iloc").unwrap();
        // Version 1 entry: id, construction, data reference, extent count,
        // then the offset — sixteen bytes into the box payload.
        let entry = iloc + 4 + 16;
        file[entry..entry + 4].copy_from_slice(&0u32.to_be_bytes());
        file[entry + 4..entry + 8].copy_from_slice(&64u32.to_be_bytes());

        let error = strip(&file).unwrap_err();
        assert!(error.contains("wrong part of itself"), "{error}");
    }

    #[test]
    fn wipes_an_exif_item_stored_in_idat() {
        let payload = exif_payload();
        let infe = {
            let mut v = vec![2, 0, 0, 0];
            v.extend_from_slice(&1u16.to_be_bytes());
            v.extend_from_slice(&0u16.to_be_bytes());
            v.extend_from_slice(b"Exif");
            v.push(0);
            boxed(b"infe", &v)
        };
        let mut iinf = vec![0, 0, 0, 0];
        iinf.extend_from_slice(&1u16.to_be_bytes());
        iinf.extend_from_slice(&infe);

        // iloc version 1, construction method 1: offset 0 into idat.
        let mut iloc = vec![1, 0, 0, 0, 0x44, 0x00];
        iloc.extend_from_slice(&1u16.to_be_bytes()); // item count
        iloc.extend_from_slice(&1u16.to_be_bytes()); // item id
        iloc.extend_from_slice(&1u16.to_be_bytes()); // construction 1
        iloc.extend_from_slice(&0u16.to_be_bytes()); // data ref
        iloc.extend_from_slice(&1u16.to_be_bytes()); // extent count
        iloc.extend_from_slice(&0u32.to_be_bytes());
        iloc.extend_from_slice(&(payload.len() as u32).to_be_bytes());

        let mut meta = vec![0, 0, 0, 0];
        meta.extend_from_slice(&boxed(b"iinf", &iinf));
        meta.extend_from_slice(&boxed(b"iloc", &iloc));
        meta.extend_from_slice(&boxed(b"idat", &payload));

        let mut file = boxed(b"ftyp", b"heic\0\0\0\0mif1heic");
        file.extend_from_slice(&boxed(b"meta", &meta));

        let result = strip(&file).unwrap();
        assert!(result.findings.iter().any(|f| f.detail.contains("Canon")), "{:?}", result.findings);
        assert!(!result.data.windows(5).any(|w| w == b"Canon"));
        assert_eq!(result.data.len(), file.len());
    }

    #[test]
    fn survives_truncation_without_panicking() {
        let full = heif();
        for n in 0..full.len() {
            let _ = strip(&full[..n]);
        }
    }
}
