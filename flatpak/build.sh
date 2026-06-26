#!/usr/bin/env bash
# Runs inside the flatpak-builder sandbox (build-args: --share=network).
# Builds + installs the app. Everything is vendored, so there is no build-time
# clone or cert-seeding step here — only protoc download, crate fetch, and
# the build. Notes on the vendored tree, since it looks submodule-shaped:
#   * third_party/rustpush/apple-private-apis/ and its nested icloud-auth /
#     omnisette / clearadi submodules are committed as ordinary files in
#     this repo (not gitlinks), so a fresh `git clone` populates them — no
#     `git submodule update --init` is needed, and trying it would fail at
#     the SSH URLs in third_party/rustpush/.gitmodules.
#   * The open-absinthe entry in that .gitmodules is dead text for this
#     build: third_party/rustpush/Cargo.toml has a path override that
#     redirects the dep to ../../crates/open-absinthe (the workspace
#     member). cargo follows the override, not the submodule URL.
#   * FairPlay certs are pre-seeded at third_party/rustpush/certs/.
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
install -Dm755 target/release/bubbles \
  /app/bin/bubbles
install -Dm644 io.github.chrispouliot.Bubbles.desktop \
  /app/share/applications/io.github.chrispouliot.Bubbles.desktop
install -Dm644 io.github.chrispouliot.Bubbles.metainfo.xml \
  /app/share/metainfo/io.github.chrispouliot.Bubbles.metainfo.xml
install -Dm644 assets/icons/hicolor/scalable/apps/io.github.chrispouliot.Bubbles.svg \
  /app/share/icons/hicolor/scalable/apps/io.github.chrispouliot.Bubbles.svg
# Rasterized sizes: appstreamcli compose needs a readable raster icon (it can't
# rely on the host having an SVG loader), and they help non-GNOME environments.
for sz in 64 128 256; do
  install -Dm644 "assets/icons/hicolor/${sz}x${sz}/apps/io.github.chrispouliot.Bubbles.png" \
    "/app/share/icons/hicolor/${sz}x${sz}/apps/io.github.chrispouliot.Bubbles.png"
done
# In-app action icons (splash hero, send button, etc.) are referenced by name
# at runtime — from_icon_name("empty-state"), from_icon_name("ob-send-symbolic")
# — and GTK resolves them against $XDG_DATA_DIRS/icons/hicolor.
for icon in assets/icons/hicolor/scalable/actions/*.svg; do
  install -Dm644 "$icon" \
    "/app/share/icons/hicolor/scalable/actions/$(basename "$icon")"
done
