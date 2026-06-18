#!/usr/bin/env bash
# Runs inside the flatpak-builder sandbox (build-args: --share=network).
# Builds + installs the app. rustpush is vendored under third_party/rustpush
# (submodules + FairPlay certs pre-seeded), so there is no build-time clone or
# cert-seeding step here — only protoc download, crate fetch, and the build.
set -euo pipefail

PROJECT="$(pwd)"   # the copied source tree = flatpak-builder build dir

# 1. protoc — build.rs (app + rustpush) needs it, and the freedesktop SDK
#    doesn't ship it. Grab a prebuilt release (bump the version freely).
PROTOC_VER=25.1
curl -fL -o /tmp/protoc.zip \
  "https://github.com/protocolbuffers/protobuf/releases/download/v${PROTOC_VER}/protoc-${PROTOC_VER}-linux-x86_64.zip"
mkdir -p /tmp/protoc
( cd /tmp/protoc && unzip -oq /tmp/protoc.zip )
export PROTOC=/tmp/protoc/bin/protoc

# 2. toolchain + cargo home inside the build dir.
#    No CARGO_NET_GIT_FETCH_WITH_CLI / git insteadOf rewriting is needed: the
#    only git cargo dep is android-loader over https, which libgit2 fetches
#    directly. rustpush and its submodules are vendored as plain directories.
export PATH="/usr/lib/sdk/rust-stable/bin:$PATH"
export CARGO_HOME="$PROJECT/.cargo-home"

# 3. Build the real backend (rustpush is in default features, vendored).
cargo build --release

# 4. Install binary + desktop integration files.
install -Dm755 target/release/openbubbles-gtk \
  /app/bin/openbubbles-gtk
install -Dm644 app.openbubbles.Gtk.Devel.desktop \
  /app/share/applications/app.openbubbles.Gtk.Devel.desktop
install -Dm644 app.openbubbles.Gtk.Devel.metainfo.xml \
  /app/share/metainfo/app.openbubbles.Gtk.Devel.metainfo.xml
install -Dm644 assets/icons/hicolor/scalable/apps/app.openbubbles.Gtk.Devel.svg \
  /app/share/icons/hicolor/scalable/apps/app.openbubbles.Gtk.Devel.svg
# Rasterized sizes: appstreamcli compose needs a readable raster icon (it can't
# rely on the host having an SVG loader), and they help non-GNOME environments.
for sz in 64 128 256; do
  install -Dm644 "assets/icons/hicolor/${sz}x${sz}/apps/app.openbubbles.Gtk.Devel.png" \
    "/app/share/icons/hicolor/${sz}x${sz}/apps/app.openbubbles.Gtk.Devel.png"
done
# In-app action icons (splash hero, send button, etc.) are referenced by name
# at runtime — from_icon_name("empty-state"), from_icon_name("ob-send-symbolic")
# — and GTK resolves them against $XDG_DATA_DIRS/icons/hicolor.
for icon in assets/icons/hicolor/scalable/actions/*.svg; do
  install -Dm644 "$icon" \
    "/app/share/icons/hicolor/scalable/actions/$(basename "$icon")"
done
