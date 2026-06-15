//! Persist and restore the main window size so the app opens at the last-used
//! dimensions instead of the hardcoded defaults.

use std::path::PathBuf;

use gtk::glib;

const STATE_FILE: &str = "window.txt";

fn data_dir() -> PathBuf {
    glib::user_data_dir().join("openbubbles-gtk")
}

fn state_path() -> PathBuf {
    data_dir().join(STATE_FILE)
}

/// Read the last-saved window size, or `None` if there's no saved state yet.
///
/// File format: `"WIDTHxHEIGHT"` (e.g. `"800x600"`).
pub fn read() -> Option<(i32, i32)> {
    let data = std::fs::read_to_string(state_path()).ok()?;
    let data = data.trim();
    let sep = data.find('x')?;
    let width: i32 = data[..sep].parse().ok()?;
    let height: i32 = data[sep + 1..].parse().ok()?;
    if width >= 300 && height >= 300 && width <= 5000 && height <= 5000 {
        Some((width, height))
    } else {
        None
    }
}

/// Save a window size to disk. Creates the parent directory if needed.
fn save(width: i32, height: i32) {
    let path = state_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let data = format!("{width}x{height}");
    let _ = std::fs::write(&path, data);
}

/// Wire up size persistence on `window`.
///
/// Saves on two events:
/// 1. **Resize** — via `notify::width` / `notify::height`, debounced 300 ms
///    so a drag coalesces into one write.
/// 2. **Close** — via `connect_close_request` so the size is saved when the
///    window is hidden (tray) or quit.
pub fn install(win: &adw::ApplicationWindow) {
    use gtk::prelude::*;

    let win = win.clone();

    // --- resize handler (debounced) ---
    use std::cell::Cell;
    use std::rc::Rc;

    let state = Rc::new(Cell::new((0i32, 0i32)));

    let st1 = state.clone();
    win.connect_notify_local(Some("width"), move |win, _| {
        let w = win.width();
        let h = win.height();
        if w <= 0 || h <= 0 {
            return;
        }
        st1.set((w.max(300), h.max(300)));
        let st = st1.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(300), move || {
            let (sw, sh) = st.get();
            if read().as_ref().map(|(rw, rh)| (*rw, *rh)) != Some((sw, sh)) {
                let _ = save(sw, sh);
            }
        });
    });

    let st2 = state.clone();
    win.connect_notify_local(Some("height"), move |win, _| {
        let w = win.width();
        let h = win.height();
        if w <= 0 || h <= 0 {
            return;
        }
        st2.set((w.max(300), h.max(300)));
        let st = st2.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(300), move || {
            let (sw, sh) = st.get();
            if read().as_ref().map(|(rw, rh)| (*rw, *rh)) != Some((sw, sh)) {
                let _ = save(sw, sh);
            }
        });
    });

    // --- close handler (fires when tray hides the window, and on real quit) ---
    win.connect_close_request(move |win| {
        let (w, h) = (win.width().max(300), win.height().max(300));
        if read().as_ref().map(|(rw, rh)| (*rw, *rh)) != Some((w, h)) {
            let _ = save(w, h);
        }
        // Don't stop propagation — let tray.rs handle the hide.
        glib::Propagation::Proceed
    });
}
