//! JPEG segment filter.
//!
//! Rebuilds the file segment by segment, copying the compressed image data
//! through byte for byte. Nothing is decoded and nothing is re-encoded, so a
//! cleaned photo is pixel-identical to the original — losing image quality to
//! remove Exif would be a bug, not a trade-off.

use super::exif;
use super::{Finding, StripResult};

const SOI: u8 = 0xD8;
const EOI: u8 = 0xD9;
const SOS: u8 = 0xDA;
const COM: u8 = 0xFE;

pub fn strip(data: &[u8]) -> Result<StripResult, String> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != SOI {
        return Err("not a JPEG file".into());
    }

    let mut out = Vec::with_capacity(data.len());
    let mut findings = Vec::new();
    let mut orientation = None;
    let mut pos = 2;

    out.extend_from_slice(&data[..2]);

    while pos + 1 < data.len() {
        if data[pos] != 0xFF {
            return Err("malformed JPEG: expected a segment marker".into());
        }
        // Any number of 0xFF bytes may pad the front of a marker.
        let mut mark_at = pos;
        while mark_at < data.len() && data[mark_at] == 0xFF {
            mark_at += 1;
        }
        let Some(&marker) = data.get(mark_at) else {
            return Err("malformed JPEG: truncated marker".into());
        };
        let body = mark_at + 1;

        if marker == EOI {
            out.extend_from_slice(&[0xFF, EOI]);
            // Some cameras and many "watermark remover" tools append their own
            // payload after the end-of-image marker, where no decoder looks.
            let trailing = data.len() - (body);
            if trailing > 0 {
                findings.push(Finding::new(
                    "Trailing data",
                    format!("{trailing} bytes hidden after the end of the image"),
                ));
            }
            return Ok(finish(out, findings, orientation));
        }

        // Markers that carry no length field at all.
        if marker == SOI || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            out.extend_from_slice(&[0xFF, marker]);
            pos = body;
            continue;
        }

        let Some(len) = be16(data, body) else {
            return Err("malformed JPEG: truncated segment length".into());
        };
        if len < 2 {
            return Err("malformed JPEG: bad segment length".into());
        }
        let seg_end = body + len as usize;
        let Some(payload) = data.get(body + 2..seg_end) else {
            return Err("malformed JPEG: segment runs past end of file".into());
        };

        match classify(marker, payload) {
            Segment::Keep => {
                out.extend_from_slice(&[0xFF, marker]);
                out.extend_from_slice(&data[body..seg_end]);
            }
            Segment::DropExif => {
                let info = exif::analyse(&payload[6..]);
                // The reader describes the tags it knows. An Exif block holding
                // only others — ImageDescription, XPAuthor, LensModel — is still
                // removed, and must still be reported, or the file is called
                // clean and never written. The one exception is the
                // orientation-only block this module writes itself.
                let ours = info.orientation.map(minimal_exif);
                if info.findings.is_empty() && ours.as_deref().map(|s| &s[2..]) != Some(&data[body..seg_end]) {
                    findings.push(Finding::new("Exif", "camera and capture data"));
                }
                findings.extend(info.findings);
                orientation = orientation.or(info.orientation);
            }
            Segment::Drop(label, detail) => {
                findings.push(Finding::new(label, detail));
            }
        }

        pos = seg_end;

        if marker == SOS {
            // Entropy-coded data follows, with no length of its own. Copy it
            // verbatim to the next real marker; progressive JPEGs come back
            // round for a further SOS.
            let end = scan_end(data, pos);
            out.extend_from_slice(&data[pos..end]);
            pos = end;
        }
    }

    // No EOI. The image is still usable, so finish rather than fail.
    Ok(finish(out, findings, orientation))
}

