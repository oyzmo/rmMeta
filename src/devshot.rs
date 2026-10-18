//! Development-only window capture.
//!
//! GNOME refuses non-interactive screenshots, so the window paints itself:
//! GTK already has a renderer attached to the surface, and a `WidgetPaintable`
//! hands it the whole window as a render node. Set `RMMETA_DEV_SHOT` to a path
//! and the app writes that PNG and quits.
//!
//! The whole module is behind `debug_assertions`, so none of it exists in the
//! release build that goes into the Flatpak.

use adw::prelude::*;
use gtk::glib;
use std::time::Duration;

pub fn capture_then_quit(window: &adw::ApplicationWindow, path: String) {
    let window = window.clone();
    // Long enough for the first frame and any icon loading to settle.
    glib::timeout_add_local_once(Duration::from_millis(1200), move || {
        match capture(&window, &path) {
            Ok(()) => eprintln!("devshot: wrote {path}"),
            Err(e) => eprintln!("devshot: {e}"),
        }
        window.close();
    });
}

fn capture(window: &adw::ApplicationWindow, path: &str) -> Result<(), String> {
    let paintable = gtk::WidgetPaintable::new(Some(window));
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, window.width() as f64, window.height() as f64);

    let node = snapshot.to_node().ok_or("the window produced an empty render node")?;
    let renderer = window
        .native()
        .and_then(|native| native.renderer())
        .ok_or("the window has no renderer yet")?;

    renderer
        .render_texture(&node, None)
        .save_to_png(path)
        .map_err(|e| format!("could not save {path}: {e}"))
}
