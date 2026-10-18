//! The window.
//!
//! Files are examined the moment they arrive and the findings are listed
//! before anything is written, so the user sees what is in a file — and
//! decides — rather than being told afterwards that something was removed.
//!
//! Auto mode, from the menu, trades that for speed: files are cleaned the
//! moment they are dropped. What came out stays listed on the cleaned row, so
//! nothing is removed without the user being able to see it.

use std::cell::{Cell, RefCell};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use gtk::gdk;
use gtk::gio;
use gtk::glib;

use rmmeta::meta::{self, Examined, Finding};

use crate::settings::{self, Settings};

const APP_ID: &str = "no.oyzmo.RmMeta";

enum Outcome {
    Ready(Examined),
    Failed(String),
    /// Written to `to`, and what was taken out and left in on the way.
    Written { to: PathBuf, removed: Vec<Finding>, kept: Vec<Finding> },
}

struct Entry {
    path: PathBuf,
    outcome: Outcome,
    /// Size and modification time when the file was examined. The cleaned
    /// bytes are computed from that version; if the file has changed since,
    /// writing them would undo the change.
    stamp: Option<(u64, std::time::SystemTime)>,
}

/// Where cleaned files go.
enum Target {
    /// Over the original.
    Replace,
    /// `name-clean.ext` beside the original.
    Beside,
    /// `name-clean.ext` in a folder the user chose.
    Into(PathBuf),
}

fn stamp(path: &Path) -> Option<(u64, std::time::SystemTime)> {
    let metadata = fs::metadata(path).ok()?;
    Some((metadata.len(), metadata.modified().ok()?))
}

/// Inside the Flatpak sandbox a picked file arrives as a path in the document
/// portal, a directory holding that one file. A new file created next to it
/// does not reach the real folder; the portal only passes through a write
/// over the file itself. A cleaned copy therefore needs a folder the user
/// grants.
fn sandboxed() -> bool {
    Path::new("/.flatpak-info").exists()
}

fn examine(path: &Path, settings: Settings) -> Outcome {
    let options = meta::Options { keep_maker_notes: settings.keep_maker_notes };
    match fs::read(path) {
        Ok(data) => match meta::examine_with(&data, &options) {
            Ok(found) => Outcome::Ready(found),
            Err(e) => Outcome::Failed(e),
        },
        Err(e) => Outcome::Failed(format!("could not read the file: {e}")),
    }
}

struct Ui {
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    stack: gtk::Stack,
    list: gtk::Box,
    replace: gtk::Switch,
    clean: gtk::Button,
    empty: adw::StatusPage,
    entries: RefCell<Vec<Entry>>,
    settings: Cell<Settings>,
    /// The folder cleaned copies went to inside the sandbox, remembered for
    /// the session so auto mode does not ask on every drop.
    copy_folder: RefCell<Option<PathBuf>>,
}

impl Ui {
    fn settings(&self) -> Settings {
        self.settings.get()
    }
}

pub fn build(app: &adw::Application) {
    let toasts = adw::ToastOverlay::new();
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("rmMeta")
        .default_width(620)
        .default_height(555)
        .content(&toasts)
        .build();

    let ui = Rc::new(Ui {
        window: window.clone(),
        toasts: toasts.clone(),
        stack: gtk::Stack::new(),
        list: gtk::Box::new(gtk::Orientation::Vertical, 0),
        replace: gtk::Switch::new(),
        clean: gtk::Button::with_label("Remove Metadata"),
        empty: adw::StatusPage::new(),
        entries: RefCell::new(Vec::new()),
        settings: Cell::new(settings::load()),
        copy_folder: RefCell::new(None),
    });

    let view = adw::ToolbarView::new();
    view.add_top_bar(&header(&ui));
    view.set_content(Some(&ui.stack));
    view.add_bottom_bar(&bottom_bar(&ui));
    toasts.set_child(Some(&view));

    ui.stack.add_named(&empty_page(&ui), Some("empty"));
    ui.stack.add_named(&list_page(&ui), Some("list"));

    install_drop_target(&ui, &toasts);
    refresh(&ui);

    // Development seam: preload files so the populated window can be checked
    // without driving a file chooser. Compiled out of release builds.
    #[cfg(debug_assertions)]
    if let Ok(list) = std::env::var("RMMETA_DEV_FILES") {
        add_files(&ui, list.split(':').map(PathBuf::from).collect());
    }

    window.present();

    #[cfg(debug_assertions)]
    if let Ok(path) = std::env::var("RMMETA_DEV_SHOT") {
        crate::devshot::capture_then_quit(&window, path);
    }
}

