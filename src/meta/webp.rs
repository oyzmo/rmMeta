//! WebP chunk filter.
//!
//! WebP is a RIFF container. Dropping the EXIF and XMP chunks is
//! straightforward, but the extended-format header `VP8X` carries a flag bit
//! for each of them: leave those set with the chunks gone and a decoder is
//! told to expect metadata that is not there. The flags are cleared to match.

use super::exif;
use super::{Finding, StripResult};

// Bits in the VP8X flags byte.
const FLAG_EXIF: u8 = 0x08;
const FLAG_XMP: u8 = 0x04;

pub fn strip(data: &[u8]) -> Result<StripResult, String> {
    if !(data.starts_with(b"RIFF") && data.get(8..12) == Some(b"WEBP")) {
        return Err("not a WebP file".into());
    }

    let declared = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;
    // The RIFF size counts everything after the size field itself.
    let end = (declared + 8).min(data.len());

    let mut body = Vec::with_capacity(data.len());
    let mut findings = Vec::new();
    let mut pos = 12;

    while pos + 8 <= end {
        let fourcc = &data[pos..pos + 4];
        let size = u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
            as usize;
        // Chunks are padded to an even length.
        let padded = size + (size & 1);
        let Some(payload_end) = pos.checked_add(8).and_then(|s| s.checked_add(size)) else {
            return Err("malformed WebP: chunk size overflows".into());
        };
        if payload_end > end {
            return Err("malformed WebP: chunk runs past end of file".into());
        }
        let payload = &data[pos + 8..payload_end];
        let chunk_end = (pos + 8 + padded).min(end);

        match fourcc {
            b"EXIF" => {
                let info = exif::analyse(strip_exif_prefix(payload));
                if info.findings.is_empty() {
                    findings.push(Finding::new("Exif", "camera and capture data"));
                } else {
                    findings.extend(info.findings);
                }
            }
            b"XMP " => findings.push(Finding::new(
                "XMP metadata",
                "creator, editing history and ratings as XML",
            )),
            _ => {
                let start = body.len();
                body.extend_from_slice(&data[pos..chunk_end]);
                if fourcc == b"VP8X" && size >= 1 {
                    // Now that the chunks are gone, say so in the header.
                    body[start + 8] &= !(FLAG_EXIF | FLAG_XMP);
                }
            }
        }

        pos = chunk_end;
    }

    let trailing = data.len() - end;
    if trailing > 0 {
        findings.push(Finding::new(
            "Trailing data",
            format!("{trailing} bytes hidden after the end of the image"),
        ));
    }

    let mut out = Vec::with_capacity(body.len() + 12);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
    out.extend_from_slice(b"WEBP");
    out.extend_from_slice(&body);

    Ok(StripResult::new(out, findings))
}

/// Some writers put the JPEG-style `Exif\0\0` marker in front of the TIFF
/// block even though WebP does not call for it.
fn strip_exif_prefix(payload: &[u8]) -> &[u8] {
    payload.strip_prefix(b"Exif\0\0").unwrap_or(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = fourcc.to_vec();
        v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        v.extend_from_slice(payload);
        if payload.len() % 2 == 1 {
            v.push(0);
        }
        v
    }

    fn webp(chunks: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = chunks.concat();
        let mut v = b"RIFF".to_vec();
        v.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
        v.extend_from_slice(b"WEBP");
        v.extend_from_slice(&body);
        v
    }

    /// VP8X with the ICC, EXIF and XMP flags all set.
    fn vp8x(flags: u8) -> Vec<u8> {
        let mut payload = vec![flags, 0, 0, 0];
        payload.extend_from_slice(&[1, 0, 0, 1, 0, 0]); // canvas size
        chunk(b"VP8X", &payload)
    }

    #[test]
    fn drops_exif_and_xmp_but_keeps_the_picture() {
        let picture = chunk(b"VP8 ", b"compressed pixels");
        let input = webp(&[
            vp8x(0x20 | FLAG_EXIF | FLAG_XMP),
            picture.clone(),
            chunk(b"EXIF", b"II\x2a\x00\x08\x00\x00\x00\x00\x00"),
            chunk(b"XMP ", b"<x:xmpmeta/>"),
        ]);

        let result = strip(&input).unwrap();
        assert_eq!(result.data, webp(&[vp8x(0x20), picture]));

        let labels: Vec<&str> = result.findings.iter().map(|f| f.label.as_str()).collect();
        assert!(labels.contains(&"XMP metadata"), "{labels:?}");
    }

    #[test]
    fn clears_the_header_flags_so_the_file_stays_consistent() {
        let input = webp(&[
            vp8x(FLAG_EXIF | FLAG_XMP),
            chunk(b"VP8 ", b"pixels"),
            chunk(b"EXIF", b"whatever"),
        ]);
        let result = strip(&input).unwrap();

        let flags = result.data[12 + 8];
        assert_eq!(flags & FLAG_EXIF, 0, "the EXIF flag still claims metadata is present");
        assert_eq!(flags & FLAG_XMP, 0);
    }

    #[test]
    fn keeps_colour_profiles_and_animation_chunks() {
        let input = webp(&[
            vp8x(0x22),
            chunk(b"ICCP", b"colour profile"),
            chunk(b"ANIM", b"\0\0\0\0\0\0"),
            chunk(b"ANMF", b"frame"),
        ]);
        assert_eq!(strip(&input).unwrap().data, input);
    }

    #[test]
    fn corrects_the_riff_size_and_strips_trailing_data() {
        let picture = chunk(b"VP8 ", b"pixels");
        let mut input = webp(&[picture.clone(), chunk(b"XMP ", b"<x/>")]);
        input.extend_from_slice(b"junk");

        let result = strip(&input).unwrap();
        assert_eq!(result.data, webp(&[picture]));
        let declared = u32::from_le_bytes([
            result.data[4],
            result.data[5],
            result.data[6],
            result.data[7],
        ]) as usize;
        assert_eq!(declared + 8, result.data.len(), "RIFF size does not match the file");
        assert!(result.findings.iter().any(|f| f.label == "Trailing data"));
    }

    #[test]
    fn rejects_non_webp_and_survives_truncation() {
        assert!(strip(b"RIFF____NOTWEBP").is_err());
        let full = webp(&[chunk(b"VP8 ", b"pixels"), chunk(b"XMP ", b"<x/>")]);
        for n in 0..full.len() {
            let _ = strip(&full[..n]); // must not panic
        }
    }
}
