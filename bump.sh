#!/usr/bin/env bash
#
# Bump the version. Cargo.toml is the single source of truth — the header bar
# reads it through env!("CARGO_PKG_VERSION") — and this script carries the new
# number into Cargo.lock and the metainfo release list.
#
#     ./bump.sh patch|minor|major [-m "what changed"]
#
# Never edit a version string by hand.
set -euo pipefail
cd "$(dirname "$0")"

PART="${1:-}"
NOTE=""
if [ "${2:-}" = "-m" ]; then
    NOTE="${3:-}"
fi

case "$PART" in
    patch | minor | major) ;;
    *)
        echo "usage: $0 patch|minor|major [-m \"what changed\"]" >&2
        exit 2
        ;;
esac

CURRENT=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
[ -n "$CURRENT" ] || { echo "error: no version in Cargo.toml" >&2; exit 1; }

IFS=. read -r MAJOR MINOR PATCH <<<"$CURRENT"
case "$PART" in
    major) MAJOR=$((MAJOR + 1)); MINOR=0; PATCH=0 ;;
    minor) MINOR=$((MINOR + 1)); PATCH=0 ;;
    patch) PATCH=$((PATCH + 1)) ;;
esac
NEW="${MAJOR}.${MINOR}.${PATCH}"
TODAY=$(date +%F)

VERSION="$NEW" DATE="$TODAY" RELEASE_NOTE="$NOTE" python3 - <<'PY'
import os
import pathlib
import re

new = os.environ["VERSION"]
date = os.environ["DATE"]
note = os.environ["RELEASE_NOTE"] or "Maintenance release."

# Cargo.toml: only the [package] version, which is the first one in the file.
toml = pathlib.Path("Cargo.toml")
toml.write_text(re.sub(r'^version = ".*"', f'version = "{new}"', toml.read_text(), count=1, flags=re.M))

# Cargo.lock: the version inside our own [[package]] block.
lock = pathlib.Path("Cargo.lock")
if lock.exists():
    lock.write_text(
        re.sub(
            r'(\[\[package\]\]\nname = "rmmeta"\nversion = ")[^"]*(")',
            rf'\g<1>{new}\g<2>',
            lock.read_text(),
            count=1,
        )
    )

# metainfo: a new <release> at the top of the list.
meta = pathlib.Path("flatpak/no.oyzmo.RmMeta.metainfo.xml")
entry = (
    f'    <release version="{new}" date="{date}">\n'
    f"      <description>\n"
    f"        <p>{note}</p>\n"
    f"      </description>\n"
    f"    </release>\n"
)
text = meta.read_text()
if f'version="{new}"' not in text:
    meta.write_text(text.replace("  <releases>\n", "  <releases>\n" + entry, 1))
PY

echo "$CURRENT -> $NEW"
echo "Remember: this only prepares the release. Building and publishing are manual."
