//! Minimal, bounds-checked TIFF/Exif reader.
//!
//! This exists to *describe* what is about to be removed, not to decode images.
//! It never panics on malformed input: every read is fallible and a truncated
//! or nonsensical IFD simply yields fewer findings.
//!
//! The one thing it feeds back into stripping is `orientation`. Dropping the
//! Orientation tag visually rotates a photo, and we refuse to re-encode pixels
//! to compensate, so a non-default orientation is preserved in a rebuilt Exif
//! block containing that tag and nothing else. See `jpeg::minimal_exif`.

use super::Finding;

// IFD0
const TAG_MAKE: u16 = 0x010F;
const TAG_MODEL: u16 = 0x0110;
const TAG_ORIENTATION: u16 = 0x0112;
const TAG_SOFTWARE: u16 = 0x0131;
const TAG_DATETIME: u16 = 0x0132;
const TAG_ARTIST: u16 = 0x013B;
const TAG_COPYRIGHT: u16 = 0x8298;
const TAG_EXIF_IFD: u16 = 0x8769;
const TAG_GPS_IFD: u16 = 0x8825;

// Exif sub-IFD
const TAG_DATETIME_ORIGINAL: u16 = 0x9003;
const TAG_USER_COMMENT: u16 = 0x9286;
const TAG_MAKER_NOTE: u16 = 0x927C;
const TAG_IMAGE_UNIQUE_ID: u16 = 0xA420;
const TAG_OWNER_NAME: u16 = 0xA430;
const TAG_BODY_SERIAL: u16 = 0xA431;
const TAG_LENS_SERIAL: u16 = 0xA435;

// GPS sub-IFD
const TAG_GPS_LAT_REF: u16 = 1;
const TAG_GPS_LAT: u16 = 2;
const TAG_GPS_LON_REF: u16 = 3;
const TAG_GPS_LON: u16 = 4;

pub struct ExifInfo {
    pub findings: Vec<Finding>,
    /// `Some` only when it is not the default 1, i.e. when discarding it would
    /// change how the image looks.
    pub orientation: Option<u16>,
}

/// `data` is the TIFF block: it starts at the "II"/"MM" byte-order mark, which
/// is also the origin all internal offsets are relative to.
pub fn analyse(data: &[u8]) -> ExifInfo {
    let mut info = ExifInfo { findings: Vec::new(), orientation: None };
    let Some(tiff) = Tiff::new(data) else { return info };

    let Some(ifd0) = tiff.ifd(tiff.first_ifd) else { return info };

    let mut camera: Vec<String> = Vec::new();
    if let Some(v) = ifd0.ascii(TAG_MAKE) {
        camera.push(v);
    }
    if let Some(v) = ifd0.ascii(TAG_MODEL) {
        camera.push(v);
    }
    if !camera.is_empty() {
        info.findings.push(Finding::new("Camera", camera.join(" ")));
    }

    if let Some(v) = ifd0.short(TAG_ORIENTATION) {
        if v != 1 {
            info.orientation = Some(v);
        }
    }
    if let Some(v) = ifd0.ascii(TAG_SOFTWARE) {
        info.findings.push(Finding::new("Software", v));
    }
    if let Some(v) = ifd0.ascii(TAG_ARTIST) {
        info.findings.push(Finding::new("Artist", v));
    }
    if let Some(v) = ifd0.ascii(TAG_COPYRIGHT) {
        info.findings.push(Finding::new("Copyright", v));
    }

    let mut when = ifd0.ascii(TAG_DATETIME);

    if let Some(sub) = ifd0.long(TAG_EXIF_IFD).and_then(|o| tiff.ifd(o)) {
        when = when.or_else(|| sub.ascii(TAG_DATETIME_ORIGINAL));
        if let Some(v) = sub.ascii(TAG_OWNER_NAME) {
            info.findings.push(Finding::new("Camera owner", v));
        }
        for tag in [TAG_BODY_SERIAL, TAG_LENS_SERIAL] {
            if let Some(v) = sub.ascii(tag) {
                info.findings.push(Finding::new("Serial number", v));
            }
        }
        if sub.entry(TAG_MAKER_NOTE).is_some() {
            info.findings.push(Finding::new(
                "Maker note",
                "vendor-private block, often holds serial numbers and shutter count",
            ));
        }
        if sub.entry(TAG_IMAGE_UNIQUE_ID).is_some() {
            info.findings.push(Finding::new("Image ID", "unique per-image identifier"));
        }
        if sub.entry(TAG_USER_COMMENT).is_some() {
            info.findings.push(Finding::new("User comment", "free-text comment"));
        }
    }

    if let Some(v) = when {
        info.findings.push(Finding::new("Date taken", v));
    }

    if let Some(gps) = ifd0.long(TAG_GPS_IFD).and_then(|o| tiff.ifd(o)) {
        let detail = gps
            .coordinate(TAG_GPS_LAT, TAG_GPS_LAT_REF)
            .zip(gps.coordinate(TAG_GPS_LON, TAG_GPS_LON_REF))
            .map(|(lat, lon)| format!("{lat}, {lon}"))
            .unwrap_or_else(|| "coordinates recorded".to_string());
        info.findings.push(Finding::new("GPS location", detail));
    }

    // A second IFD is the embedded thumbnail. Worth calling out on its own: it
    // survives naive stripping and can still show the image as it was *before*
    // a crop or a paint-over.
    if let Some(next) = tiff.next_ifd(tiff.first_ifd) {
        if next != 0 && tiff.ifd(next).is_some() {
            info.findings.push(Finding::new(
                "Embedded thumbnail",
                "a second copy of the image, which may predate cropping or edits",
            ));
        }
    }

    info
}

