#!/usr/bin/env python3
"""Turn Cargo.lock into the flatpak sources list for an offline cargo build.

This is a stdlib-only stand-in for flatpak-builder-tools' flatpak-cargo-generator.py.
That script pulls in aiohttp and toml, neither of which is packaged here by
default, and it needs both only for git dependencies -- it has to clone the repo
and read its Cargo.toml to work out the crate names. rmMeta has no git
dependencies: every entry in Cargo.lock is a crates.io registry crate whose
tarball URL and sha256 are already in the lock file. So the whole job is a
transcription, and tomllib (stdlib since 3.11) is enough to do it.

If a git dependency is ever added, this refuses to guess and says to use the
upstream generator instead.

    python3 flatpak/cargo-sources.py Cargo.lock -o generated-sources.json
"""

import argparse
import json
import sys
import tomllib

CRATES_IO = "registry+https://github.com/rust-lang/crates.io-index"
VENDOR = "cargo/vendor"

# Points cargo at the vendored directory instead of the network.
CARGO_CONFIG = (
    "[source.vendored-sources]\n"
    'directory = "cargo/vendor"\n'
    "\n"
    "[source.crates-io]\n"
    'replace-with = "vendored-sources"\n'
)


def sources_for(lockfile):
    with open(lockfile, "rb") as fh:
        lock = tomllib.load(fh)

    sources = []
    for pkg in lock.get("package", []):
        source = pkg.get("source")
        if source is None:
            continue  # the workspace's own crates: built from the source tree
        name, version = pkg["name"], pkg["version"]
        if source.startswith("git+"):
            sys.exit(
                f"error: {name} {version} is a git dependency ({source}).\n"
                "This generator only handles crates.io. Use the upstream\n"
                "flatpak-cargo-generator.py (needs python3-aiohttp and python3-toml)."
            )
        if source != CRATES_IO:
            sys.exit(f"error: {name} {version} comes from an unknown source: {source}")

        checksum = pkg.get("checksum")
        if not checksum:
            sys.exit(f"error: {name} {version} has no checksum in {lockfile}")

        dest = f"{VENDOR}/{name}-{version}"
        sources.append(
            {
                "type": "archive",
                "archive-type": "tar-gzip",
                "url": f"https://static.crates.io/crates/{name}/{name}-{version}.crate",
                "sha256": checksum,
                "dest": dest,
            }
        )
        # cargo refuses to use a vendored crate without this file. The upstream
        # generator writes an empty "files" map too: cargo only verifies the
        # per-file hashes for crates it thinks it may have modified, and the
        # package hash above is what actually pins the contents.
        sources.append(
            {
                "type": "inline",
                "contents": json.dumps({"package": checksum, "files": {}}),
                "dest": dest,
                "dest-filename": ".cargo-checksum.json",
            }
        )

    sources.append(
        {
            "type": "inline",
            "contents": CARGO_CONFIG,
            "dest": "cargo",
            "dest-filename": "config",
        }
    )
    return sources


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("lockfile", nargs="?", default="Cargo.lock")
    ap.add_argument("-o", "--output", default="generated-sources.json")
    args = ap.parse_args()

    sources = sources_for(args.lockfile)
    with open(args.output, "w") as fh:
        json.dump(sources, fh, indent=4)
        fh.write("\n")
    crates = sum(1 for s in sources if s["type"] == "archive")
    print(f"    {crates} crates -> {args.output}")


if __name__ == "__main__":
    main()