fn header(ui: &Rc<Ui>) -> adw::HeaderBar {
    let header = adw::HeaderBar::new();

    let add = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Add files")
        .build();
    add.connect_clicked({
        let ui = ui.clone();
        move |_| choose_files(&ui)
    });
    header.pack_start(&add);

    // Stateful boolean actions render as check items.
    let options = gio::Menu::new();
    options.append(Some("Clean Automatically"), Some("win.auto-clean"));
    options.append(Some("Keep Maker Notes in RAW Files"), Some("win.keep-maker-notes"));
    let general = gio::Menu::new();
    general.append(Some("Clear List"), Some("win.clear"));
    general.append(Some("About rmMeta"), Some("win.about"));
    let menu = gio::Menu::new();
    menu.append_section(None, &options);
    menu.append_section(None, &general);
    header.pack_end(
        &gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .menu_model(&menu)
            .tooltip_text("Main menu")
            .build(),
    );

    let clear = gio::SimpleAction::new("clear", None);
    clear.connect_activate({
        let ui = ui.clone();
        move |_, _| {
            ui.entries.borrow_mut().clear();
            refresh(&ui);
        }
    });
    let about = gio::SimpleAction::new("about", None);
    about.connect_activate({
        let ui = ui.clone();
        move |_, _| show_about(&ui)
    });
    ui.window.add_action(&clear);
    ui.window.add_action(&about);

    ui.window.add_action(&toggle(ui, "auto-clean", |s| &mut s.auto_clean, |ui| {
        // Only new drops are cleaned automatically. Files already listed were
        // added under the old rule; the button is still there for them.
        refresh(ui);
    }));
    ui.window.add_action(&toggle(ui, "keep-maker-notes", |s| &mut s.keep_maker_notes, |ui| {
        // The findings depend on it, so what is listed is examined again.
        let settings = ui.settings();
        for entry in ui.entries.borrow_mut().iter_mut() {
            if matches!(entry.outcome, Outcome::Ready(_)) {
                entry.outcome = examine(&entry.path, settings);
                entry.stamp = stamp(&entry.path);
            }
        }
        refresh(ui);
    }));

    header
}

/// A check item in the menu, backed by one field of the saved settings.
fn toggle(
    ui: &Rc<Ui>,
    name: &str,
    field: fn(&mut Settings) -> &mut bool,
    changed: fn(&Rc<Ui>),
) -> gio::SimpleAction {
    let mut settings = ui.settings();
    let action = gio::SimpleAction::new_stateful(name, None, &(*field(&mut settings)).to_variant());
    action.connect_activate({
        let ui = ui.clone();
        move |action, _| {
            let mut settings = ui.settings();
            let value = field(&mut settings);
            *value = !*value;
            action.set_state(&(*value).to_variant());
            ui.settings.set(settings);
            settings::save(settings);
            changed(&ui);
        }
    });
    action
}

fn bottom_bar(ui: &Rc<Ui>) -> gtk::Box {
    let bar = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .margin_top(10)
        .margin_bottom(10)
        .margin_start(14)
        .margin_end(14)
        .build();

    let version = gtk::Label::builder()
        .label(format!("v{}", env!("CARGO_PKG_VERSION")))
        .css_classes(["dim-label", "caption"])
        .build();
    bar.append(&version);

    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    bar.append(&spacer);

    ui.replace.set_valign(gtk::Align::Center);
    ui.replace.set_tooltip_text(Some(
        "Off: write a cleaned copy beside the original. On: overwrite the original.",
    ));
    let replace_label = gtk::Label::new(Some("Replace originals"));
    bar.append(&replace_label);
    bar.append(&ui.replace);

    ui.clean.add_css_class("suggested-action");
    ui.clean.add_css_class("pill");
    ui.clean.connect_clicked({
        let ui = ui.clone();
        move |_| clean(&ui, None)
    });
    bar.append(&ui.clean);

    bar
}

fn empty_page(ui: &Rc<Ui>) -> adw::StatusPage {
    let button = gtk::Button::builder()
        .label("Add Files\u{2026}")
        .halign(gtk::Align::Center)
        .css_classes(["suggested-action", "pill"])
        .build();
    button.connect_clicked({
        let ui = ui.clone();
        move |_| choose_files(&ui)
    });

    ui.empty.set_icon_name(Some(crate::icons::DROP));
    ui.empty.set_description(Some(
        "Photos and documents carry hidden data \u{2014} location, camera, author, dates. \
         Nothing is uploaded; files are examined on this computer.",
    ));
    ui.empty.set_child(Some(&button));
    ui.empty.clone()
}

