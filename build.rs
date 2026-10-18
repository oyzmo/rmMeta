//! Compiles the icon resource bundle.
//!
//! This calls `glib-compile-resources` directly rather than pulling in the
//! `glib-build-tools` crate: that crate does exactly this and nothing more,
//! and every dependency added here has to be transcribed into
//! `generated-sources.json` for the offline Flatpak build. The binary lives in
//! glib2-devel on Fedora and in the GNOME SDK, both of which are already
//! required to build at all.

use std::path::PathBuf;
use std::process::Command;

const SOURCE_DIR: &str = "data/icons/scalable/actions";
const MANIFEST: &str = "data/rmmeta.gresource.xml";

fn main() {
    println!("cargo:rerun-if-changed={MANIFEST}");
    println!("cargo:rerun-if-changed={SOURCE_DIR}");

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR"))
        .join("rmmeta.gresource");

    let status = Command::new("glib-compile-resources")
        .arg("--sourcedir")
        .arg(SOURCE_DIR)
        .arg("--target")
        .arg(&out)
        .arg(MANIFEST)
        .status();

    match status {
        Ok(s) if s.success() => {}
        Ok(s) => panic!("glib-compile-resources failed ({s}) on {MANIFEST}"),
        Err(e) => panic!(
            "could not run glib-compile-resources ({e}). It ships in glib2-devel \
             on Fedora: sudo dnf install glib2-devel"
        ),
    }
}
