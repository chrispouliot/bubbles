//! Persist and restore the chat text scale factor so the user's preference
//! survives app restarts.

use std::path::PathBuf;

use gtk::glib;

const STATE_FILE: &str = "text_scale.txt";
const DEFAULT_SCALE: f64 = 1.0;
const MIN_SCALE: f64 = 0.5;
const MAX_SCALE: f64 = 2.0;

fn data_dir() -> PathBuf {
    glib::user_data_dir().join("openbubbles-gtk")
}

fn state_path() -> PathBuf {
    data_dir().join(STATE_FILE)
}

/// Read the saved text scale, or the default if nothing is saved yet.
pub fn get() -> f64 {
    let data = match std::fs::read_to_string(state_path()) {
        Ok(d) => d,
        Err(_) => return DEFAULT_SCALE,
    };
    let val: f64 = data.trim().parse().unwrap_or(DEFAULT_SCALE);
    if val >= MIN_SCALE && val <= MAX_SCALE {
        val
    } else {
        DEFAULT_SCALE
    }
}

/// Save a text scale to disk. Creates the parent directory if needed.
pub fn set(val: f64) {
    let clamped = val.max(MIN_SCALE).min(MAX_SCALE);
    let path = state_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, format!("{:.1}", clamped));
}
