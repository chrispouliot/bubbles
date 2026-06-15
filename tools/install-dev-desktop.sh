#!/usr/bin/env bash
# Installs the .desktop + icons into ~/.local/share so that notifications sent
# under a plain `cargo run` (no Flatpak) get the right app name, icon, and
# click-to-open routing. GNOME maps a notification to an app by application ID
# -> <app-id>.desktop, so the desktop file's basename must match APP_ID.
#
#   ./tools/install-dev-desktop.sh            # uses target/debug/openbubbles-gtk
#   ./tools/install-dev-desktop.sh --release  # uses target/release/openbubbles-gtk
#
# Re-run if you switch between debug/release (the Exec path changes).
set -euo pipefail

APP_ID=app.openbubbles.Gtk.Devel
PROJECT="$(cd "$(dirname "$0")/.." && pwd)"

PROFILE=debug
[ "${1:-}" = "--release" ] && PROFILE=release
BIN="$PROJECT/target/$PROFILE/openbubbles-gtk"
if [ ! -x "$BIN" ]; then
  echo "!! $BIN not found -- build it first (cargo build${PROFILE:+ --$PROFILE} excluded for debug)" >&2
  echo "   debug:   cargo build" >&2
  echo "   release: cargo build --release" >&2
  exit 1
fi

DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
APPS="$DATA/applications"
ICONS="$DATA/icons/hicolor"
mkdir -p "$APPS"

# Desktop file: reuse the repo's canonical one but point Exec at the real binary.
sed "s|^Exec=.*|Exec=$BIN|" "$PROJECT/$APP_ID.desktop" > "$APPS/$APP_ID.desktop"
echo ">> installed $APPS/$APP_ID.desktop (Exec=$BIN)"

# Icons: scalable SVG + the rasterized sizes.
install -Dm644 "$PROJECT/assets/icons/hicolor/scalable/apps/$APP_ID.svg" \
  "$ICONS/scalable/apps/$APP_ID.svg"
for sz in 64 128 256; do
  src="$PROJECT/assets/icons/hicolor/${sz}x${sz}/apps/$APP_ID.png"
  [ -f "$src" ] && install -Dm644 "$src" "$ICONS/${sz}x${sz}/apps/$APP_ID.png"
done
echo ">> installed icons into $ICONS"

# Refresh caches if the tools are around (hicolor works without them, but this
# nudges the shell to notice the new app immediately).
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$APPS" || true
if command -v gtk4-update-icon-cache >/dev/null 2>&1; then
  gtk4-update-icon-cache -f -t "$ICONS" 2>/dev/null || true
elif command -v gtk-update-icon-cache >/dev/null 2>&1; then
  gtk-update-icon-cache -f -t "$ICONS" 2>/dev/null || true
fi

echo ">> done. Run the app with: cargo run"
echo "   (If the icon/name doesn't show on the first notification, log out/in"
echo "    once so the shell re-scans ~/.local/share/applications.)"
