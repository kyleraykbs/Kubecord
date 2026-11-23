#!/usr/bin/env bash
# Builds the Debian package on Debian 13 (trixie). Run inside the official
# Rust image, from the repository root:
#
#   podman run --rm -v "$PWD":/src -w /src docker.io/library/rust:1-trixie \
#     bash packaging/deb/build.sh
#
# The .deb lands in target/debian/.
set -euo pipefail

export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq --no-install-recommends \
  clang libclang-dev pkg-config dpkg-dev cmake git \
  libgtk-4-dev libadwaita-1-dev libwebkitgtk-6.0-dev \
  libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev libgstreamer-plugins-bad1.0-dev \
  libpipewire-0.3-dev libopus-dev librlottie-dev >/dev/null

command -v cargo-deb >/dev/null || cargo install --locked cargo-deb

cargo build --release --locked -p kubecord
cargo deb --no-build -p kubecord
ls -l target/debian/*.deb
