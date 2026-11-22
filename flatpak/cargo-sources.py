#!/usr/bin/env python3
"""Writes flatpak/cargo-sources.json from Cargo.lock: every crates.io crate
as a Flatpak source (with the checksum Cargo.lock already records), vendored
under cargo/vendor, plus the Cargo config that points Cargo at it. Flathub
builds have no network, so this is how the crates get in.

Run from the repository root after Cargo.lock changes:

    python3 flatpak/cargo-sources.py
"""

import json
import sys
import tomllib
from pathlib import Path

CRATES_IO = "registry+https://github.com/rust-lang/crates.io-index"
VENDOR = "cargo/vendor"


def main() -> int:
    root = Path(__file__).resolve().parent.parent
    lock = tomllib.loads((root / "Cargo.lock").read_text())
    sources = []
    for package in lock["package"]:
        source = package.get("source")
        if source is None:
            continue  # this workspace's own crates
        if source != CRATES_IO:
            print(f"unsupported source for {package['name']}: {source}", file=sys.stderr)
            return 1
        name, version, checksum = package["name"], package["version"], package["checksum"]
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
            "contents": (
                "[source.vendored-sources]\n"
                f'directory = "{VENDOR}"\n\n'
                "[source.crates-io]\n"
                'replace-with = "vendored-sources"\n'
            ),
            "dest": "cargo",
            "dest-filename": "config.toml",
        }
    )
    out = root / "flatpak" / "cargo-sources.json"
    out.write_text(json.dumps(sources, indent=2) + "\n")
    print(f"wrote {len(sources) // 2} crates to {out.relative_to(root)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
