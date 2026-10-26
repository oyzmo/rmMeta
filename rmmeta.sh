#!/usr/bin/env bash
# rmMeta helper menu: install build tools, compile, install, uninstall, make a
# distribution, clean up, and the release steps (bump, flatpak.oyzmo.no,
# GitHub). Same numbers and order as Bindle's bindle.sh.
set -u

APP_ID="no.oyzmo.RmMeta"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# The checkout sits on a share that is unreliable for parallel compiler
# writes, so build on local disk.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/rmmeta/build}"
FLATPAK_SITE="${FLATPAK_SITE:-/mnt/mycode/flatpak}"

BIN_DIR="$HOME/.local/bin"
DESKTOP_DIR="$HOME/.local/share/applications"
ICON_DIR="$HOME/.local/share/icons/hicolor/scalable/apps"
META_DIR="$HOME/.local/share/metainfo"
# Same layout the Flatpak uses: the GPL wants its text to travel with the binary.
LICENSE_DIR="$HOME/.local/share/licenses/$APP_ID"

bold() { printf '\033[1m%s\033[0m\n' "$*"; }
ok()   { printf '\033[32m✔ %s\033[0m\n' "$*"; }
err()  { printf '\033[31m✘ %s\033[0m\n' "$*"; }

current_version() {
    sed -n 's/^version = "\(.*\)"/\1/p' "$SCRIPT_DIR/Cargo.toml" | head -1
}

install_tools() {
    bold "Installing build tools…"
    if command -v dnf >/dev/null; then
        sudo dnf install -y cargo rust gcc pkgconf-pkg-config gtk4-devel libadwaita-devel glib2-devel zip
    elif command -v apt >/dev/null; then
        sudo apt update && sudo apt install -y cargo rustc gcc pkg-config libgtk-4-dev libadwaita-1-dev libglib2.0-dev-bin zip
    elif command -v pacman >/dev/null; then
        sudo pacman -S --needed rust gcc pkgconf gtk4 libadwaita zip
    else
        err "Unknown package manager — install Rust, GTK 4 and libadwaita development files yourself."
        return 1
    fi
}

compile_app() {
    bold "Compiling rmMeta $(current_version) (release)…"
    cargo build --release --manifest-path "$SCRIPT_DIR/Cargo.toml" && ok "Built $CARGO_TARGET_DIR/release/rmmeta"
}

install_app() {
    compile_app || return 1
    bold "Installing to ~/.local…"
    install -Dm755 "$CARGO_TARGET_DIR/release/rmmeta" "$BIN_DIR/rmmeta" &&
    install -Dm644 "$SCRIPT_DIR/flatpak/$APP_ID.desktop" "$DESKTOP_DIR/$APP_ID.desktop" &&
    install -Dm644 "$SCRIPT_DIR/flatpak/$APP_ID.svg" "$ICON_DIR/$APP_ID.svg" &&
    install -Dm644 "$SCRIPT_DIR/flatpak/$APP_ID.metainfo.xml" "$META_DIR/$APP_ID.metainfo.xml" &&
    install -Dm644 "$SCRIPT_DIR/COPYING" "$LICENSE_DIR/COPYING" || { err "Install failed."; return 1; }
    command -v update-desktop-database >/dev/null && update-desktop-database "$DESKTOP_DIR" 2>/dev/null
    command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -qtf "$HOME/.local/share/icons/hicolor" 2>/dev/null
    ok "Installed. rmMeta is in the app grid; the command is $BIN_DIR/rmmeta."
}

uninstall_app() {
    bold "Uninstalling rmMeta…"
    rm -f "$BIN_DIR/rmmeta" "$DESKTOP_DIR/$APP_ID.desktop" "$ICON_DIR/$APP_ID.svg" \
          "$META_DIR/$APP_ID.metainfo.xml" "$LICENSE_DIR/COPYING"
    rmdir "$LICENSE_DIR" 2>/dev/null
    command -v update-desktop-database >/dev/null && update-desktop-database "$DESKTOP_DIR" 2>/dev/null
    ok "Removed binary, desktop entry, icon and metainfo."
}

# Everything a release consists of, in dist/: the source zip and the Flatpak
# bundle (makeflatpak.sh also publishes into the Flatpak site).
make_distribution() {
    bold "Making distribution for rmMeta $(current_version) → dist/"
    "$SCRIPT_DIR/source.sh" || { err "The source zip failed."; return 1; }
    "$SCRIPT_DIR/makeflatpak.sh" || { err "The flatpak failed."; return 1; }
    echo
    bold "dist/ now holds:"
    ls -lh "$SCRIPT_DIR/dist" | tail -n +2 | sed 's/^/  /'
}

cleanup_build() {
    # build/ is root-owned in places (an old sudo build), so say if it stays.
    rm -rf "$SCRIPT_DIR/build" "$SCRIPT_DIR/.flatpak-builder" "$CARGO_TARGET_DIR" 2>/dev/null
    if [ -e "$SCRIPT_DIR/build" ]; then
        err "Parts of build/ are owned by root: sudo rm -rf $SCRIPT_DIR/build"
    else
        ok "Removed build/, .flatpak-builder/ and the cargo build. dist/ is kept."
    fi
}