fn list_page(ui: &Rc<Ui>) -> gtk::ScrolledWindow {
    ui.list.set_margin_top(18);
    ui.list.set_margin_bottom(18);
    ui.list.set_margin_start(18);
    ui.list.set_margin_end(18);

    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&ui.list)
        .build()
}

fn install_drop_target(ui: &Rc<Ui>, on: &adw::ToastOverlay) {
    let target = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
    target.connect_drop({
        let ui = ui.clone();
        move |_, value, _, _| {
            let Ok(files) = value.get::<gdk::FileList>() else {
                return false;
            };
            add_files(&ui, files.files().iter().filter_map(|f| f.path()).collect());
            true
        }
    });
    on.add_controller(target);
}

fn choose_files(ui: &Rc<Ui>) {
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("Supported files"));
    for mime in [
        "image/jpeg",
        "image/png",
        "application/pdf",
        "image/gif",
        "image/webp",
        "image/tiff",
        "image/heif",
        "image/heic",
        "image/avif",
        "image/svg+xml",
        "image/x-canon-cr2",
        "image/x-nikon-nef",
        "image/x-sony-arw",
        "image/x-adobe-dng",
        "application/msword",
        "application/vnd.ms-excel",
        "application/vnd.ms-powerpoint",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "application/vnd.oasis.opendocument.text",
        "application/vnd.oasis.opendocument.spreadsheet",
        "application/vnd.oasis.opendocument.presentation",
    ] {
        filter.add_mime_type(mime);
    }
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);
    let all = gtk::FileFilter::new();
    all.set_name(Some("All files"));
    all.add_pattern("*");
    filters.append(&all);

    let dialog = gtk::FileDialog::builder()
        .title("Add Files")
        .filters(&filters)
        .modal(true)
        .build();

    dialog.open_multiple(Some(&ui.window), gio::Cancellable::NONE, {
        let ui = ui.clone();
        move |result| {
            let Ok(model) = result else { return };
            let paths = model
                .into_iter()
                .flatten()
                .filter_map(|obj| obj.downcast::<gio::File>().ok()?.path())
                .collect();
            add_files(&ui, paths);
        }
    });
}

fn add_files(ui: &Rc<Ui>, paths: Vec<PathBuf>) {
    let settings = ui.settings();
    let mut added = Vec::new();
    {
        let mut entries = ui.entries.borrow_mut();
        for path in paths {
            let existing = entries.iter().position(|e| e.path == path);
            // Already listed and waiting: nothing new to show.
            if existing.is_some_and(|i| matches!(entries[i].outcome, Outcome::Ready(_))) {
                continue;
            }
            let entry = Entry { stamp: stamp(&path), outcome: examine(&path, settings), path: path.clone() };
            match existing {
                // Cleaned or failed before: adding it again looks afresh.
                Some(i) => entries[i] = entry,
                None => entries.push(entry),
            }
            added.push(path);
        }
    }
    refresh(ui);

    // Auto mode cleans what was just added, and only that: files listed
    // before it was switched on are still waiting for the user's decision.
    if settings.auto_clean && !added.is_empty() {
        clean(ui, Some(added));
    }
}

fn refresh(ui: &Rc<Ui>) {
    while let Some(child) = ui.list.first_child() {
        ui.list.remove(&child);
    }

    let entries = ui.entries.borrow();
    ui.stack
        .set_visible_child_name(if entries.is_empty() { "empty" } else { "list" });
    ui.empty.set_title(if ui.settings().auto_clean {
        "Drop Files to Clean Them"
    } else {
        "Drop Files Here"
    });

    let group = adw::PreferencesGroup::new();
    // A single file is the whole point of the window, so show what is in it
    // without making the user click.
    let only_one = entries.len() == 1;
    let pending = entries
        .iter()
        .filter(|e| matches!(&e.outcome, Outcome::Ready(f) if !f.is_clean()))
        .count();

    for entry in entries.iter() {
        group.add(&row_for(entry, only_one));
    }
    ui.list.append(&group);

    ui.clean.set_sensitive(pending > 0);
    ui.clean.set_label(&match pending {
        0 => "Remove Metadata".to_string(),
        1 => "Remove Metadata from 1 File".to_string(),
        n => format!("Remove Metadata from {n} Files"),
    });
}

