//! GIF block filter.
//!
//! Image data is copied through untouched; only the extension blocks that
//! carry text are dropped. The one trap is the application extension: most of
//! them are metadata, but `NETSCAPE2.0` is what makes an animated GIF loop, so
//! dropping it wholesale would leave animations playing once and stopping.

use super::{Finding, StripResult};

/// Application extensions that are function, not metadata, and must survive.
const KEEP_APPLICATIONS: [&[u8]; 2] = [
    b"NETSCAPE2.0", // loop count for animations
    b"ICCRGBG1012", // embedded colour profile
];

pub fn strip(data: &[u8]) -> Result<StripResult, String> {
    if !(data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a")) {
        return Err("not a GIF file".into());
    }

    let mut pos = 6;
    let packed = *data.get(10).ok_or("malformed GIF: truncated screen descriptor")?;
    pos += 7;
    if packed & 0x80 != 0 {
        pos += 3 * (1 << ((packed & 7) + 1));
    }
    if pos > data.len() {
        return Err("malformed GIF: colour table runs past end of file".into());
    }

    let mut out = Vec::with_capacity(data.len());
    let mut findings = Vec::new();
    out.extend_from_slice(&data[..pos]);

    while pos < data.len() {
        match data[pos] {
            0x3B => {
                // Trailer.
                out.push(0x3B);
                let trailing = data.len() - pos - 1;
                if trailing > 0 {
                    findings.push(Finding::new(
                        "Trailing data",
                        format!("{trailing} bytes hidden after the end of the image"),
                    ));
                }
                return Ok(StripResult::new(out, findings));
            }
            0x21 => {
                let label = *data.get(pos + 1).ok_or("malformed GIF: truncated extension")?;
                let blocks_start = pos + 2;
                let end = skip_sub_blocks(data, blocks_start)?;
                match describe(label, data, blocks_start) {
                    Some(finding) => findings.push(finding),
                    None => out.extend_from_slice(&data[pos..end]),
                }
                pos = end;
            }
            0x2C => {
                // Image descriptor: 10 bytes, then an optional local colour
                // table, then the LZW data. All of it is picture, so it is
                // copied verbatim.
                let local = *data.get(pos + 9).ok_or("malformed GIF: truncated image descriptor")?;
                let mut cursor = pos + 10;
                if local & 0x80 != 0 {
                    cursor += 3 * (1 << ((local & 7) + 1));
                }
                cursor += 1; // LZW minimum code size
                let end = skip_sub_blocks(data, cursor)?;
                out.extend_from_slice(&data[pos..end]);
                pos = end;
            }
            other => return Err(format!("malformed GIF: unexpected block 0x{other:02X}")),
        }
    }

    Ok(StripResult::new(out, findings))
}

/// `Some` means the extension is metadata and gets dropped.
fn describe(label: u8, data: &[u8], blocks_start: usize) -> Option<Finding> {
    match label {
        // Comment extension.
        0xFE => {
            let text = read_sub_blocks(data, blocks_start);
            let text = String::from_utf8_lossy(&text);
            let text: String =
                text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
            let text = text.trim();
            Some(Finding::new(
                "Comment",
                if text.is_empty() { "free-text comment".to_string() } else { text.to_string() },
            ))
        }
        0xFF => {
            let identifier = data.get(blocks_start + 1..blocks_start + 12)?;
            if KEEP_APPLICATIONS.contains(&identifier) {
                return None;
            }
            let name = String::from_utf8_lossy(identifier).trim().to_string();
            if identifier.starts_with(b"XMP Data") {
                Some(Finding::new(
                    "XMP metadata",
                    "creator, editing history and ratings as XML",
                ))
            } else {
                Some(Finding::new("Application data", format!("private block from {name}")))
            }
        }
        _ => None,
    }
}

