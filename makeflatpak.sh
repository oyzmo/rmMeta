#!/usr/bin/env bash
#
# Build a Flatpak bundle of rmMeta.
#
# This BUILDS ONLY. Nothing is installed to your system, and the only
# repository outside ./build/ it touches is the oyzmo Flatpak site, when present
# (see the bundle step; publishing there stays local until rmmeta.sh pushes
# it). The result is dist/RmMeta-<version>.flatpak (version from Cargo.toml),
# which you can install later with:
#
#     flatpak install --user dist/RmMeta-<version>.flatpak
#
set -euo pipefail
cd "$(dirname "$0")"

APP_ID="no.oyzmo.RmMeta"
MANIFEST="flatpak/${APP_ID}.yml"
RUNTIME_VERSION="50"        # must match runtime-version: in the manifest
RUST_EXT_VERSION="25.08"    # freedesktop base of that GNOME runtime

BUILD_DIR="build/builddir"
REPO_DIR="build/repo"
STATE_DIR="build/state"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
[ -n "$VERSION" ] || { echo "error: no version in Cargo.toml" >&2; exit 1; }
OUT="dist/RmMeta-${VERSION}.flatpak"

say() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------- prerequisites
command -v flatpak >/dev/null || die "flatpak is not installed (sudo dnf install flatpak)"
command -v flatpak-builder >/dev/null \
    || die "flatpak-builder is not installed (sudo dnf install flatpak-builder)"
command -v python3 >/dev/null || die "python3 is not installed"

if ! flatpak remotes --columns=name | grep -qx flathub; then
    die "the flathub remote is not configured. Add it with:
    flatpak remote-add --user --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo"
fi

say "Checking runtimes"
MISSING=()
need_runtime() {
    if ! flatpak info "$1" "$2" >/dev/null 2>&1; then
        echo "    missing: $1 // $2"
        MISSING+=("$1//$2")
    fi
}
need_runtime org.gnome.Platform "$RUNTIME_VERSION"
need_runtime org.gnome.Sdk "$RUNTIME_VERSION"
need_runtime org.freedesktop.Sdk.Extension.rust-stable "$RUST_EXT_VERSION"

if [ ${#MISSING[@]} -gt 0 ]; then
    die "install the missing runtimes first:
    flatpak install --user flathub ${MISSING[*]}

If those versions do not exist any more, list what is available with
    flatpak remote-ls flathub --runtime | grep -E 'gnome.Platform|rust-stable'
then update RUNTIME_VERSION and RUST_EXT_VERSION at the top of this script and
runtime-version: in ${MANIFEST}."
fi

# ------------------------------------------------------------ manifest sanity
# Paths inside a flatpak manifest resolve relative to the manifest's own
# directory, so everything in flatpak/*.yml has to climb out of flatpak/ to
# reach the project. Getting this wrong builds an empty tree and fails much
# later with "manifest path `Cargo.toml` does not exist", so check it here.
MANIFEST_DIR="$(dirname "$MANIFEST")"
SRC_DIR=$(sed -n 's/^ *path: *\(.*[^ ]\) *$/\1/p' "$MANIFEST" | head -1)
[ -n "$SRC_DIR" ] || die "no source 'path:' found in ${MANIFEST}"
[ -f "${MANIFEST_DIR}/${SRC_DIR}/Cargo.toml" ] || die \
    "${MANIFEST} has 'path: ${SRC_DIR}', which resolves to ${MANIFEST_DIR}/${SRC_DIR} and holds no
Cargo.toml. Paths in the manifest are relative to ${MANIFEST_DIR}/, not to the project root."

# ------------------------------------------------------- vendored cargo sources
# Flatpak builds run offline, so every crate has to be declared up front.
# flatpak/cargo-sources.py turns Cargo.lock into that list. It is a stdlib-only
# stand-in for upstream's flatpak-cargo-generator.py, which needs aiohttp and
# toml purely to resolve git dependencies -- rmMeta has none, so the list is a
# straight transcription of Cargo.lock. The local script bails out loudly if a
# git dependency ever appears.
GEN="flatpak/cargo-sources.py"
if [ ! -f generated-sources.json ] \
   || [ Cargo.lock -nt generated-sources.json ] \
   || [ "$GEN" -nt generated-sources.json ]; then
    say "Generating vendored cargo sources from Cargo.lock"
    [ -f "$GEN" ] || die "$GEN is missing"
    python3 "$GEN" Cargo.lock -o generated-sources.json
else
    say "generated-sources.json is up to date"
fi

# The manifest names the generated file by a path of its own; make sure that
# one lands where the manifest expects it, not just where this script wrote it.
JSON_REF=$(sed -n 's/^ *- *\(.*generated-sources\.json\) *$/\1/p' "$MANIFEST" | head -1)
if [ -n "$JSON_REF" ] && [ ! -f "${MANIFEST_DIR}/${JSON_REF}" ]; then
    die "${MANIFEST} references '${JSON_REF}', which resolves to ${MANIFEST_DIR}/${JSON_REF}
and does not exist. This script writes generated-sources.json to $(pwd)."
fi

# ------------------------------------------------------------------------ build
say "Building (this takes a while the first time)"
mkdir -p "$(dirname "$OUT")"
rm -rf "$BUILD_DIR"

flatpak-builder \
    --user \
    --force-clean \
    --disable-updates \
    --state-dir="$STATE_DIR" \
    --repo="$REPO_DIR" \
    "$BUILD_DIR" \
    "$MANIFEST"

# ----------------------------------------------------------------------- bundle
# With the oyzmo Flatpak site checked out and its signing key present, the
# build is also published there (signed into its repo, not yet pushed live)
# and the bundle carries flatpak.oyzmo.no as its update address, so GNOME
# Software offers new versions as updates. Without either, a plain bundle.
FLATPAK_SITE="${FLATPAK_SITE:-/mnt/mycode/flatpak}"
FLATPAK_GNUPG="${FLATPAK_GNUPG:-$HOME/.local/share/oyzmo-flatpak/gnupg}"
if [ -x "$FLATPAK_SITE/tools/publish.sh" ] && [ -d "$FLATPAK_GNUPG" ]; then
    say "Publishing into $FLATPAK_SITE"
    "$FLATPAK_SITE/tools/publish.sh" "$REPO_DIR" "$APP_ID" rmMeta "$VERSION" "$OUT"
else
    say "Exporting bundle (no Flatpak site or signing key, so not published)"
    flatpak build-bundle "$REPO_DIR" "$OUT" "$APP_ID" \
        --runtime-repo=https://flathub.org/repo/flathub.flatpakrepo
fi

say "Done — $(du -h "$OUT" | cut -f1) at $OUT"
echo
echo "Nothing was installed. To try it:"
echo "    flatpak install --user $OUT"
echo "    flatpak run $APP_ID"
