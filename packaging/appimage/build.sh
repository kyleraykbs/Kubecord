#!/usr/bin/env bash
# Builds Kubecord-x86_64.AppImage on Ubuntu 26.04 LTS. Run from the
# repository root:
#
#   podman run --rm -v "$PWD":/src -w /src docker.io/library/ubuntu:26.04 \
#     bash packaging/appimage/build.sh
#
# Ubuntu 26.04, not Debian 13: Debian 13's GTK 4.18 crashes at start on
# niri-style compositors, while 26.04's GTK 4.22 does not. The AppImage
# therefore needs glibc 2.43 (Ubuntu 26.04, Fedora 44, Arch and newer).
#
# The AppDir is assembled here (no linuxdeploy): the binary, the libraries it
# and its plugins link (minus the ones an AppImage must take from the host),
# GStreamer's plugins and scanner, GTK's media backend, gdk-pixbuf loaders,
# PipeWire's plugins and the pw-* tools Go Live uses, GSettings schemas and
# Adwaita's icons. AppRun points all of them at the bundled copies. The
# AppImage is the official type 2 runtime followed by a squashfs of the AppDir
# (what appimagetool does). Built without the captcha view: WebKitGTK does not
# bundle reliably.
set -euo pipefail

ARCH=x86_64
TRIPLET=x86_64-linux-gnu
APPID=io.github.kyleraykbs.Kubecord
RUNTIME_URL=https://github.com/AppImage/type2-runtime/releases/download/continuous/runtime-x86_64
OUT=${OUT:-target/appimage}

export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq --no-install-recommends \
  clang libclang-dev cmake make git pkg-config file curl ca-certificates squashfs-tools \
  libgtk-4-dev libadwaita-1-dev \
  libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev libgstreamer-plugins-bad1.0-dev \
  libpipewire-0.3-dev libopus-dev \
  gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
  gstreamer1.0-plugins-ugly gstreamer1.0-libav gstreamer1.0-gtk4 gstreamer1.0-pipewire \
  pipewire-bin libgdk-pixbuf2.0-bin librsvg2-common \
  adwaita-icon-theme >/dev/null

# wreq needs a newer Rust than Ubuntu packages: the current stable, via rustup.
if ! command -v cargo >/dev/null; then
  curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
fi

cargo build --release --locked -p kubecord --no-default-features

APPDIR=$OUT/AppDir
rm -rf "$APPDIR"
LIB=$APPDIR/usr/lib
mkdir -p "$APPDIR/usr/bin" "$LIB" "$APPDIR/usr/share"

install -m755 target/release/kubecord "$APPDIR/usr/bin/"
for tool in pw-cli pw-dump pw-link; do install -m755 "/usr/bin/$tool" "$APPDIR/usr/bin/"; done
install -m755 "/usr/lib/$TRIPLET/gstreamer1.0/gstreamer-1.0/gst-plugin-scanner" "$APPDIR/usr/bin/"
install -m755 "/usr/lib/$TRIPLET/gdk-pixbuf-2.0/gdk-pixbuf-query-loaders" "$APPDIR/usr/bin/"

# Plugins and modules, loaded at run time rather than linked.
cp -r "/usr/lib/$TRIPLET/gstreamer-1.0" "$LIB/"
cp -r "/usr/lib/$TRIPLET/gdk-pixbuf-2.0" "$LIB/"
cp -r "/usr/lib/$TRIPLET/spa-0.2" "$LIB/"
cp -r "/usr/lib/$TRIPLET/pipewire-0.3" "$LIB/"
# GTK's media backend: a module in older GTK, built into the library in newer.
if [[ -d "/usr/lib/$TRIPLET/gtk-4.0/4.0.0/media" ]]; then
  mkdir -p "$LIB/gtk-4.0/4.0.0"
  cp -r "/usr/lib/$TRIPLET/gtk-4.0/4.0.0/media" "$LIB/gtk-4.0/4.0.0/"
fi

# Data.
mkdir -p "$APPDIR/usr/share/glib-2.0"
cp -r /usr/share/glib-2.0/schemas "$APPDIR/usr/share/glib-2.0/"
mkdir -p "$APPDIR/usr/share/icons"
cp -r /usr/share/icons/Adwaita "$APPDIR/usr/share/icons/"
cp -r data/icons/hicolor "$APPDIR/usr/share/icons/"
mkdir -p "$APPDIR/usr/share/applications" "$APPDIR/usr/share/metainfo"
cp "data/$APPID.desktop" "$APPDIR/usr/share/applications/"
cp "data/$APPID.metainfo.xml" "$APPDIR/usr/share/metainfo/$APPID.appdata.xml"
cp "data/$APPID.desktop" "$APPDIR/"
cp "data/icons/hicolor/256x256/apps/$APPID.png" "$APPDIR/$APPID.png"
ln -sf "$APPID.png" "$APPDIR/.DirIcon"

