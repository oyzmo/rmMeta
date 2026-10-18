//! Runs the engine over files written by other software — Pillow's Exif and
//! PNG text chunks, LibreOffice's office packages.
//!
//! Generate the fixtures first:
//!
//!     python3 dev/make_fixtures.py
//!
//! Cleaned copies are left in `dev/out/` so `dev/check.py` can hold them up
//! against exiv2 and LibreOffice, which know nothing about this code.

use std::fs;
use std::path::PathBuf;

use rmmeta::meta;

fn dir(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("dev").join(name)
}

/// The fixtures are generated, not committed, so a checkout without them skips
/// rather than fails.
fn fixtures() -> Option<Vec<PathBuf>> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir("fixtures"))
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file())
        .collect();
    paths.sort();
    if paths.is_empty() {
        return None;
    }
    Some(paths)
}

#[test]
fn every_fixture_is_cleaned_and_the_result_is_already_clean() {
    let Some(paths) = fixtures() else {
        eprintln!("skipped: run `python3 dev/make_fixtures.py` to generate fixtures");
        return;
    };
    let out = dir("out");
    fs::create_dir_all(&out).expect("create dev/out");

    for path in &paths {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let data = fs::read(path).expect("read fixture");

        let found = meta::examine(&data).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(!found.is_clean(), "{name}: real file had no metadata to remove");

        // Cleaning is idempotent: a second pass must find nothing left. This is
        // the check that catches metadata hiding somewhere the stripper walked
        // past the first time.
        let again = meta::examine(&found.cleaned).unwrap_or_else(|e| panic!("{name} (again): {e}"));
        assert!(again.is_clean(), "{name}: still holds {:?}", again.findings);

        fs::write(out.join(&name), &found.cleaned).expect("write dev/out");
        println!("{name}: removed {} items", found.findings.len());
    }
}

#[test]
fn the_photo_gives_up_its_camera_and_location_but_keeps_which_way_is_up() {
    let Some(paths) = fixtures() else { return };
    let Some(photo) = paths.iter().find(|p| p.extension().is_some_and(|e| e == "jpg")) else {
        return;
    };

    let found = meta::examine(&fs::read(photo).unwrap()).unwrap();
    let report: Vec<String> = found
        .findings
        .iter()
        .map(|f| format!("{}: {}", f.label, f.detail))
        .collect();
    let joined = report.join("\n");

    for expected in ["Canon", "59.91", "10.75", "SN-12345678", "Jane Doe"] {
        assert!(joined.contains(expected), "expected {expected} in:\n{joined}");
    }
    assert!(joined.contains("GPS location"), "{joined}");

    // The rebuilt Exif block carries the orientation and nothing else, so the
    // photo still displays the right way round.
    let tiff_at = found.cleaned.windows(6).position(|w| w == b"Exif\0\0").expect("orientation kept")
        + 6;
    let kept = meta::exif::analyse(&found.cleaned[tiff_at..]);
    assert_eq!(kept.orientation, Some(6));
    assert!(kept.findings.is_empty(), "{:?}", kept.findings);
}