/// A file with findings gets an expander listing them; anything else gets a
/// plain row, so a chevron never appears on something that will not open.
fn row_for(entry: &Entry, expanded: bool) -> gtk::Widget {
    let name = entry
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| entry.path.display().to_string());

    let (icon, subtitle, findings, kept) = match &entry.outcome {
        Outcome::Failed(why) => ("dialog-warning-symbolic", why.clone(), &[][..], &[][..]),
        // Still listing what came out, so a file cleaned automatically can be
        // checked afterwards.
        Outcome::Written { to, removed, kept } => (
            "object-select-symbolic",
            format!(
                "Cleaned \u{2192} {} \u{2014} {} removed",
                to.file_name().unwrap_or_default().to_string_lossy(),
                items(removed.len()),
            ),
            &removed[..],
            &kept[..],
        ),
        Outcome::Ready(found) if found.is_clean() => (
            "object-select-symbolic",
            format!("{} \u{2014} nothing to remove", found.kind.describe()),
            &[][..],
            &found.kept[..],
        ),
        Outcome::Ready(found) => {
            let mut subtitle =
                format!("{} \u{2014} {} of metadata", found.kind.describe(), items(found.findings.len()));
            // Only worth mentioning when it is a visible saving; blanking a
            // document's properties shaves off bytes, not kilobytes.
            if found.bytes_saved() >= 1024 {
                subtitle.push_str(&format!(", {} smaller", human_size(found.bytes_saved())));
            }
            ("dialog-information-symbolic", subtitle, &found.findings[..], &found.kept[..])
        }
    };

    let title = glib::markup_escape_text(&name);
    let subtitle = glib::markup_escape_text(&subtitle);

    if findings.is_empty() && kept.is_empty() {
        let row = adw::ActionRow::builder().title(title).subtitle(subtitle).build();
        row.set_subtitle_lines(0);
        row.add_prefix(&gtk::Image::from_icon_name(icon));
        return row.upcast();
    }

    let row = adw::ExpanderRow::builder().title(title).subtitle(subtitle).build();
    row.add_prefix(&gtk::Image::from_icon_name(icon));
    row.set_expanded(expanded);
    for finding in findings {
        row.add_row(&detail_row(finding, None));
    }
    // What could not be removed, and why. Listed after the findings and marked
    // differently, so a limitation is never mistaken for a removal.
    for note in kept {
        row.add_row(&detail_row(note, Some("dialog-warning-symbolic")));
    }
    row.upcast()
}

fn detail_row(finding: &meta::Finding, warning: Option<&str>) -> adw::ActionRow {
    let title = match warning {
        Some(_) => format!("Kept: {}", finding.label),
        None => finding.label.clone(),
    };
    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(&title))
        .subtitle(glib::markup_escape_text(&finding.detail))
        .build();
    row.set_subtitle_lines(0);
    if let Some(icon) = warning {
        row.add_prefix(&gtk::Image::from_icon_name(icon));
    }
    row
}

fn items(n: usize) -> String {
    format!("{n} item{}", plural(n))
}

/// Cleans every waiting file, or with `only`, just those paths.
fn clean(ui: &Rc<Ui>, only: Option<Vec<PathBuf>>) {
    let waiting = ui
        .entries
        .borrow()
        .iter()
        .filter(|e| only.as_ref().is_none_or(|paths| paths.contains(&e.path)))
        .filter(|e| matches!(&e.outcome, Outcome::Ready(f) if !f.is_clean()))
        .count();
    if waiting == 0 {
        // Said rather than silent: in auto mode this is the only answer a
        // dropped file that was already clean gets.
        ui.toasts.add_toast(adw::Toast::new("Nothing to remove"));
        return;
    }

    if ui.replace.is_active() {
        write_all(ui, Target::Replace, only);
        return;
    }
    if !sandboxed() {
        write_all(ui, Target::Beside, only);
        return;
    }
    // Auto mode asks once per session; a click on the button asks each time,
    // since the user is there to choose.
    let remembered = ui.copy_folder.borrow().clone();
    if let Some(folder) = remembered.filter(|_| only.is_some()) {
        write_all(ui, Target::Into(folder), only);
        return;
    }
    let dialog = gtk::FileDialog::builder()
        .title("Save Cleaned Copies To")
        .accept_label("Save Here")
        .modal(true)
        .build();
    dialog.select_folder(Some(&ui.window), gio::Cancellable::NONE, {
        let ui = ui.clone();
        move |result| {
            if let Some(folder) = result.ok().and_then(|f| f.path()) {
                ui.copy_folder.replace(Some(folder.clone()));
                write_all(&ui, Target::Into(folder), only);
            }
        }
    });
}

