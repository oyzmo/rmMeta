
#[cfg(debug_assertions)]
mod devshot;
mod icons;
mod settings;
mod style;
mod ui;

use adw::prelude::*;

fn main() -> gtk::glib::ExitCode {
    let app = adw::Application::builder()
        .application_id("no.oyzmo.RmMeta")
        .build();

    app.connect_startup(|_| {
        icons::install();
        style::install();
    });
    app.connect_activate(ui::build);

    // The window takes files from the file chooser and from drag and drop, so
    // command-line arguments are none of GTK's business.
    app.run_with_args::<&str>(&[])
}