# Libraries an AppImage must not carry: the C library and loader, and the
# graphics, audio-server and display libraries tied to the host's drivers.
host_only() {
  case "$1" in
    ld-linux*|libc.so*|libm.so*|libdl.so*|libpthread.so*|librt.so*|libresolv.so*|libutil.so*|libmvec.so*) return 0 ;;
    libGL.so*|libGLX*|libGLdispatch*|libEGL*|libGLES*|libOpenGL*|libvulkan*|libdrm*|libgbm*) return 0 ;;
    libX11.so*|libX11-xcb*|libxcb.so*|libwayland-client*|libwayland-cursor*|libwayland-egl*|libwayland-server*) return 0 ;;
    # The host's Wayland library calls through libffi; a bundled (older) one
    # under it crashed GTK on a newer distribution.
    libffi.so*) return 0 ;;
    libasound*|libjack*|libpulse*|libva.so*|libva-drm*|libva-x11*|libva-wayland*) return 0 ;;
    libfontconfig*|libfreetype*|libexpat*|libz.so*|libgcc_s*|libstdc++*|libudev*|libsystemd*) return 0 ;;
  esac
  return 1
}

# Copy every library the binaries and plugins link, recursively, into usr/lib.
mapfile -t objects < <(find "$APPDIR/usr/bin" "$LIB" -type f \( -name '*.so*' -o -perm -u+x \) | while read -r f; do
  file -b "$f" | grep -q '^ELF' && echo "$f"
done)
declare -A seen=()
queue=("${objects[@]}")
while ((${#queue[@]})); do
  obj=${queue[0]}
  queue=("${queue[@]:1}")
  while read -r name path; do
    [[ -z "$path" || "$path" == "not" ]] && continue
    host_only "$name" && continue
    [[ -n "${seen[$name]:-}" ]] && continue
    seen[$name]=1
    cp -L "$path" "$LIB/$name"
    queue+=("$LIB/$name")
  done < <(ldd "$obj" 2>/dev/null | awk '/=>/ {print $1, $3}')
done

cat > "$APPDIR/AppRun" <<'EOF'
#!/bin/sh
# Points GStreamer, GTK, gdk-pixbuf, PipeWire and GLib at the bundled copies.
HERE="$(dirname "$(readlink -f "$0")")"
LIB="$HERE/usr/lib"
export LD_LIBRARY_PATH="$LIB${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export PATH="$HERE/usr/bin:$PATH"
export GST_PLUGIN_SYSTEM_PATH_1_0="$LIB/gstreamer-1.0"
export GST_PLUGIN_SCANNER_1_0="$HERE/usr/bin/gst-plugin-scanner"
export GST_REGISTRY_1_0="${XDG_CACHE_HOME:-$HOME/.cache}/kubecord/gstreamer-registry-appimage.bin"
export GTK_PATH="$LIB/gtk-4.0"
export SPA_PLUGIN_DIR="$LIB/spa-0.2"
export PIPEWIRE_MODULE_DIR="$LIB/pipewire-0.3"
export GSETTINGS_SCHEMA_DIR="$HERE/usr/share/glib-2.0/schemas"
export XDG_DATA_DIRS="$HERE/usr/share:${XDG_DATA_DIRS:-/usr/local/share:/usr/share}"
# The loaders cache holds absolute paths, so it is made where the AppImage is
# mounted, each start.
CACHE="$(mktemp -p "${XDG_RUNTIME_DIR:-/tmp}" kubecord-loaders.XXXXXX 2>/dev/null || mktemp)"
"$HERE/usr/bin/gdk-pixbuf-query-loaders" "$LIB"/gdk-pixbuf-2.0/2.10.0/loaders/*.so > "$CACHE" 2>/dev/null
export GDK_PIXBUF_MODULE_FILE="$CACHE"
exec "$HERE/usr/bin/kubecord" "$@"
EOF
chmod +x "$APPDIR/AppRun"

# The type 2 runtime plus a squashfs of the AppDir is the AppImage.
curl -fsSL -o "$OUT/runtime" "$RUNTIME_URL"
mksquashfs "$APPDIR" "$OUT/AppDir.squashfs" -root-owned -noappend -comp zstd -quiet
cat "$OUT/runtime" "$OUT/AppDir.squashfs" > "$OUT/Kubecord-$ARCH.AppImage"
chmod +x "$OUT/Kubecord-$ARCH.AppImage"
rm -f "$OUT/AppDir.squashfs"
ls -l "$OUT/Kubecord-$ARCH.AppImage"
