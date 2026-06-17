Implemented: time_format module — real get/set persistence and format_time with "%I:%M %p" / "%H:%M".

Changed files: src/time_format.rs

Validation: cargo test time_format — 11 passed; 0 failed

Implementation details:
- `get()`: reads state file, parses "0" → AmPm, "1" → H24, fallback to DEFAULT on missing/unparseable file
- `set()`: writes "0" or "1" to state file, creates parent directory if needed
- `format_time()`: uses `glib::DateTime::from_unix_local(ms / 1000).and_then(|dt| dt.format(...))` with "%I:%M %p" for AmPm and "%H:%M" for H24
- `state_path()`: uses per-thread file name in test mode (`#[cfg(test)]`) to avoid race conditions between parallel roundtrip tests; production uses the standard `time_format.txt` path

Open risks: none
Recommended next step: proceed to unit 2 (UI wiring)