struct Tiff<'a> {
    data: &'a [u8],
    le: bool,
    first_ifd: u32,
}

impl<'a> Tiff<'a> {
    fn new(data: &'a [u8]) -> Option<Self> {
        let le = match data.get(..2)? {
            b"II" => true,
            b"MM" => false,
            _ => return None,
        };
        let mut t = Tiff { data, le, first_ifd: 0 };
        // 42 is TIFF; the others are the ORF and RW2 variants, which are TIFF
        // directories under a different magic number.
        if !matches!(t.u16_at(2)?, 42 | 0x4F52 | 0x5352 | 0x0055) {
            return None;
        }
        t.first_ifd = t.u32_at(4)?;
        Some(t)
    }

    fn u16_at(&self, at: u32) -> Option<u16> {
        let b = self.data.get(at as usize..at as usize + 2)?;
        let b = [b[0], b[1]];
        Some(if self.le { u16::from_le_bytes(b) } else { u16::from_be_bytes(b) })
    }

    fn u32_at(&self, at: u32) -> Option<u32> {
        let b = self.data.get(at as usize..at as usize + 4)?;
        let b = [b[0], b[1], b[2], b[3]];
        Some(if self.le { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) })
    }

    fn entry_count(&self, at: u32) -> Option<u16> {
        let n = self.u16_at(at)?;
        // Reject an IFD that cannot fit, so a corrupt count can't make us walk
        // the whole file looking for entries.
        let end = at as u64 + 2 + n as u64 * 12 + 4;
        (end <= self.data.len() as u64).then_some(n)
    }

    fn ifd(&self, at: u32) -> Option<Ifd<'_, 'a>> {
        let count = self.entry_count(at)?;
        Some(Ifd { tiff: self, at, count })
    }

    fn next_ifd(&self, at: u32) -> Option<u32> {
        let count = self.entry_count(at)?;
        self.u32_at(at + 2 + count as u32 * 12)
    }
}

struct Ifd<'t, 'a> {
    tiff: &'t Tiff<'a>,
    at: u32,
    count: u16,
}

struct Entry {
    typ: u16,
    count: u32,
    /// Offset of the value: either the inline 4-byte field or the place it
    /// points at, already resolved.
    value_at: u32,
}

