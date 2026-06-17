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

    use std::cell::Cell;
    use std::rc::Rc;

    // Last known client size, refreshed whenever the surface reports a resize.
    let state = Rc::new(Cell::new((0i32, 0i32)));
    // Pending debounce timer id, so a drag coalesces into a single write.
    let timer = Rc::new(Cell::new(None::<glib::SourceId>));

    // Record the current size and schedule a debounced save (300 ms).
    let schedule: Rc<dyn Fn(i32, i32)> = {
        let state = state.clone();
        let timer = timer.clone();
        Rc::new(move |w, h| {
            if w < 300 || h < 300 {
                return;
            }
            state.set((w, h));
            // Cancel any pending write and reschedule, so only the final size
            // of a drag burst is persisted.
            if let Some(id) = timer.take() {
                id.remove();
            }
            let state = state.clone();
            let timer_inner = timer.clone();
            let id = glib::timeout_add_local_once(std::time::Duration::from_millis(300), move || {
                timer_inner.set(None);
                let (sw, sh) = state.get();
                if read().as_ref().map(|(rw, rh)| (*rw, *rh)) != Some((sw, sh)) {
                    save(sw, sh);
                }
            });
            timer.set(Some(id));
        })
    };

    // --- resize handler (debounced 300 ms) ---
    //
    // `notify::width` / `notify::height` are *not* properties of GtkWindow, and
    // `size-allocate` is a vfunc, not a GObject signal in GTK4 — so neither of
    // the old approaches fired. The reliable hook is the toplevel `GdkSurface`'s
    // `width`/`height` properties, which update on every step of a user drag.
    // The surface exists only after the window is realized, so bind it on
    // `realize` (or immediately if already realized).
    let bind_surface: Rc<dyn Fn()> = {
        let win = win.clone();
        let schedule = schedule.clone();
        Rc::new(move || {
            let Some(surface) = win.surface() else { return; };
            let sched1 = schedule.clone();
            let win1 = win.clone();
            surface.connect_notify_local(Some("width"), move |_, _| {
                sched1(win1.width(), win1.height());
            });
            let sched2 = schedule.clone();
            let win2 = win.clone();
            surface.connect_notify_local(Some("height"), move |_, _| {
                sched2(win2.width(), win2.height());
            });
        })
    };
    if win.is_realized() {
        bind_surface();
    } else {
        let bind = bind_surface.clone();
        win.connect_realize(move |_| {
            bind();
        });
    }

    // --- close handler (fires on tray-hide and user close; NOT on quit) ---
    let st = state.clone();
    let w1 = win.clone();
    win.connect_close_request(move |_| {
        // Prefer the live size; fall back to the last reported size.
        let (cw, ch) = (w1.width(), w1.height());
        let (w, h) = if cw >= 300 && ch >= 300 { (cw, ch) } else { st.get() };
        if w >= 300
            && h >= 300
            && read().as_ref().map(|(rw, rh)| (*rw, *rh)) != Some((w, h))
        {
            save(w, h);
        }
        // Don't stop propagation — let tray.rs handle the hide.
        glib::Propagation::Proceed
    });

    // --- quit handler (fires when the app exits, incl. app.quit()) ---
    //
    // `close-request` is NOT emitted on `app.quit()` (tray "Quit", session
    // logout), so without this the size would be lost whenever the app quits
    // instead of being hidden to the tray.
    if let Some(app) = win.application() {
        let st = state.clone();
        app.connect_shutdown(move |_| {
            let (w, h) = st.get();
            if w >= 300
                && h >= 300
                && read().as_ref().map(|(rw, rh)| (*rw, *rh)) != Some((w, h))
            {
                save(w, h);
            }
        });
    }
}
