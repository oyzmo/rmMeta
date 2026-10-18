//! The app's own symbolic icons.
//!
//! `build.rs` compiles `data/rmmeta.gresource.xml` into the binary, so there
//! is nothing to install and a source-tree run behaves exactly like the
//! Flatpak. `GtkIconTheme` treats a registered resource path as part of the
//! hicolor theme, which is why the bundle lays the file out as
//! `scalable/actions/NAME.svg` — after this, the icon is reachable by name
//! like any stock one.

use gtk::gdk;
use gtk::gio;

/// The empty state's icon: the app's mark, monochrome, recoloured by the theme.
pub const DROP: &str = "no.oyzmo.RmMeta-drop-symbolic";

pub fn install() {
    gio::resources_register_include!("rmmeta.gresource")
        .expect("the icon bundle is compiled into the binary by build.rs");

    // No display means no icon theme to add to; that only happens when GTK
    // could not open one, in which case the window is not coming either.
    if let Some(display) = gdk::Display::default() {
        gtk::IconTheme::for_display(&display).add_resource_path("/no/oyzmo/RmMeta/icons");
    }
}