impl Ifd<'_, '_> {
    fn entry(&self, tag: u16) -> Option<Entry> {
        for i in 0..self.count {
            let e = self.at + 2 + i as u32 * 12;
            if self.tiff.u16_at(e)? != tag {
                continue;
            }
            let typ = self.tiff.u16_at(e + 2)?;
            let count = self.tiff.u32_at(e + 4)?;
            let size = type_size(typ)? as u64 * count as u64;
            let value_at = if size <= 4 { e + 8 } else { self.tiff.u32_at(e + 8)? };
            // Bounds-check once here so every reader below can stay simple.
            if value_at as u64 + size > self.tiff.data.len() as u64 {
                return None;
            }
            return Some(Entry { typ, count, value_at });
        }
        None
    }

    fn ascii(&self, tag: u16) -> Option<String> {
        let e = self.entry(tag)?;
        if e.typ != 2 {
            return None;
        }
        let raw = self.tiff.data.get(e.value_at as usize..(e.value_at + e.count) as usize)?;
        let text: String = raw
            .iter()
            .take_while(|&&b| b != 0)
            .map(|&b| if b.is_ascii_graphic() || b == b' ' { b as char } else { ' ' })
            .collect();
        let text = text.trim().to_string();
        (!text.is_empty()).then_some(text)
    }

    fn short(&self, tag: u16) -> Option<u16> {
        let e = self.entry(tag)?;
        (e.typ == 3).then(|| self.tiff.u16_at(e.value_at))?
    }

    fn long(&self, tag: u16) -> Option<u32> {
        let e = self.entry(tag)?;
        // Type 13 is IFD, a LONG that is declared to be a directory pointer.
        matches!(e.typ, 4 | 13).then(|| self.tiff.u32_at(e.value_at))?
    }

    fn rational(&self, at: u32) -> Option<f64> {
        let num = self.tiff.u32_at(at)?;
        let den = self.tiff.u32_at(at + 4)?;
        (den != 0).then(|| num as f64 / den as f64)
    }

    /// Degrees/minutes/seconds triple plus its N/S/E/W reference, rendered for
    /// display. Returns `None` unless the whole triple is present.
    fn coordinate(&self, tag: u16, ref_tag: u16) -> Option<String> {
        let e = self.entry(tag)?;
        if e.typ != 5 || e.count < 3 {
            return None;
        }
        let deg = self.rational(e.value_at)?;
        let min = self.rational(e.value_at + 8)?;
        let sec = self.rational(e.value_at + 16)?;
        let hemisphere = self.ascii(ref_tag).unwrap_or_default();
        let decimal = deg + min / 60.0 + sec / 3600.0;
        Some(format!("{decimal:.5}\u{b0} {hemisphere}").trim().to_string())
    }
}

fn type_size(typ: u16) -> Option<u32> {
    Some(match typ {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 | 13 => 4,
        5 | 10 | 12 => 8,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Little-endian TIFF with one IFD0 entry: Orientation = 6.
    fn tiff_with_orientation() -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"II");
        v.extend_from_slice(&42u16.to_le_bytes());
        v.extend_from_slice(&8u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes()); // entry count
        v.extend_from_slice(&TAG_ORIENTATION.to_le_bytes());
        v.extend_from_slice(&3u16.to_le_bytes()); // SHORT
        v.extend_from_slice(&1u32.to_le_bytes());
        v.extend_from_slice(&6u32.to_le_bytes()); // inline value
        v.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
        v
    }

    #[test]
    fn reads_orientation_and_ignores_the_default() {
        assert_eq!(analyse(&tiff_with_orientation()).orientation, Some(6));

        let mut upright = tiff_with_orientation();
        upright[18] = 1;
        assert_eq!(analyse(&upright).orientation, None);
    }

    #[test]
    fn truncated_input_yields_nothing_rather_than_panicking() {
        let full = tiff_with_orientation();
        for n in 0..full.len() {
            let info = analyse(&full[..n]);
            assert!(info.findings.is_empty());
        }
        analyse(b"").findings.is_empty().then_some(()).unwrap();
        analyse(b"not a tiff at all");
    }

    #[test]
    fn absurd_entry_count_is_rejected() {
        let mut v = tiff_with_orientation();
        v[8] = 0xFF;
        v[9] = 0xFF;
        assert!(analyse(&v).findings.is_empty());
    }
}