fn write_all(ui: &Rc<Ui>, target: Target, only: Option<Vec<PathBuf>>) {
    let settings = ui.settings();
    let mut cleaned = 0usize;
    let mut changed = 0usize;
    let mut failed: Vec<String> = Vec::new();

    {
        let mut entries = ui.entries.borrow_mut();
        for entry in entries.iter_mut() {
            if only.as_ref().is_some_and(|paths| !paths.contains(&entry.path)) {
                continue;
            }
            let Outcome::Ready(found) = &entry.outcome else { continue };
            if found.is_clean() {
                continue;
            }
            // Edited since it was examined: the cleaned bytes belong to the
            // old version. Look again and let the user see the new findings
            // rather than overwrite the edit.
            let now = stamp(&entry.path);
            if now != entry.stamp {
                entry.outcome = examine(&entry.path, settings);
                entry.stamp = now;
                changed += 1;
                continue;
            }
            let destination = match &target {
                // Through a symbolic link to the file it names; renaming over
                // the link would replace the link and leave the file dirty.
                Target::Replace => fs::canonicalize(&entry.path).unwrap_or_else(|_| entry.path.clone()),
                Target::Beside => meta::copy_destination(&entry.path),
                Target::Into(folder) => meta::copy_destination_in(&entry.path, folder),
            };
            match write_file(&entry.path, &destination, &found.cleaned) {
                Ok(()) => {
                    cleaned += 1;
                    entry.outcome = Outcome::Written {
                        to: destination,
                        removed: found.findings.clone(),
                        kept: found.kept.clone(),
                    };
                }
                Err(e) => {
                    failed.push(format!("{}: {e}", entry.path.display()));
                    entry.outcome = Outcome::Failed(e);
                }
            }
        }
    }

    refresh(ui);

    let replace = matches!(target, Target::Replace);
    let mut message = match (cleaned, failed.len()) {
        (0, 0) if changed > 0 => String::new(),
        (0, 0) => "Nothing to remove".to_string(),
        (n, 0) if replace => format!("Cleaned {n} file{}", plural(n)),
        (n, 0) => format!("Wrote {n} cleaned cop{}", if n == 1 { "y" } else { "ies" }),
        (0, f) => format!("Failed on {f} file{}", plural(f)),
        (n, f) => format!("Cleaned {n}, failed on {f}"),
    };
    if changed > 0 {
        if !message.is_empty() {
            message.push_str(". ");
        }
        message.push_str(&format!(
            "{changed} file{} changed since {} added and {} examined again",
            plural(changed),
            if changed == 1 { "it was" } else { "they were" },
            if changed == 1 { "was" } else { "were" },
        ));
    }
    ui.toasts.add_toast(adw::Toast::new(&message));
}

fn human_size(bytes: usize) -> String {
    match bytes {
        b if b >= 1024 * 1024 => format!("{:.1} MB", b as f64 / (1024.0 * 1024.0)),
        b => format!("{} kB", b / 1024),
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Writes through a temporary file in the same directory and renames it into
/// place, so an interrupted write cannot leave a half-written file where the
/// original used to be.
fn write_file(source: &Path, destination: &Path, data: &[u8]) -> Result<(), String> {
    let parent = destination.parent().unwrap_or(Path::new("."));
    let temporary = parent.join(format!(
        ".{}.rmmeta-tmp",
        destination.file_name().unwrap_or_default().to_string_lossy()
    ));

    let write = || -> std::io::Result<()> {
        fs::write(&temporary, data)?;
        // Keep the original's permissions; a fresh temp file is created with
        // the process umask, which is usually stricter.
        if let Ok(metadata) = fs::metadata(source) {
            fs::set_permissions(&temporary, metadata.permissions())?;
        }
        fs::rename(&temporary, destination)
    };

    write().map_err(|e| {
        let _ = fs::remove_file(&temporary);
        format!("could not write {}: {e}", destination.display())
    })
}

fn show_about(ui: &Rc<Ui>) {
    adw::AboutDialog::builder()
        .application_name("rmMeta")
        .application_icon(APP_ID)
        .version(env!("CARGO_PKG_VERSION"))
        .comments(
            "Removes metadata that files record about you: camera, location and timestamps \
             in photos, author and company in office documents.",
        )
        .website("https://oyzmo.no/apps/")
        .license_type(gtk::License::Gpl30)
        .build()
        .present(Some(&ui.window));
}
