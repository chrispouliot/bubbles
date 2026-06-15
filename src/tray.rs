//! System-tray (StatusNotifierItem) integration.
//!
//! Puts the app icon in the desktop's tray / top bar via the freedesktop
//! StatusNotifierItem spec — what GNOME's "AppIndicator / KStatusNotifierItem
//! Support" extension renders. Left-click (or the "Open" menu entry) raises the
//! window; a red dot is overlaid on the icon while any chat has an outstanding
//! notification. Closing the window hides it to the tray instead of quitting.
//!
//! Built on the pure-Rust `ksni` crate — no C `libappindicator`, no GTK3.
//!
//! Wiring (already done in `main.rs` / `ui/mod.rs`):
//!   * `tray::install(&app, &window)` once, right after the window is built.
//!   * `tray::set_unread(bool)` whenever the notification state changes.
//!
//! If the desktop has no StatusNotifier host, everything here no-ops cleanly.

use std::sync::OnceLock;

use adw::prelude::*;
use gtk::gdk_pixbuf::PixbufLoader;
use gtk::gdk_pixbuf::prelude::*;
use ksni::blocking::TrayMethods;

/// The app icon, embedded at compile time so the tray renders identically in
/// dev, system installs, and Flatpak with no runtime icon-theme lookup (the
/// shell can't resolve a sandboxed theme path, so we hand it raw pixels).
const ICON_PNG: &[u8] =
    include_bytes!("../assets/icons/hicolor/128x128/apps/app.openbubbles.Gtk.Devel.png");

/// Live handle to the running tray; `set_unread` pushes state through it.
static TRAY: OnceLock<ksni::blocking::Handle<ObTray>> = OnceLock::new();

enum TrayEvent {
    Activate,
    Quit,
}

struct ObTray {
    icon: Vec<ksni::Icon>,
    icon_badged: Vec<ksni::Icon>,
    unread: bool,
    tx: async_channel::Sender<TrayEvent>,
}

impl ksni::Tray for ObTray {
    fn id(&self) -> String {
        "app.openbubbles.Gtk".into()
    }

    fn title(&self) -> String {
        "OpenBubbles".into()
    }

