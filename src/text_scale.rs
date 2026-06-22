//! Persist and restore the chat text size offset (in points) so the user's
//! preference survives app restarts.

use std::path::PathBuf;

use gtk::glib;

const STATE_FILE: &str = "text_scale.txt";
const DEFAULT_OFFSET: f64 = 0.0;
/// Minimum chat-text-size offset the UI is willing to step down to. Used
/// by the +/- stepper to clamp and to disable the "-" button at the floor.
pub const MIN_OFFSET: f64 = -5.0;
/// Maximum chat-text-size offset the UI is willing to step up to. Used
/// by the +/- stepper to clamp and to disable the "+" button at the ceiling.
pub const MAX_OFFSET: f64 = 5.0;

fn data_dir() -> PathBuf {
    glib::user_data_dir().join("openbubbles-gtk")
}

fn state_path() -> PathBuf {
    data_dir().join(STATE_FILE)
}

/// Read the saved text size offset (in points), or the default if nothing is
/// saved yet.
pub fn get() -> f64 {
    let data = match std::fs::read_to_string(state_path()) {
        Ok(d) => d,
        Err(_) => return DEFAULT_OFFSET,
    };
    let val: f64 = data.trim().parse().unwrap_or(DEFAULT_OFFSET);
    if (MIN_OFFSET..=MAX_OFFSET).contains(&val) {
        val
    } else {
        DEFAULT_OFFSET
    }
}

/// Save a text size offset (in points) to disk. Creates the parent directory
/// if needed.
pub fn set(val: f64) {
    let clamped = val.clamp(MIN_OFFSET, MAX_OFFSET);
    let path = state_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, format!("{:.1}", clamped));
}
