//! The menu's two toggles, remembered between runs.
//!
//! A key file in the user's config directory rather than GSettings: a GSettings
//! schema has to be compiled and installed before the app can start, which a
//! `cargo run` from the source tree does not do. Inside the Flatpak the config
//! directory is the app's own, so no permission is needed to write it.

use std::path::PathBuf;

use gtk::glib;

const GROUP: &str = "options";

#[derive(Clone, Copy, Debug)]
pub struct Settings {
    /// Clean files the moment they are added, without the button.
    pub auto_clean: bool,
    /// Leave maker notes in RAW files; see `meta::Options`.
    pub keep_maker_notes: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { auto_clean: false, keep_maker_notes: true }
    }
}

fn path() -> PathBuf {
    glib::user_config_dir().join("rmmeta").join("settings.ini")
}

pub fn load() -> Settings {
    let mut settings = Settings::default();
    let file = glib::KeyFile::new();
    if file.load_from_file(path(), glib::KeyFileFlags::NONE).is_ok() {
        if let Ok(v) = file.boolean(GROUP, "auto-clean") {
            settings.auto_clean = v;
        }
        if let Ok(v) = file.boolean(GROUP, "keep-maker-notes") {
            settings.keep_maker_notes = v;
        }
    }
    settings
}

/// Best effort: a setting that cannot be saved still applies to this run.
pub fn save(settings: Settings) {
    let path = path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let file = glib::KeyFile::new();
    file.set_boolean(GROUP, "auto-clean", settings.auto_clean);
    file.set_boolean(GROUP, "keep-maker-notes", settings.keep_maker_notes);
    let _ = file.save_to_file(path);
}
