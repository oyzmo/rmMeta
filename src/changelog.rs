//! The About dialog's release notes, read from the metainfo at compile time.
//!
//! `bump.sh` writes every release into the metainfo's `<releases>` list, which
//! is also what GNOME Software shows, so that list is the one changelog; the
//! dialog gets a copy of it rather than a second one to keep in step.

const METAINFO: &str = include_str!("../flatpak/no.oyzmo.RmMeta.metainfo.xml");

/// Every release, newest first, in the markup subset `AdwAboutDialog`
/// accepts (`p`, `ul`, `li`, `em`). A note `bump.sh` joined from several
/// with "; " becomes one bullet each.
pub fn release_notes() -> String {
    markup(METAINFO)
}

fn markup(metainfo: &str) -> String {
    let mut out = String::new();
    let mut rest = metainfo;
    while let Some(start) = rest.find("<release ") {
        rest = &rest[start..];
        let end = rest.find("</release>").unwrap_or(rest.len());
        let release = &rest[..end];
        rest = &rest[end..];

        let Some(version) = attribute(release, "version") else { continue };
        out.push_str("<p><em>");
        out.push_str(version);
        out.push_str("</em>");
        if let Some(date) = attribute(release, "date") {
            out.push_str(" · ");
            out.push_str(date);
        }
        out.push_str("</p><ul>");
        for paragraph in paragraphs(release) {
            for note in paragraph.split("; ").map(str::trim).filter(|n| !n.is_empty()) {
                out.push_str("<li>");
                out.push_str(note);
                out.push_str("</li>");
            }
        }
        out.push_str("</ul>");
    }
    out
}

fn attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let open = &tag[..tag.find('>')?];
    let key = format!(" {name}=\"");
    let start = open.find(&key)? + key.len();
    let len = open[start..].find('"')?;
    Some(&open[start..start + len])
}

/// The text of each `<p>`, whitespace collapsed. It is still escaped as
/// the metainfo has it, which is also what the dialog's markup expects.
fn paragraphs(release: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = release;
    while let Some(start) = rest.find("<p>") {
        rest = &rest[start + 3..];
        let end = rest.find("</p>").unwrap_or(rest.len());
        found.push(rest[..end].split_whitespace().collect::<Vec<_>>().join(" "));
        rest = &rest[end..];
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_releases_newest_first_with_joined_notes_split() {
        let xml = r#"<releases>
    <release version="0.2.0" date="2026-02-02">
      <description>
        <p>Faster; R&amp;D
           kept</p>
      </description>
    </release>
    <release version="0.1.0" date="2026-01-01">
      <description><p>First</p></description>
    </release>
  </releases>"#;
        assert_eq!(
            markup(xml),
            "<p><em>0.2.0</em> · 2026-02-02</p><ul><li>Faster</li><li>R&amp;D kept</li></ul>\
             <p><em>0.1.0</em> · 2026-01-01</p><ul><li>First</li></ul>"
        );
    }

    #[test]
    fn the_real_metainfo_starts_with_this_version() {
        let notes = release_notes();
        let current = format!("<p><em>{}</em>", env!("CARGO_PKG_VERSION"));
        assert!(notes.starts_with(&current), "{}", &notes[..80.min(notes.len())]);
    }
}