enum Segment {
    Keep,
    DropExif,
    Drop(&'static str, String),
}

fn classify(marker: u8, payload: &[u8]) -> Segment {
    match marker {
        // APP0 is JFIF (density and aspect, no identity).
        0xE0 => Segment::Keep,
        0xE1 if payload.starts_with(b"Exif\0\0") => Segment::DropExif,
        0xE1 if payload.starts_with(b"http://ns.adobe.com/xap/1.0/\0") => Segment::Drop(
            "XMP",
            "Adobe sidecar metadata: editing history, creator, ratings".into(),
        ),
        0xE1 => Segment::Drop("APP1", "unrecognised metadata block".into()),
        // ICC colour profiles change how the image is rendered, so they stay.
        0xE2 if payload.starts_with(b"ICC_PROFILE\0") => Segment::Keep,
        0xE2 => Segment::Drop("APP2", "unrecognised metadata block".into()),
        0xED => Segment::Drop(
            "IPTC / Photoshop",
            "captions, credits, keywords and Photoshop's own resources".into(),
        ),
        // APP14 tells the decoder how colour components are transformed.
        // Removing it turns some CMYK images inside out.
        0xEE => Segment::Keep,
        0xE3..=0xEC | 0xEF => Segment::Drop("Application data", "vendor metadata block".into()),
        COM => Segment::Drop("Comment", "free-text comment".into()),
        _ => Segment::Keep,
    }
}

fn finish(mut out: Vec<u8>, findings: Vec<Finding>, orientation: Option<u16>) -> StripResult {
    if let Some(o) = orientation {
        // Splice the rebuilt Exif in directly after SOI, where APP segments live.
        let seg = minimal_exif(o);
        out.splice(2..2, seg);
    }
    StripResult::new(out, findings)
}

/// An Exif block holding the Orientation tag and nothing else.
///
/// The tag is metadata, but it is the one piece that is also *presentation*:
/// drop it and a phone photo shot sideways displays sideways. Re-encoding the
/// pixels to bake the rotation in would cost image quality, so the tag is kept
/// on its own. It says which way is up and nothing about who took the picture.
fn minimal_exif(orientation: u16) -> Vec<u8> {
    let mut tiff = Vec::new();
    tiff.extend_from_slice(b"II");
    tiff.extend_from_slice(&42u16.to_le_bytes());
    tiff.extend_from_slice(&8u32.to_le_bytes());
    tiff.extend_from_slice(&1u16.to_le_bytes());
    tiff.extend_from_slice(&0x0112u16.to_le_bytes());
    tiff.extend_from_slice(&3u16.to_le_bytes());
    tiff.extend_from_slice(&1u32.to_le_bytes());
    tiff.extend_from_slice(&orientation.to_le_bytes());
    tiff.extend_from_slice(&[0, 0]);
    tiff.extend_from_slice(&0u32.to_le_bytes());

    let mut seg = vec![0xFF, 0xE1];
    seg.extend_from_slice(&((tiff.len() + 8) as u16).to_be_bytes());
    seg.extend_from_slice(b"Exif\0\0");
    seg.extend_from_slice(&tiff);
    seg
}

/// Walk entropy-coded data to the next marker that ends it. `0xFF00` is a
/// stuffed literal byte and restart markers belong to the scan, so neither
/// counts as the end.
fn scan_end(data: &[u8], from: usize) -> usize {
    let mut i = from;
    while i + 1 < data.len() {
        if data[i] == 0xFF {
            let next = data[i + 1];
            if next != 0x00 && next != 0xFF && !(0xD0..=0xD7).contains(&next) {
                return i;
            }
        }
        i += 1;
    }
    data.len()
}

fn be16(data: &[u8], at: usize) -> Option<u16> {
    let b = data.get(at..at + 2)?;
    Some(u16::from_be_bytes([b[0], b[1]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(marker: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![0xFF, marker];
        v.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        v.extend_from_slice(payload);
        v
    }

    /// A structurally valid JPEG: SOI, the segments given, SOS, scan data, EOI.
    fn jpeg(segments: &[Vec<u8>]) -> Vec<u8> {
        let mut v = vec![0xFF, SOI];
        for s in segments {
            v.extend_from_slice(s);
        }
        v.extend_from_slice(&seg(SOS, &[0x01, 0x00]));
        v.extend_from_slice(&[0x12, 0x34, 0xFF, 0x00, 0x56]); // incl. a stuffed FF
        v.extend_from_slice(&[0xFF, EOI]);
        v
    }

    fn exif_app1() -> Vec<u8> {
        let mut p = b"Exif\0\0".to_vec();
        p.extend_from_slice(b"II");
        p.extend_from_slice(&42u16.to_le_bytes());
        p.extend_from_slice(&8u32.to_le_bytes());
        p.extend_from_slice(&1u16.to_le_bytes());
        p.extend_from_slice(&0x010Fu16.to_le_bytes()); // Make
        p.extend_from_slice(&2u16.to_le_bytes()); // ASCII
        p.extend_from_slice(&6u32.to_le_bytes());
        p.extend_from_slice(&26u32.to_le_bytes()); // offset within the TIFF block
        p.extend_from_slice(&0u32.to_le_bytes());
        p.extend_from_slice(b"Canon\0");
        seg(0xE1, &p)
    }

    #[test]
    fn removes_exif_and_reports_the_camera() {
        let input = jpeg(&[exif_app1()]);
        let r = strip(&input).unwrap();
        assert!(!r.data.windows(4).any(|w| w == b"Exif"));
        assert!(r.findings.iter().any(|f| f.detail.contains("Canon")));
        assert!(r.data.len() < input.len());
    }

    #[test]
    fn keeps_the_image_data_byte_for_byte() {
        let clean = jpeg(&[]);
        let dirty = jpeg(&[exif_app1(), seg(COM, b"secret note")]);
        assert_eq!(strip(&dirty).unwrap().data, clean);
        // ...and cleaning an already-clean file changes nothing.
        assert_eq!(strip(&clean).unwrap().data, clean);
    }

    #[test]
    fn keeps_icc_profiles_but_drops_other_app2() {
        let icc = seg(0xE2, b"ICC_PROFILE\0stuff");
        assert_eq!(strip(&jpeg(std::slice::from_ref(&icc))).unwrap().data, jpeg(&[icc]));

        let other = seg(0xE2, b"something else");
        assert_eq!(strip(&jpeg(&[other])).unwrap().data, jpeg(&[]));
    }

    #[test]
    fn preserves_orientation_so_photos_do_not_rotate() {
        let mut p = b"Exif\0\0".to_vec();
        p.extend_from_slice(b"II");
        p.extend_from_slice(&42u16.to_le_bytes());
        p.extend_from_slice(&8u32.to_le_bytes());
        p.extend_from_slice(&1u16.to_le_bytes());
        p.extend_from_slice(&0x0112u16.to_le_bytes());
        p.extend_from_slice(&3u16.to_le_bytes());
        p.extend_from_slice(&1u32.to_le_bytes());
        p.extend_from_slice(&6u32.to_le_bytes());
        p.extend_from_slice(&0u32.to_le_bytes());

        let r = strip(&jpeg(&[seg(0xE1, &p)])).unwrap();
        let tiff_at = r
            .data
            .windows(6)
            .position(|w| w == b"Exif\0\0")
            .expect("a rebuilt Exif block")
            + 6;
        let back = exif::analyse(&r.data[tiff_at..]);
        assert_eq!(back.orientation, Some(6));
        // The rebuilt block carries the one tag and no camera identity.
        assert!(back.findings.is_empty());
    }

    #[test]
    fn exif_the_reader_cannot_describe_is_still_reported() {
        // ImageDescription only: no tag the Exif reader names.
        let mut p = b"Exif\0\0".to_vec();
        p.extend_from_slice(b"II");
        p.extend_from_slice(&42u16.to_le_bytes());
        p.extend_from_slice(&8u32.to_le_bytes());
        p.extend_from_slice(&1u16.to_le_bytes());
        p.extend_from_slice(&0x010Eu16.to_le_bytes());
        p.extend_from_slice(&2u16.to_le_bytes());
        p.extend_from_slice(&4u32.to_le_bytes());
        p.extend_from_slice(b"Jan\0");
        p.extend_from_slice(&0u32.to_le_bytes());

        let r = strip(&jpeg(&[seg(0xE1, &p)])).unwrap();
        assert!(!r.findings.is_empty(), "removed without being reported");
        assert_eq!(r.data, jpeg(&[]));
    }

    #[test]
    fn the_rebuilt_orientation_block_is_not_a_finding() {
        let once = strip(&jpeg(&[seg(0xE1, &{
            let mut p = b"Exif\0\0".to_vec();
            p.extend_from_slice(&minimal_exif(6)[10..]);
            p.extend_from_slice(b"extra");
            p
        })]))
        .unwrap();
        let twice = strip(&once.data).unwrap();
        assert!(twice.findings.is_empty(), "{:?}", twice.findings);
        assert_eq!(once.data, twice.data);
    }

    #[test]
    fn strips_data_appended_after_the_end_marker() {
        let mut input = jpeg(&[]);
        input.extend_from_slice(b"appended payload");
        let r = strip(&input).unwrap();
        assert_eq!(r.data, jpeg(&[]));
        assert!(r.findings.iter().any(|f| f.label == "Trailing data"));
    }

    #[test]
    fn rejects_non_jpeg_and_survives_truncation() {
        assert!(strip(b"not a jpeg").is_err());
        let full = jpeg(&[exif_app1()]);
        for n in 0..full.len() {
            let _ = strip(&full[..n]); // must not panic
        }
    }
}
