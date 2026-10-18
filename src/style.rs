//! The palette, as a CSS provider.
//!
//! Colours are published twice: as libadwaita's `--*-color` custom properties,
//! which restyle the stock widgets (header bar, buttons, rows) without
//! touching them, and as `@define-color` names for the handful of rules in
//! this file that need them directly.
//!
//! **The app is light only.** The palette is the cream-and-cocoa one and there
//! is no dark variant; `ColorScheme::ForceLight` is set so libadwaita does not
//! hand the stock widgets a dark ground underneath these colours when the
//! desktop is set to dark.

use gtk::gdk::Display;
use gtk::CssProvider;

// Sage, Cream, Cocoa, Tan, plus the state and background extensions.
const SAGE: &str = "#7FA07A";
const SAGE_HOVER: &str = "#6E8C6A";
const SAGE_TINT: &str = "#DCE6DA";
const CREAM: &str = "#F4E7D3";
const CREAM_PAGE: &str = "#FBF5EA";
const COCOA: &str = "#4A3428";
const TAN: &str = "#8A6A50";
const CARD: &str = "#FFFFFF";

pub fn install() {
    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceLight);

    let provider = CssProvider::new();
    provider.load_from_string(&css());

    if let Some(display) = Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

fn css() -> String {
    // Cream on sage is decorative weight, so the accent's foreground is the
    // deep cocoa rather than a near-white.
    format!(
        r#"
:root {{
  --accent-bg-color: {SAGE};
  --accent-fg-color: {COCOA};
  --accent-color: {SAGE};
  --window-bg-color: {CREAM_PAGE};
  --window-fg-color: {COCOA};
  --view-bg-color: {CREAM_PAGE};
  --view-fg-color: {COCOA};
  --headerbar-bg-color: {CREAM_PAGE};
  --headerbar-fg-color: {COCOA};
  --card-bg-color: {CARD};
  --card-fg-color: {COCOA};
  --dialog-bg-color: {CREAM_PAGE};
  --dialog-fg-color: {COCOA};
  --popover-bg-color: {CARD};
  --popover-fg-color: {COCOA};
}}

@define-color rm_sage {SAGE};
@define-color rm_sage_hover {SAGE_HOVER};
@define-color rm_selected {SAGE_TINT};
@define-color rm_muted {TAN};
@define-color rm_cream {CREAM};
@define-color rm_cocoa {COCOA};

/* AdwStatusPage spaces itself for a full-screen empty state: libadwaita gives
   the outer box `margin: 36px 12px; border-spacing: 36px` and the icon another
   `margin-bottom: 24px` on top of the inner box's 12px spacing. That is 36px
   under the icon and 36 between the description and the button, which in a
   540px-tall window pushes the button below the fold and raises a scrollbar on
   a page that has nothing worth scrolling to.

   The gaps live on the *containers*, so they have to be overridden there — a
   margin on the clamp adds to the border-spacing instead of replacing it. */
statuspage > scrolledwindow > viewport > box {{
  margin: 24px 12px;
  border-spacing: 16px;
}}

statuspage > scrolledwindow > viewport > box > clamp > box > .icon:not(:last-child) {{
  margin-bottom: 4px;
}}

.finding-label {{
  font-weight: bold;
}}

.finding-detail {{
  color: @rm_muted;
}}

/* The drop target, drawn only while a drag is over the window. */
.drop-active {{
  background-color: @rm_selected;
}}

.file-card {{
  background-color: {CARD};
  border-radius: 12px;
  border: 1px solid alpha(@rm_muted, 0.35);
}}

.status-clean {{
  color: @rm_sage;
}}

.status-dirty {{
  color: {TAN};
}}
"#
    )
}