    /// Swap the whole icon to a pre-badged variant when unread — more reliable
    /// across tray hosts than `overlay_icon_pixmap`, which some ignore.
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        if self.unread {
            self.icon_badged.clone()
        } else {
            self.icon.clone()
        }
    }

    /// Left-click. `MENU_ON_ACTIVATE` stays at its default `false`, so this
    /// fires instead of opening the menu.
    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.tx.send_blocking(TrayEvent::Activate);
    }

    /// Right-click menu — also covers hosts that route left-click to the menu.
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        vec![
            StandardItem {
                label: "Open OpenBubbles".into(),
                activate: Box::new(|t: &mut ObTray| {
                    let _ = t.tx.send_blocking(TrayEvent::Activate);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|t: &mut ObTray| {
                    let _ = t.tx.send_blocking(TrayEvent::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Toggle the unread/notification dot on the tray icon. No-op if the tray
/// failed to start or the desktop has no StatusNotifier host.
pub fn set_unread(on: bool) {
    if let Some(handle) = TRAY.get() {
        handle.update(move |t: &mut ObTray| t.unread = on);
    }
}

/// Start the tray and wire it to `window`: close-to-tray, left-click / "Open"
/// to raise + focus, "Quit" to exit. Call once after the main window is built.
pub fn install(app: &adw::Application, window: &adw::ApplicationWindow) {
    let (base, badged) = match load_icons() {
        Some(icons) => icons,
        None => {
            log::warn!("tray: could not decode app icon; tray disabled");
            return;
        }
    };

    let (tx, rx) = async_channel::unbounded::<TrayEvent>();
    let tray = ObTray {
        icon: base,
        icon_badged: badged,
        unread: false,
        tx,
    };

    // Flatpak (and other sandboxes) can't own the spec's well-known bus name.
    let spawned = if std::path::Path::new("/.flatpak-info").exists() {
        tray.disable_dbus_name(true).spawn()
    } else {
        tray.spawn()
    };
    let handle = match spawned {
        Ok(h) => h,
        Err(e) => {
            log::warn!("tray: StatusNotifier host unavailable: {e}");
            return;
        }
    };
    let _ = TRAY.set(handle);

    // Closing the window hides it to the tray instead of quitting.
    window.connect_close_request(|w| {
        w.set_visible(false);
        glib::Propagation::Stop
    });

    // Keep the application alive while it lives only in the tray (otherwise GTK
    // quits after the last visible window closes).
    let hold = app.hold();
    let app = app.clone();
    let win: gtk::Window = window.clone().upcast();
    glib::spawn_future_local(async move {
        let _hold = hold; // released (letting the app exit) only when the loop ends
        while let Ok(event) = rx.recv().await {
            match event {
                TrayEvent::Activate => {
                    win.set_visible(true);
                    win.present();
                }
                TrayEvent::Quit => app.quit(),
            }
        }
    });
}

/// Decode the embedded PNG into (base, badged) ARGB32 pixmaps.
fn load_icons() -> Option<(Vec<ksni::Icon>, Vec<ksni::Icon>)> {
    let (w, h, rgba) = load_rgba(ICON_PNG)?;
    let base = rgba_to_icon(w, h, &rgba);
    let mut badged = rgba.clone();
    stamp_dot(w, h, &mut badged);
    Some((vec![base], vec![rgba_to_icon(w, h, &badged)]))
}

/// Load a PNG into tightly-packed RGBA via gdk-pixbuf (no extra image dep).
fn load_rgba(png: &[u8]) -> Option<(i32, i32, Vec<u8>)> {
    let loader = PixbufLoader::new();
    loader.write(png).ok()?;
    loader.close().ok()?;
    let pb = loader.pixbuf()?;

    let (w, h) = (pb.width(), pb.height());
    let nch = pb.n_channels() as usize; // 3 (RGB) or 4 (RGBA)
    let stride = pb.rowstride() as usize;
    let bytes = pb.read_pixel_bytes();
    let src = bytes.as_ref();

    let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
    for y in 0..h as usize {
        let row = &src[y * stride..y * stride + w as usize * nch];
        for px in row.chunks_exact(nch) {
            rgba.push(px[0]);
            rgba.push(px[1]);
            rgba.push(px[2]);
            rgba.push(if nch == 4 { px[3] } else { 0xff });
        }
    }
    Some((w, h, rgba))
}

/// RGBA (straight alpha) -> ksni `Icon` ARGB32 (network byte order: A,R,G,B).
fn rgba_to_icon(w: i32, h: i32, rgba: &[u8]) -> ksni::Icon {
    let mut data = Vec::with_capacity(rgba.len());
    for px in rgba.chunks_exact(4) {
        data.push(px[3]);
        data.push(px[0]);
        data.push(px[1]);
        data.push(px[2]);
    }
    ksni::Icon {
        width: w,
        height: h,
        data,
    }
}

/// Stamp a white-ringed red dot into the bottom-right corner (in place, RGBA).
fn stamp_dot(w: i32, h: i32, rgba: &mut [u8]) {
    let radius = (w.min(h) as f32 * 0.26) as i32;
    let margin = w.min(h) / 16;
    let cx = w - radius - margin;
    let cy = h - radius - margin;
    let ring = radius as f32 + 1.5;
    for y in (cy - radius - 2).max(0)..(cy + radius + 2).min(h) {
        for x in (cx - radius - 2).max(0)..(cx + radius + 2).min(w) {
            let dx = (x - cx) as f32;
            let dy = (y - cy) as f32;
            let dist = (dx * dx + dy * dy).sqrt();
            let idx = ((y * w + x) * 4) as usize;
            if dist <= radius as f32 {
                rgba[idx] = 0xff; // R
                rgba[idx + 1] = 0x3b; // G
                rgba[idx + 2] = 0x30; // B
                rgba[idx + 3] = 0xff; // A
            } else if dist <= ring {
                rgba[idx] = 0xff;
                rgba[idx + 1] = 0xff;
                rgba[idx + 2] = 0xff;
                rgba[idx + 3] = 0xff;
            }
        }
    }
}