# Swap an installed rmMeta flatpak for the newest bundle in dist/. GNOME
# Software only offers "Open" for a bundle of an app that is already
# installed. Uninstalling keeps the app's data (~/.var/app/$APP_ID).
reinstall_flatpak() {
    command -v flatpak >/dev/null || { err "flatpak not found."; return 1; }
    local bundle scope found=() version
    bundle="$(ls "$SCRIPT_DIR"/dist/RmMeta-*.flatpak 2>/dev/null | sort -V | tail -1)"
    [ -n "$bundle" ] || { err "No RmMeta-*.flatpak in dist/ — make one with option 7 first."; return 1; }
    for scope in user system; do
        if flatpak info --"$scope" "$APP_ID" >/dev/null 2>&1; then
            version="$(flatpak info --"$scope" "$APP_ID" | sed -n 's/^ *Version: //p')"
            found+=("$scope")
            echo "  Installed ($scope): ${version:-unknown version}"
        fi
    done
    [ ${#found[@]} -eq 0 ] && echo "  Not installed yet."
    echo "  New:  $(basename "$bundle")"
    read -rp "Uninstall the old and install this one? [y/N] " answer
    [[ "$answer" == [yY]* ]] || { echo "  Cancelled."; return 0; }
    for scope in ${found[@]+"${found[@]}"}; do
        flatpak uninstall --"$scope" -y --noninteractive "$APP_ID" || { err "Uninstall failed."; return 1; }
    done
    scope=user
    [ ${#found[@]} -eq 1 ] && scope="${found[0]}"
    if flatpak install --"$scope" -y --noninteractive "$bundle"; then
        ok "Installed $(flatpak info --"$scope" "$APP_ID" | sed -n 's/^ *Version: //p')."
    else
        err "Install failed."
        return 1
    fi
}

# bump.sh plus a commit of just the files it touches, so the release steps
# after it (which clone main) see the new version. bump.sh takes one note;
# several become one line, as the earlier releases read.
bump_version() {
    local level notes=() note joined
    echo "  Now at $(current_version)."
    read -rp "Bump which part? [p]atch (fixes) / [m]inor (features) / ma[j]or: " level
    case "$level" in
        p|P|patch) level=patch ;;
        m|M|minor) level=minor ;;
        j|J|major) level=major ;;
        *) echo "  Cancelled."; return 0 ;;
    esac
    echo "  What changed? One short line per note; an empty line ends."
    while read -rp "  note: " note && [ -n "$note" ]; do
        notes+=("$note")
    done
    [ ${#notes[@]} -gt 0 ] || { err "At least one note is needed."; return 1; }
    joined="$(printf '%s; ' "${notes[@]}")"
    "$SCRIPT_DIR/bump.sh" "$level" -m "${joined%; }" || return 1
    git -C "$SCRIPT_DIR" commit -q -m "rmMeta $(current_version)" -- \
        Cargo.toml Cargo.lock "flatpak/$APP_ID.metainfo.xml" \
        && ok "Committed rmMeta $(current_version)."
}

publish_flatpak() {
    [ -x "$FLATPAK_SITE/tools/push.sh" ] || { err "No Flatpak site at $FLATPAK_SITE."; return 1; }
    "$FLATPAK_SITE/tools/push.sh"
}

release_github() {
    bold "Pushing main to GitHub…"
    "$SCRIPT_DIR/dev/push-github.sh" || { err "Push failed — nothing released."; return 1; }
    "$SCRIPT_DIR/dev/release-github.sh"
}

# Everything after the bump: dist/, NAS backup, flatpak.oyzmo.no, GitHub.
full_release() {
    if ! git -C "$SCRIPT_DIR" diff --quiet HEAD; then
        err "There are uncommitted changes — commit them first, a release is built from main."
        return 1
    fi
    bold "Full release of rmMeta $(current_version)"
    make_distribution || { err "Stopped: the distribution failed."; return 1; }
    bold "Backing up to the NAS…"
    git -C "$SCRIPT_DIR" push -q nas main || err "NAS push failed — continuing."
    publish_flatpak || err "flatpak.oyzmo.no was not updated."
    release_github || err "The GitHub release failed (see above)."
}

backup_key() {
    "$FLATPAK_SITE/tools/backup-key.sh"
}

while true; do
    echo
    bold "── rmMeta $(current_version) ────────────────────────"
    echo "  1) Install tools needed to compile"
    echo "  2) Compile"
    echo "  3) Install on this GNOME desktop"
    echo "  4) Uninstall"
    echo "  8) Cleanup (delete temporary build files)"
    echo " 10) Replace installed flatpak with the newest one (from dist/)"
    bold "── Release (in this order) ──────────────"
    echo " 11) Bump version (asks for notes, commits)"
    echo " 13) Full release: 7 → NAS → 12 → 9 in one go"
    echo "     or step by step:"
    echo "  7)   Make distribution (source zip + flatpak → dist/ and the Flatpak site)"
    echo " 12)   Publish flatpak updates (flatpak.oyzmo.no → GNOME Software)"
    echo "  9)   Release newest version on GitHub (from dist/)"
    echo " 14) Back up the flatpak signing key to the NAS (after the key changes)"
    echo "  q) Quit"
    read -rp "Choose: " choice
    case "$choice" in
        1) install_tools ;;
        2) compile_app ;;
        3) install_app ;;
        4) uninstall_app ;;
        7) make_distribution ;;
        8) cleanup_build ;;
        9) release_github ;;
        10) reinstall_flatpak ;;
        11) bump_version ;;
        12) publish_flatpak ;;
        13) full_release ;;
        14) backup_key ;;
        q|Q) exit 0 ;;
        *) err "Unknown choice." ;;
    esac
done