/// Returns the offset just past the terminating empty block.
fn skip_sub_blocks(data: &[u8], mut pos: usize) -> Result<usize, String> {
    loop {
        let size = *data.get(pos).ok_or("malformed GIF: truncated data block")? as usize;
        pos += 1 + size;
        if size == 0 {
            if pos > data.len() {
                return Err("malformed GIF: data block runs past end of file".into());
            }
            return Ok(pos);
        }
    }
}

fn read_sub_blocks(data: &[u8], mut pos: usize) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(&size) = data.get(pos) {
        if size == 0 {
            break;
        }
        let start = pos + 1;
        let end = (start + size as usize).min(data.len());
        out.extend_from_slice(&data[start..end]);
        pos = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub_blocks(payload: &[u8]) -> Vec<u8> {
        let mut v = vec![payload.len() as u8];
        v.extend_from_slice(payload);
        v.push(0);
        v
    }

    fn extension(label: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![0x21, label];
        v.extend_from_slice(&sub_blocks(payload));
        v
    }

    fn application(identifier: &[u8; 11], payload: &[u8]) -> Vec<u8> {
        let mut v = vec![0x21, 0xFF, 11];
        v.extend_from_slice(identifier);
        v.extend_from_slice(&sub_blocks(payload));
        v
    }

    fn gif(blocks: &[Vec<u8>]) -> Vec<u8> {
        let mut v = b"GIF89a".to_vec();
        // 2x2, global colour table of 2 entries.
        v.extend_from_slice(&[2, 0, 2, 0, 0x80, 0, 0]);
        v.extend_from_slice(&[0, 0, 0, 255, 255, 255]);
        for b in blocks {
            v.extend_from_slice(b);
        }
        // One image: descriptor, no local table, LZW size, one data block.
        v.extend_from_slice(&[0x2C, 0, 0, 0, 0, 2, 0, 2, 0, 0, 0x02]);
        v.extend_from_slice(&sub_blocks(&[0x44, 0x01]));
        v.push(0x3B);
        v
    }

    #[test]
    fn drops_comments_and_keeps_the_image() {
        let input = gif(&[extension(0xFE, b"made by Jane Doe")]);
        let result = strip(&input).unwrap();

        assert_eq!(result.data, gif(&[]));
        assert_eq!(result.findings[0].label, "Comment");
        assert_eq!(result.findings[0].detail, "made by Jane Doe");
    }

    #[test]
    fn keeps_the_block_that_makes_animations_loop() {
        // NETSCAPE2.0 is an application extension like any other, but removing
        // it stops an animated GIF looping.
        let loops = application(b"NETSCAPE2.0", &[1, 0, 0]);
        let input = gif(std::slice::from_ref(&loops));
        assert_eq!(strip(&input).unwrap().data, input);

        let xmp = application(b"XMP DataXMP", b"<x:xmpmeta/>");
        let result = strip(&gif(&[xmp, loops.clone()])).unwrap();
        assert_eq!(result.data, gif(&[loops]));
        assert!(result.findings.iter().any(|f| f.label == "XMP metadata"));
    }

    #[test]
    fn keeps_the_graphic_control_extension() {
        // Frame delay and transparency: presentation, not metadata.
        let control = extension(0xF9, &[0x04, 0x0A, 0x00, 0x00]);
        let input = gif(&[control]);
        assert_eq!(strip(&input).unwrap().data, input);
    }

    #[test]
    fn strips_data_appended_after_the_trailer() {
        let mut input = gif(&[]);
        input.extend_from_slice(b"appended payload");
        let result = strip(&input).unwrap();
        assert_eq!(result.data, gif(&[]));
        assert!(result.findings.iter().any(|f| f.label == "Trailing data"));
    }

    #[test]
    fn rejects_non_gif_and_survives_truncation() {
        assert!(strip(b"not a gif at all").is_err());
        let full = gif(&[extension(0xFE, b"note")]);
        for n in 0..full.len() {
            let _ = strip(&full[..n]); // must not panic
        }
    }
}
