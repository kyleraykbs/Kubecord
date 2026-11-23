#!/usr/bin/env python3
"""Writes flatpak/cargo-sources.json from Cargo.lock: every crates.io crate
as a Flatpak source (with the checksum Cargo.lock already records), vendored
under cargo/vendor, plus the Cargo config that points Cargo at it. Flatpak
builds have no network, so this is how the crates get in.

Git dependencies (wreq and BoringSSL's bindings, pinned to commits) come in
as a checkout of their repository, submodules included, from which each
crate's directory is copied into the vendor directory. A crate in a git
workspace inherits fields from the workspace's manifest, which Cargo cannot
read once the crate is vendored, so its manifest is replaced with the one
`cargo vendor` writes (fields filled in). That needs `cargo vendor` to have
the git checkouts (`cargo fetch` first), and is the only use of the network.

Run from the repository root after Cargo.lock changes, in the dev shell:

    python3 -I flatpak/cargo-sources.py
"""

import json
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path
from urllib.parse import urlsplit

CRATES_IO = "registry+https://github.com/rust-lang/crates.io-index"
VENDOR = "cargo/vendor"
CHECKOUTS = "cargo/git-checkouts"


def git_source(source):
    """`git+https://host/repo?rev=REV#COMMIT` -> (url, query url, commit)."""
    spec, commit = source[len("git+"):].split("#", 1)
    url = spec.split("?", 1)[0]
    return url, spec, commit


def crate_dir(checkout, name):
    """The directory of crate `name` in a checkout of its repository."""
    for manifest in sorted(checkout.rglob("Cargo.toml")):
        try:
            package = tomllib.loads(manifest.read_text()).get("package", {})
        except (tomllib.TOMLDecodeError, UnicodeDecodeError):
            continue
        if package.get("name") == name:
            return manifest.parent.relative_to(checkout)
    raise SystemExit(f"{name} not found in {checkout}")


def cargo_checkout(url, commit):
    """Cargo's own checkout of `url` at `commit` (from `cargo fetch`)."""
    home = Path.home() / ".cargo" / "git" / "checkouts"
    repo = Path(urlsplit(url).path).name.removesuffix(".git")
    for candidate in home.glob(f"{repo}-*/{commit[:7]}"):
        return candidate
    raise SystemExit(f"no checkout of {url} at {commit}; run `cargo fetch` first")


def main() -> int:
    root = Path(__file__).resolve().parent.parent
    lock = tomllib.loads((root / "Cargo.lock").read_text())
    sources = []
    git_crates = []
    for package in lock["package"]:
        source = package.get("source")
        if source is None:
            continue  # this workspace's own crates
        if source.startswith("git+"):
            git_crates.append(package)
            continue
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
    git_config = ""
    if git_crates:
        with tempfile.TemporaryDirectory() as vendored:
            subprocess.run(
                ["cargo", "vendor", "--offline", "--locked", "--versioned-dirs", vendored],
                cwd=root,
                check=True,
                stdout=subprocess.DEVNULL,
            )
            repos = {}
            for package in git_crates:
                url, spec, commit = git_source(package["source"])
                name, version = package["name"], package["version"]
                checkout_dest = f"{CHECKOUTS}/{Path(urlsplit(url).path).name}-{commit[:12]}"
                if (url, commit) not in repos:
                    repos[(url, commit)] = spec
                    sources.append({"type": "git", "url": url, "commit": commit, "dest": checkout_dest})
                    git_config += (
                        f'\n[source."git+{spec}"]\n'
                        f'git = "{url}"\n'
                        f'rev = "{spec.split("rev=", 1)[1]}"\n'
                        'replace-with = "vendored-sources"\n'
                    )
                subdir = crate_dir(cargo_checkout(url, commit), name)
                dest = f"{VENDOR}/{name}-{version}"
                sources.append(
                    {
                        "type": "shell",
                        "commands": [f"cp -r {checkout_dest}/{subdir} {dest}".replace("/.", "")],
                    }
                )
                manifest = Path(vendored) / f"{name}-{version}" / "Cargo.toml"
                sources.append(
                    {
                        "type": "inline",
                        "contents": manifest.read_text(),
                        "dest": dest,
                        "dest-filename": "Cargo.toml",
                    }
                )
                sources.append(
                    {
                        "type": "inline",
                        "contents": json.dumps({"package": None, "files": {}}),
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
                + git_config
            ),
            "dest": "cargo",
            "dest-filename": "config.toml",
        }
    )
    out = root / "flatpak" / "cargo-sources.json"
    out.write_text(json.dumps(sources, indent=2) + "\n")
    crates = sum(1 for source in sources if source.get("dest-filename") == ".cargo-checksum.json")
    print(f"wrote {crates} crates ({len(git_crates)} from git) to {out.relative_to(root)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
