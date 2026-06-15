#!/usr/bin/env bash
# Runs inside the flatpak-builder sandbox (build-args: --share=network).
# Reproduces the cargo-run prerequisites, then builds + installs the app.
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

# 2. toolchain + cargo home inside the build dir
export PATH="/usr/lib/sdk/rust-stable/bin:$PATH"
export CARGO_HOME="$PROJECT/.cargo-home"
export CARGO_NET_GIT_FETCH_WITH_CLI=true

# 3. rustpush clone + FairPlay cert seeding.
#    Inlined from setup-rustpush.sh so the build doesn't depend on that script
#    being copied into the sandbox or kept executable. third_party/rustpush is
#    inside the build dir, exactly where Cargo's `path = "third_party/rustpush"`
#    points — no escaping the build root.
#    The cert names mirror setup-rustpush.sh; they're tied to the pinned rev.
RUSTPUSH_PIN=a7fab473e7a33325a760635285db2860de8e1cb0
RUSTPUSH_DIR="third_party/rustpush"
git config --global url."https://github.com/".insteadOf "git@github.com:" || true
if [ ! -d "$RUSTPUSH_DIR/.git" ]; then
  git clone https://github.com/OpenBubbles/rustpush.git "$RUSTPUSH_DIR"
fi
git -C "$RUSTPUSH_DIR" fetch --all --tags
git -C "$RUSTPUSH_DIR" checkout "$RUSTPUSH_PIN"
git -C "$RUSTPUSH_DIR" submodule update --init --recursive

CERT_DIR="$RUSTPUSH_DIR/certs"
CERT_SRC="$CERT_DIR/legacy-fairplay"
if [ ! -f "$CERT_SRC/fairplay.crt" ] || [ ! -f "$CERT_SRC/fairplay.pem" ]; then
  echo "!! $CERT_SRC/fairplay.{crt,pem} missing -- wrong rustpush rev or layout" >&2
  exit 1
fi
mkdir -p "$CERT_DIR/fairplay"
for name in \
  4056631661436364584235346952193 4056631661436364584235346952194 \
  4056631661436364584235346952195 4056631661436364584235346952196 \
  4056631661436364584235346952197 4056631661436364584235346952198 \
  4056631661436364584235346952199 4056631661436364584235346952200 \
  4056631661436364584235346952201 4056631661436364584235346952208 ; do
  cp "$CERT_SRC/fairplay.crt" "$CERT_DIR/fairplay/$name.crt"
  cp "$CERT_SRC/fairplay.pem" "$CERT_DIR/fairplay/$name.pem"
done

# 4. Build the real backend (rustpush is in default features).
cargo build --release

# 5. Install binary + desktop integration files.
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
