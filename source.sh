#!/usr/bin/env bash
#
# Pack the source tree into a zip for distribution.
#
#     ./source.sh
#
# The result is dist/rmmeta-<version>-src.zip, holding a single top-level
# rmmeta-<version>/ directory so it unpacks tidily. Nothing is uploaded
# anywhere; publishing is manual.
#
# The list below is an ALLOWLIST, not a set of exclusions. target/ alone is
# half a gigabyte and build/ is root-owned in places, so an "everything except"
# rule is one forgotten pattern away from shipping a build tree. If a file
# belongs in the distribution, name it here.
set -euo pipefail
cd "$(dirname "$0")"

say() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

command -v zip >/dev/null || die "zip is not installed (sudo dnf install zip)"

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
[ -n "$VERSION" ] || die "no version in Cargo.toml"

NAME="rmmeta-${VERSION}"
OUT="dist/${NAME}-src.zip"

# What goes in. Directories are taken whole; files are named one by one.
DIRS=(
    src        # the app and the engine
    tests      # integration tests
    data       # gresource bundle and the icons it carries
    flatpak    # manifest, desktop entry, metainfo, cargo-sources.py
)
FILES=(
    Cargo.toml
    Cargo.lock
    build.rs
    readme.md
    CLAUDE.md
    # The GPL requires the licence text to travel with the code it covers, so
    # this is a distribution file, not documentation.
    COPYING
    .gitignore
    bump.sh
    makeflatpak.sh
    source.sh
    screenshot.png
    # dev/ is scratch and gitignored, but these three are the documented way to
    # produce the fixtures and check the output with tools that know nothing
    # about this code. The fixtures and their cleaned copies are regenerated,
    # so they stay out.
    dev/make_fixtures.py
    dev/check.py
    dev/render_icon.js
)

# generated-sources.json is deliberately absent: makeflatpak.sh regenerates it
# from Cargo.lock whenever it is stale, so shipping a stale copy helps nobody.

say "Checking the file list"
MISSING=()
for d in "${DIRS[@]}"; do [ -d "$d" ] || MISSING+=("$d/"); done
for f in "${FILES[@]}"; do [ -f "$f" ] || MISSING+=("$f"); done
[ ${#MISSING[@]} -eq 0 ] || die "not found: ${MISSING[*]}
Either the tree moved or the list in $0 is out of date."

STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT

say "Staging ${NAME}"
mkdir -p "$STAGE/$NAME"
for d in "${DIRS[@]}"; do
    cp -a "$d" "$STAGE/$NAME/"
done
for f in "${FILES[@]}"; do
    mkdir -p "$STAGE/$NAME/$(dirname "$f")"
    cp -a "$f" "$STAGE/$NAME/$f"
done

# Editor and OS cruft can sit inside a copied directory; drop it from the copy
# rather than from the source tree.
find "$STAGE/$NAME" \
    \( -name '*.swp' -o -name '*~' -o -name '.DS_Store' \
       -o -name '__pycache__' -o -name '.mypy_cache' \) \
    -exec rm -rf {} + 2>/dev/null || true

say "Writing $OUT"
mkdir -p dist
rm -f "$OUT"
ABS_OUT="$PWD/$OUT"
(cd "$STAGE" && zip -r -q -X "$ABS_OUT" "$NAME")

COUNT=$(unzip -Z1 "$OUT" | grep -cv '/$' || true)
say "Done — $(du -h "$OUT" | cut -f1), $COUNT files"
echo
echo "Unpacks to ${NAME}/. To build from it:"
echo "    unzip $(basename "$OUT") && cd $NAME"
echo "    CARGO_TARGET_DIR=/tmp/rmmeta-target cargo build --release"
