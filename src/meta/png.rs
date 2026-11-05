//! PNG chunk filter.
//!
//! Chunks are copied through with their original CRC, so nothing is
//! recompressed and the image data is untouched.

use super::exif;
use super::{Finding, StripResult};

pub const SIGNATURE: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

pub fn strip(data: &[u8]) -> Result<StripResult, String> {
    if !data.starts_with(SIGNATURE) {
        return Err("not a PNG file".into());
    }

    let mut out = Vec::with_capacity(data.len());
    let mut findings = Vec::new();
    out.extend_from_slice(SIGNATURE);

    let mut pos = SIGNATURE.len();
    while pos + 8 <= data.len() {
        let len = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
        let kind = &data[pos + 4..pos + 8];
        let Some(end) = (len as usize)
            .checked_add(12)
            .and_then(|n| pos.checked_add(n))
            .filter(|&e| e <= data.len())
        else {
            return Err("malformed PNG: chunk runs past end of file".into());
        };
        let payload = &data[pos + 8..end - 4];

        if kind == b"eXIf" {
            // Orientation is kept, as for JPEG: it is presentation, and
            // dropping it turns a sideways-shot picture on its side. Our own
            // orientation-only chunk is already clean.
            let info = exif::analyse(payload);
            let ours = info.orientation.map(super::jpeg::minimal_tiff);
            if ours.as_deref() != Some(payload) {
                findings.push(exif_finding(&info));
                if let Some(tiff) = ours {
                    out.extend_from_slice(&chunk(b"eXIf", &tiff));
                }
            } else {
                out.extend_from_slice(&data[pos..end]);
            }
        } else {
            match describe(kind, payload) {
                Some(f) => findings.push(f),
                None => out.extend_from_slice(&data[pos..end]),
            }
        }

        pos = end;
        if kind == b"IEND" {
            let trailing = data.len() - pos;
            if trailing > 0 {
                findings.push(Finding::new(
                    "Trailing data",
                    format!("{trailing} bytes hidden after the end of the image"),
                ));
            }
            return Ok(StripResult::new(out, findings));
        }
    }

    Ok(StripResult::new(out, findings))
}

/// `Some` means the chunk carries metadata and is dropped; `None` keeps it.
fn describe(kind: &[u8], payload: &[u8]) -> Option<Finding> {
    match kind {
        b"tEXt" | b"zTXt" | b"iTXt" => {
            let keyword = payload
                .split(|&b| b == 0)
                .next()
                .map(|k| String::from_utf8_lossy(k).trim().to_string())
                .filter(|k| !k.is_empty())
                .unwrap_or_else(|| "text".into());
            // Only tEXt is stored plain; the others are compressed or tagged,
            // so name the field rather than guessing at its contents.
            let detail = if kind == b"tEXt" {
                payload
                    .splitn(2, |&b| b == 0)
                    .nth(1)
                    .map(|v| summarise(&String::from_utf8_lossy(v)))
                    .unwrap_or_default()
            } else {
                "embedded text".to_string()
            };
            Some(Finding::new(format!("Text: {keyword}"), detail))
        }
        b"tIME" if payload.len() >= 7 => {
            let year = u16::from_be_bytes([payload[0], payload[1]]);
            Some(Finding::new(
                "Last modified",
                format!(
                    "{year:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                    payload[2], payload[3], payload[4], payload[5], payload[6]
                ),
            ))
        }
        b"tIME" => Some(Finding::new("Last modified", "modification timestamp")),
        _ => None,
    }
}

fn exif_finding(info: &exif::ExifInfo) -> Finding {
    let detail = if info.findings.is_empty() {
        "camera and capture data".to_string()
    } else {
        info.findings
            .iter()
            .map(|f| format!("{}: {}", f.label, f.detail))
            .collect::<Vec<_>>()
            .join("; ")
    };
    Finding::new("Exif", detail)
}

/// A complete chunk: length, type, payload and CRC over type and payload.
fn chunk(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut v = (payload.len() as u32).to_be_bytes().to_vec();
    v.extend_from_slice(kind);
    v.extend_from_slice(payload);
    let mut crc = crc32fast::Hasher::new();
    crc.update(kind);
    crc.update(payload);
    v.extend_from_slice(&crc.finalize().to_be_bytes());
    v
}

fn summarise(text: &str) -> String {
    let one_line: String = text.trim().chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let one_line = one_line.trim();
    match one_line.char_indices().nth(80) {
        Some((i, _)) => format!("{}\u{2026}", &one_line[..i]),
        None => one_line.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = (payload.len() as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(payload);
        v.extend_from_slice(&[0, 0, 0, 0]); // CRC; copied through, never checked
        v
    }

    fn png(extra: &[Vec<u8>]) -> Vec<u8> {
        let mut v = SIGNATURE.to_vec();
        v.extend_from_slice(&chunk(b"IHDR", &[0; 13]));
        for c in extra {
            v.extend_from_slice(c);
        }
        v.extend_from_slice(&chunk(b"IDAT", b"pixels"));
        v.extend_from_slice(&chunk(b"IEND", b""));
        v
    }

    #[test]
    fn keeps_orientation_in_a_rebuilt_exif_chunk() {
        let r = strip(&png(&[chunk(b"eXIf", &crate::meta::jpeg::oriented_tiff_with_make())])).unwrap();
        assert!(r.findings.iter().any(|f| f.label == "Exif" && f.detail.contains("Cam")));
        let tiff = crate::meta::jpeg::minimal_tiff(6);
        assert_eq!(r.data, png(&[super::chunk(b"eXIf", &tiff)]));
        // A real CRC, not a copied one: the chunk is new.
        let crc = &r.data[r.data.len() - 12 - 18 - 4..][..4];
        assert_eq!(crc, crc32fast::hash(&[b"eXIf".as_slice(), &tiff].concat()).to_be_bytes());
        let again = strip(&r.data).unwrap();
        assert!(again.findings.is_empty(), "{:?}", again.findings);
        assert_eq!(again.data, r.data);
    }

    #[test]
    fn drops_text_chunks_and_names_the_keyword() {
        let mut text = b"Author".to_vec();
        text.push(0);
        text.extend_from_slice(b"Jane Doe");
        let r = strip(&png(&[chunk(b"tEXt", &text)])).unwrap();

        assert_eq!(r.data, png(&[]));
        assert_eq!(r.findings[0].label, "Text: Author");
        assert_eq!(r.findings[0].detail, "Jane Doe");
    }

    #[test]
    fn keeps_colour_management_chunks() {
        let icc = chunk(b"iCCP", b"profile");
        let srgb = chunk(b"sRGB", &[0]);
        let input = png(&[icc, srgb]);
        assert_eq!(strip(&input).unwrap().data, input);
    }

    #[test]
    fn drops_timestamps_and_trailing_data() {
        let time = chunk(b"tIME", &[0x07, 0xE8, 5, 11, 9, 30, 0]);
        let mut input = png(&[time]);
        input.extend_from_slice(b"junk");

        let r = strip(&input).unwrap();
        assert_eq!(r.data, png(&[]));
        assert!(r.findings.iter().any(|f| f.detail.contains("2024-05-11")));
        assert!(r.findings.iter().any(|f| f.label == "Trailing data"));
    }

    #[test]
    fn rejects_non_png_and_survives_truncation() {
        assert!(strip(b"not a png").is_err());
        let full = png(&[chunk(b"tEXt", b"k\0v")]);
        for n in 0..full.len() {
            let _ = strip(&full[..n]); // must not panic
        }
    }
}
