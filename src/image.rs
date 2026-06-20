//! Decode HEIC/HEIF images to RGBA pixel data.
//!
//! This module provides the low-level decode step that turns an Apple HEIC photo
//! on disk into tightly-packed RGBA pixels so the caller can wrap them in a
//! [`gdk::MemoryTexture`] and render the image in the chat.
//!
//! The production implementation uses [`libheif-rs`] under the hood.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gtk::gdk_pixbuf::prelude::PixbufLoaderExt;

/// Tightly-packed RGBA pixel data, row-major, stride = width × 4.
///
/// Every pixel is four bytes: R, G, B, A (each 0–255).
#[derive(Debug, PartialEq)]
pub struct DecodedRgba {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// Errors that can occur when loading an image.
#[derive(Debug, PartialEq)]
pub enum ImageLoadError {
    /// The file at the given path does not exist.
    FileNotFound,
    /// The file could not be decoded (wrong format, truncated, corrupt, …).
    DecodeFailed(String),
}

impl std::fmt::Display for ImageLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImageLoadError::FileNotFound => write!(f, "file not found"),
            ImageLoadError::DecodeFailed(msg) => write!(f, "decode failed: {msg}"),
        }
    }
}

impl std::error::Error for ImageLoadError {}

/// Decode a HEIC/HEIF image at `path` into tightly-packed RGBA pixels.
///
/// Returns [`ImageLoadError::FileNotFound`] when the path doesn't exist, and
/// [`ImageLoadError::DecodeFailed`] when the file isn't a valid HEIC/HEIF or
/// the underlying decoder fails.
pub fn decode_heic_to_rgba(path: &Path) -> Result<DecodedRgba, ImageLoadError> {
    use libheif_rs::{ColorSpace, HeifContext, LibHeif, RgbChroma};

    // Read the file into bytes first so we can distinguish NotFound from
    // other I/O errors before handing the data to libheif.
    let bytes = std::fs::read(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ImageLoadError::FileNotFound
        } else {
            ImageLoadError::DecodeFailed(e.to_string())
        }
    })?;

    let ctx = HeifContext::read_from_bytes(&bytes)
        .map_err(|e| ImageLoadError::DecodeFailed(e.to_string()))?;
    let handle = ctx
        .primary_image_handle()
        .map_err(|e| ImageLoadError::DecodeFailed(e.to_string()))?;
    let width = handle.width();
    let height = handle.height();

    let heif = LibHeif::new();
    let img = heif
        .decode(&handle, ColorSpace::Rgb(RgbChroma::Rgba), None)
        .map_err(|e| ImageLoadError::DecodeFailed(e.to_string()))?;

    let plane = img
        .planes()
        .interleaved
        .ok_or_else(|| ImageLoadError::DecodeFailed("no interleaved RGBA plane".into()))?;

    // Copy row-by-row to strip stride padding, producing tightly-packed RGBA.
    let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height as usize {
        let row_start = y * plane.stride;
        pixels.extend_from_slice(
            &plane.data[row_start..row_start + width as usize * 4],
        );
    }

    Ok(DecodedRgba {
        width,
        height,
        pixels,
    })
}

/// Read the EXIF Orientation tag (TIFF tag 0x0112) from a JPEG byte buffer.
///
/// Returns `Some(n)` where `n` is 1..=8 when the image has a valid Orientation
/// tag in IFD0, or `None` if any part of the JPEG/EXIF/TIFF structure is
/// missing, truncated, or malformed.  Never panics.
pub fn read_exif_orientation(bytes: &[u8]) -> Option<u8> {
    // Must begin with JPEG SOI marker
    if bytes.len() < 2 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }

    let mut pos = 2usize;
    while pos + 3 < bytes.len() {
        if bytes[pos] != 0xFF {
            return None; // expected a marker
        }
        let marker = bytes[pos + 1];

        match marker {
            0xE1 => {
                // APP1 — Exif segment
                let seg_len =
                    u16::from_be_bytes([bytes[pos + 2], bytes[pos + 3]]) as usize;
                // seg_len includes the 2 length bytes
                if seg_len < 2 || pos + 2 + seg_len > bytes.len() {
                    return None;
                }
                // Payload starts after FF E1 <len_be16>
                let payload = &bytes[pos + 4..pos + 2 + seg_len];
                if payload.len() < 6 || &payload[0..6] != b"Exif\0\0" {
                    return None;
                }
                return parse_tiff_orientation(&payload[6..]);
            }
            0xD9 | 0xDA => {
                // EOI (end of image) or SOS (start of scan) — no more metadata
                return None;
            }
            0xD0..=0xD8 => {
                // Markers with no length field (RST markers / SOI)
                pos += 2;
                continue;
            }
            _ => {
                // All other markers carry a 2-byte length field
                let seg_len =
                    u16::from_be_bytes([bytes[pos + 2], bytes[pos + 3]]) as usize;
                if seg_len < 2 {
                    return None;
                }
                pos += 2 + seg_len;
            }
        }
    }

    None
}

/// Parse the TIFF structure inside a raw Exif block, looking for the
/// Orientation tag (0x0112) in IFD0.
///
/// `data` must start at the TIFF byte-order bytes (i.e. skip "Exif\0\0").
fn parse_tiff_orientation(data: &[u8]) -> Option<u8> {
    if data.len() < 8 {
        return None;
    }

    // TIFF byte order
    let le = match &data[0..2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };

    // TIFF magic: 0x002A
    let magic = if le {
        u16::from_le_bytes([data[2], data[3]])
    } else {
        u16::from_be_bytes([data[2], data[3]])
    };
    if magic != 0x002A {
        return None;
    }

    // IFD0 offset — 4 bytes starting at offset 4
    let ifd0_off = if le {
        u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize
    } else {
        u32::from_be_bytes([data[4], data[5], data[6], data[7]]) as usize
    };

    if ifd0_off + 2 > data.len() {
        return None;
    }

    // Number of IFD0 entries (2 bytes at IFD0 offset)
    let entry_count = if le {
        u16::from_le_bytes([data[ifd0_off], data[ifd0_off + 1]]) as usize
    } else {
        u16::from_be_bytes([data[ifd0_off], data[ifd0_off + 1]]) as usize
    };

    // Scan each 12-byte IFD entry
    for i in 0..entry_count {
        let entry_off = ifd0_off + 2 + i * 12;
        if entry_off + 12 > data.len() {
            return None;
        }

        let tag = if le {
            u16::from_le_bytes([data[entry_off], data[entry_off + 1]])
        } else {
            u16::from_be_bytes([data[entry_off], data[entry_off + 1]])
        };

        if tag != 0x0112 {
            continue;
        }

        // Orientation entry found — verify type is SHORT (0x0003) and count is 1
        let typ = if le {
            u16::from_le_bytes([data[entry_off + 2], data[entry_off + 3]])
        } else {
            u16::from_be_bytes([data[entry_off + 2], data[entry_off + 3]])
        };

        let count = if le {
            u32::from_le_bytes([
                data[entry_off + 4],
                data[entry_off + 5],
                data[entry_off + 6],
                data[entry_off + 7],
            ])
        } else {
            u32::from_be_bytes([
                data[entry_off + 4],
                data[entry_off + 5],
                data[entry_off + 6],
                data[entry_off + 7],
            ])
        };

        if typ != 0x0003 || count != 1 {
            return None;
        }

        // For SHORT count=1 the value lives in the first 2 bytes of the
        // 4-byte value-or-offset field; the remaining 2 bytes are padding.
        let value = if le {
            u16::from_le_bytes([data[entry_off + 8], data[entry_off + 9]])
        } else {
            u16::from_be_bytes([data[entry_off + 8], data[entry_off + 9]])
        };

        if (1..=8).contains(&value) {
            return Some(value as u8);
        }
        return None;
    }

    None
}

/// Apply an EXIF orientation transform (1–8) to decoded RGBA pixel data.
///
/// Orientation 1 returns the input unchanged. Orientations 2–8 produce a
/// correctly rotated/mirrored image per the TIFF/EXIF spec.
/// Out-of-range values (0, 9+) are treated as identity.
///
/// Pixel-transform formulas (stored → display):
///  1: identity
///  2: mirror horizontal  — (x, y) → (width-1-x, y)
///  3: 180°               — (x, y) → (width-1-x, height-1-y)
///  4: mirror vertical    — (x, y) → (x, height-1-y)
///  5: mirror horiz + 270 — (x, y) → (height-1-y, width-1-x)
///  6: 90° CW             — (x, y) → (height-1-y, x)
///  7: mirror horiz + 90  — (x, y) → (y, x)
///  8: 90° CCW            — (x, y) → (y, width-1-x)
pub fn apply_orientation(decoded: DecodedRgba, orientation: u8) -> DecodedRgba {
    let w = decoded.width as usize;
    let h = decoded.height as usize;
    let stride = w * 4;

    match orientation {
        2 => {
            // Mirror horizontal: stored(x,y) → display(width-1-x, y)
            let mut pixels = vec![0u8; decoded.pixels.len()];
            for y in 0..h {
                let src_base = y * stride;
                let dst_base = y * stride; // same row
                for x in 0..w {
                    let src_idx = src_base + x * 4;
                    let dst_x = w - 1 - x;
                    let dst_idx = dst_base + dst_x * 4;
                    pixels[dst_idx..dst_idx + 4]
                        .copy_from_slice(&decoded.pixels[src_idx..src_idx + 4]);
                }
            }
            DecodedRgba { width: decoded.width, height: decoded.height, pixels }
        }
        3 => {
            // 180°: stored(x,y) → display(width-1-x, height-1-y)
            let mut pixels = vec![0u8; decoded.pixels.len()];
            for y in 0..h {
                let src_base = y * stride;
                let dst_y = h - 1 - y;
                let dst_base = dst_y * stride;
                for x in 0..w {
                    let src_idx = src_base + x * 4;
                    let dst_x = w - 1 - x;
                    let dst_idx = dst_base + dst_x * 4;
                    pixels[dst_idx..dst_idx + 4]
                        .copy_from_slice(&decoded.pixels[src_idx..src_idx + 4]);
                }
            }
            DecodedRgba { width: decoded.width, height: decoded.height, pixels }
        }
        4 => {
            // Mirror vertical: stored(x,y) → display(x, height-1-y)
            let mut pixels = vec![0u8; decoded.pixels.len()];
            for y in 0..h {
                let src_base = y * stride;
                let dst_y = h - 1 - y;
                let dst_base = dst_y * stride;
                for x in 0..w {
                    let src_idx = src_base + x * 4;
                    let dst_idx = dst_base + x * 4; // same column
                    pixels[dst_idx..dst_idx + 4]
                        .copy_from_slice(&decoded.pixels[src_idx..src_idx + 4]);
                }
            }
            DecodedRgba { width: decoded.width, height: decoded.height, pixels }
        }
        5 => {
            // Mirror horiz + rotate 270° CW: (x,y) → (h-1-y, w-1-x)
            let new_w = h;
            let new_h = w;
            let new_stride = new_w * 4;
            let mut pixels = vec![0u8; new_w * new_h * 4];
            for y in 0..h {
                let src_base = y * stride;
                for x in 0..w {
                    let src_idx = src_base + x * 4;
                    let dst_x = h - 1 - y;
                    let dst_y = w - 1 - x;
                    let dst_idx = dst_y * new_stride + dst_x * 4;
                    pixels[dst_idx..dst_idx + 4]
                        .copy_from_slice(&decoded.pixels[src_idx..src_idx + 4]);
                }
            }
            DecodedRgba { width: new_w as u32, height: new_h as u32, pixels }
        }
        6 => {
            // Rotate 90° CW: (x,y) → (h-1-y, x)
            let new_w = h;
            let new_h = w;
            let new_stride = new_w * 4;
            let mut pixels = vec![0u8; new_w * new_h * 4];
            for y in 0..h {
                let src_base = y * stride;
                for x in 0..w {
                    let src_idx = src_base + x * 4;
                    let dst_x = h - 1 - y;
                    let dst_y = x;
                    let dst_idx = dst_y * new_stride + dst_x * 4;
                    pixels[dst_idx..dst_idx + 4]
                        .copy_from_slice(&decoded.pixels[src_idx..src_idx + 4]);
                }
            }
            DecodedRgba { width: new_w as u32, height: new_h as u32, pixels }
        }
        7 => {
            // Mirror horiz + rotate 90° CW: (x,y) → (y, x)
            let new_w = h;
            let new_h = w;
            let new_stride = new_w * 4;
            let mut pixels = vec![0u8; new_w * new_h * 4];
            for y in 0..h {
                let src_base = y * stride;
                for x in 0..w {
                    let src_idx = src_base + x * 4;
                    let dst_x = y;
                    let dst_y = x;
                    let dst_idx = dst_y * new_stride + dst_x * 4;
                    pixels[dst_idx..dst_idx + 4]
                        .copy_from_slice(&decoded.pixels[src_idx..src_idx + 4]);
                }
            }
            DecodedRgba { width: new_w as u32, height: new_h as u32, pixels }
        }
        8 => {
            // Rotate 90° CCW: (x,y) → (y, w-1-x)
            let new_w = h;
            let new_h = w;
            let new_stride = new_w * 4;
            let mut pixels = vec![0u8; new_w * new_h * 4];
            for y in 0..h {
                let src_base = y * stride;
                for x in 0..w {
                    let src_idx = src_base + x * 4;
                    let dst_x = y;
                    let dst_y = w - 1 - x;
                    let dst_idx = dst_y * new_stride + dst_x * 4;
                    pixels[dst_idx..dst_idx + 4]
                        .copy_from_slice(&decoded.pixels[src_idx..src_idx + 4]);
                }
            }
            DecodedRgba { width: new_w as u32, height: new_h as u32, pixels }
        }
        // 1 and any out-of-range value (0, 9+): identity
        _ => decoded,
    }
}

/// Read the file at `path`, decode it (JPEG/PNG/etc. via gdk-pixbuf; HEIC/HEIF
/// via libheif), apply EXIF orientation (JPEG/PNG only — libheif applies it
/// internally), and return tightly-packed RGBA pixels.
///
/// This is a synchronous, CPU/memory-bound function intended to be called from
/// `tokio::task::spawn_blocking` — never call it on the GTK main thread.
pub fn decode_image_rgba(path: &Path) -> Result<DecodedRgba, ImageLoadError> {
    // HEIC/HEIF: delegate to libheif (applies EXIF orientation internally).
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    if matches!(ext.as_deref(), Some("heic") | Some("heif")) {
        return decode_heic_to_rgba(path);
    }

    let bytes = std::fs::read(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ImageLoadError::FileNotFound
        } else {
            ImageLoadError::DecodeFailed(e.to_string())
        }
    })?;

    let orientation = read_exif_orientation(&bytes).unwrap_or(1);

    // Decode from memory via gdk-pixbuf
    let loader = gtk::gdk_pixbuf::PixbufLoader::new();
    loader
        .write(&bytes)
        .map_err(|e| ImageLoadError::DecodeFailed(e.to_string()))?;
    loader
        .close()
        .map_err(|e| ImageLoadError::DecodeFailed(e.to_string()))?;
    let pb = loader
        .pixbuf()
        .ok_or_else(|| ImageLoadError::DecodeFailed("no pixbuf from PixbufLoader".into()))?;

    let w = pb.width() as u32;
    let h = pb.height() as u32;
    let nch = pb.n_channels() as usize;
    let stride = pb.rowstride() as usize;
    let src = pb.read_pixel_bytes();
    let src = src.as_ref();

    // Copy to tightly-packed RGBA (strip stride padding)
    let mut pixels = Vec::with_capacity(w as usize * h as usize * 4);
    for y in 0..h as usize {
        let row = &src[y * stride..y * stride + w as usize * nch];
        for px in row.chunks_exact(nch) {
            pixels.push(px[0]);
            pixels.push(px[1]);
            pixels.push(px[2]);
            pixels.push(if nch == 4 { px[3] } else { 0xff });
        }
    }

    let decoded = DecodedRgba {
        width: w,
        height: h,
        pixels,
    };
    Ok(apply_orientation(decoded, orientation))
}

/// Dispatch `work` for each path on a `spawn_blocking` thread and call
/// `deliver` with the result.  All items run in parallel — the call returns
/// immediately after launching the tasks.
///
/// The deliver closure runs from the async context that awaits the
/// `spawn_blocking` JoinHandle (a tokio worker thread).  If the underlying
/// work task panics or the JoinHandle returns an error, the result forwarded
/// to deliver is `Err(ImageLoadError::DecodeFailed(...))`.
///
/// Called by [`schedule_image_loads`] and scheduler tests.
pub(crate) fn schedule_parallel<W, D>(items: Vec<PathBuf>, work: W, deliver: D)
where
    W: Fn(&Path) -> Result<DecodedRgba, ImageLoadError> + Send + Sync + 'static,
    D: Fn(Result<DecodedRgba, ImageLoadError>) + Send + Sync + 'static,
{
    let work = Arc::new(work);
    let deliver = Arc::new(deliver);
    for path in items {
        let work = Arc::clone(&work);
        let deliver = Arc::clone(&deliver);
        crate::runtime::runtime().spawn(async move {
            let result = tokio::task::spawn_blocking(move || work(&path)).await;
            let result = match result {
                Ok(r) => r,
                Err(e) => Err(ImageLoadError::DecodeFailed(e.to_string())),
            };
            deliver(result);
        });
    }
}

/// Production wrapper around the parallel scheduler: decodes images at the
/// given paths (via [`decode_image_rgba`]) on `spawn_blocking` threads and
/// invokes `on_each` on the GTK main thread with each result as it arrives.
///
/// `on_each` does not need to be `Send` — delivery is always on the main
/// thread.  The outer dispatcher uses a channel to ferry results from the
/// tokio worker threads to the GTK main loop.
pub fn schedule_image_loads<F>(items: Vec<PathBuf>, on_each: F)
where
    F: Fn(Result<DecodedRgba, ImageLoadError>) + Clone + 'static,
{
    // Channel ferries results from tokio workers → GTK main thread.
    // async_channel::Sender is Send + Sync, so the deliver closure below
    // satisfies schedule_parallel's D: Send + Sync bound.
    let (tx, rx) = async_channel::unbounded::<Result<DecodedRgba, ImageLoadError>>();

    schedule_parallel(
        items,
        |path: &Path| decode_image_rgba(path),
        move |result| {
            // send on an unbounded channel is non-blocking; send_blocking
            // on an unbounded channel never blocks.
            let _ = tx.send_blocking(result);
        },
    );

    // Drain on the GTK main thread and invoke the user's !Send callback.
    glib::spawn_future_local(async move {
        while let Ok(result) = rx.recv().await {
            on_each(result);
        }
    });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The real HEIC fixture, embedded so we don't depend on a run-time working
    /// directory.  4×4 pixel, lossless-encoded RGBA.
    const HEIC_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/sample.heic");

    // -----------------------------------------------------------------------
    // Test 1 – real HEIC decodes to expected dimensions & pixel data
    // -----------------------------------------------------------------------

    #[test]
    fn heic_fixture_decodes_to_expected_dimensions_and_pixel_data() {
        let temp = tempfile::NamedTempFile::new().expect("create temp file");
        std::fs::write(temp.path(), HEIC_FIXTURE).expect("write fixture bytes");

        let result = decode_heic_to_rgba(temp.path());

        assert!(
            result.is_ok(),
            "expected Ok(DecodedRgba), got Err: {:?}",
            result
        );
        let decoded = result.unwrap();
        assert_eq!(decoded.width, 4, "width should be 4");
        assert_eq!(decoded.height, 4, "height should be 4");
        assert_eq!(
            decoded.pixels.len(),
            4 * 4 * 4,
            "pixel buffer should be tightly-packed RGBA (4×4×4 = 64 bytes)"
        );

        // At least some pixel components must be non-zero — the decoder returned
        // real image data, not a zero-filled placeholder.
        let nonzero_count = decoded.pixels.iter().filter(|&&b| b != 0).count();
        assert!(
            nonzero_count > 0,
            "decoded pixel data is all zeros — no real data decoded"
        );
    }

    // -----------------------------------------------------------------------
    // Test 2 – decode_image_rgba dispatches HEIC to libheif (regression)
    // -----------------------------------------------------------------------

    #[test]
    fn decode_image_rgba_dispatches_heic_to_libheif() {
        // Regression test: decode_image_rgba (the async-scheduler work
        // function) must handle HEIC files, not just JPEG/PNG. Previously
        // it only knew gdk-pixbuf, so HEIC previews rendered as empty
        // placeholders forever — the deliver callback's `if let Ok` branch
        // was skipped when the work returned Err.
        //
        // The fix: decode_image_rgba should detect `.heic`/`.heif` paths
        // and delegate to decode_heic_to_rgba (which uses libheif-rs).
        // libheif applies EXIF orientation by default, so we do not call
        // apply_orientation on the result.

        let temp = tempfile::Builder::new()
            .suffix(".heic")
            .tempfile()
            .expect("create temp .heic file");
        std::fs::write(temp.path(), HEIC_FIXTURE).expect("write HEIC fixture bytes");

        let result = decode_image_rgba(temp.path());

        assert!(
            result.is_ok(),
            "expected Ok(DecodedRgba) for .heic file, got Err: {:?}",
            result
        );
        let decoded = result.unwrap();
        assert_eq!(decoded.width, 4, "width should be 4");
        assert_eq!(decoded.height, 4, "height should be 4");
        assert_eq!(
            decoded.pixels.len(),
            4 * 4 * 4,
            "pixel buffer should be tightly-packed RGBA (4×4×4 = 64 bytes)"
        );
        let nonzero = decoded.pixels.iter().filter(|&&b| b != 0).count();
        assert!(
            nonzero > 0,
            "decoded pixel data is all zeros — no real data decoded"
        );
    }

    // -----------------------------------------------------------------------
    // Test 3 – missing file returns Err
    // -----------------------------------------------------------------------

    #[test]
    fn heic_decode_returns_err_for_missing_file() {
        let path = std::path::PathBuf::from("/nonexistent/path/photo.heic");

        let result = decode_heic_to_rgba(&path);

        assert!(
            result.is_err(),
            "expected Err for missing file, got Ok: {:?}",
            result
        );
    }

    // -----------------------------------------------------------------------
    // Test 3 – garbage content returns Err
    // -----------------------------------------------------------------------

    #[test]
    fn heic_decode_returns_err_for_invalid_heic_file() {
        let temp = tempfile::NamedTempFile::new().expect("create temp file");
        std::fs::write(temp.path(), b"not a real heic file").expect("write garbage");

        let result = decode_heic_to_rgba(temp.path());

        assert!(
            result.is_err(),
            "expected Err for garbage content, got Ok: {:?}",
            result
        );
    }

    // =======================================================================
    // EXIF orientation: read_exif_orientation(&[u8]) -> Option<u8>
    // =======================================================================

    /// Build a minimal JPEG byte buffer containing an APP1 Exif segment with
    /// the TIFF IFD0 Orientation tag (0x0112, SHORT, count=1) set to `value`.
    ///
    /// `byte_order` must be `b"II"` (little-endian) or `b"MM"` (big-endian).
    fn jpeg_with_exif_orientation(byte_order: &[u8; 2], value: u8) -> Vec<u8> {
        let le = byte_order == b"II";

        // TIFF header (8 bytes)
        let mut tiff = Vec::with_capacity(22);
        tiff.extend_from_slice(byte_order);
        if le {
            tiff.extend_from_slice(&[0x2A, 0x00]); // magic 0x002A LE
            tiff.extend_from_slice(&[0x08, 0x00, 0x00, 0x00]); // IFD0 offset = 8 LE
            // IFD0: entry count = 1 LE
            tiff.extend_from_slice(&[0x01, 0x00]);
            // Orientation entry: tag 0x0112, type 0x0003 (SHORT), count 1
            tiff.extend_from_slice(&[0x12, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00]);
            // Value in first 2 bytes (SHORT LE), remaining 2 bytes padding
            tiff.extend_from_slice(&[value, 0x00, 0x00, 0x00]);
        } else {
            tiff.extend_from_slice(&[0x00, 0x2A]); // magic 0x002A BE
            tiff.extend_from_slice(&[0x00, 0x00, 0x00, 0x08]); // IFD0 offset = 8 BE
            // IFD0: entry count = 1 BE
            tiff.extend_from_slice(&[0x00, 0x01]);
            // Orientation entry: tag 0x0112, type 0x0003 (SHORT), count 1
            tiff.extend_from_slice(&[0x01, 0x12, 0x00, 0x03, 0x00, 0x00, 0x00, 0x01]);
            // Value in first 2 bytes (SHORT BE), remaining 2 bytes padding
            tiff.extend_from_slice(&[0x00, value, 0x00, 0x00]);
        }
        // Next IFD offset = 0 (no more IFDs)
        tiff.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

        // Build APP1: 0xFF 0xE1 <len_be16> "Exif\0\0" <tiff>
        let exif_tag = b"Exif\0\0";
        let app1_payload: Vec<u8> = exif_tag.iter().chain(tiff.iter()).copied().collect();
        // APP1 length includes the 2 length bytes but not the 0xFF 0xE1 marker
        let app1_len_be = ((app1_payload.len() + 2) as u16).to_be_bytes();

        let mut jpeg = Vec::with_capacity(4 + 2 + app1_payload.len() + 2);
        jpeg.extend_from_slice(&[0xFF, 0xD8]); // SOI
        jpeg.extend_from_slice(&[0xFF, 0xE1]); // APP1 marker
        jpeg.extend_from_slice(&app1_len_be); // segment length
        jpeg.extend_from_slice(&app1_payload); // "Exif\0\0" + TIFF
        jpeg.extend_from_slice(&[0xFF, 0xD9]); // EOI
        jpeg
    }

    #[test]
    fn read_exif_orientation_no_exif() {
        // Minimal JPEG with no APP1 segments at all
        assert_eq!(read_exif_orientation(&[0xFF, 0xD8, 0xFF, 0xD9]), None);
    }

    #[test]
    fn read_exif_orientation_orientation_1() {
        let jpeg = jpeg_with_exif_orientation(b"II", 1);
        assert_eq!(read_exif_orientation(&jpeg), Some(1));
    }

    #[test]
    fn read_exif_orientation_orientation_6() {
        let jpeg = jpeg_with_exif_orientation(b"II", 6);
        assert_eq!(read_exif_orientation(&jpeg), Some(6));
    }

    #[test]
    fn read_exif_orientation_big_endian_mm() {
        let jpeg = jpeg_with_exif_orientation(b"MM", 8);
        assert_eq!(read_exif_orientation(&jpeg), Some(8));
    }

    #[test]
    fn read_exif_orientation_out_of_range() {
        assert_eq!(
            read_exif_orientation(&jpeg_with_exif_orientation(b"II", 0)),
            None,
        );
        assert_eq!(
            read_exif_orientation(&jpeg_with_exif_orientation(b"II", 9)),
            None,
        );
    }

    #[test]
    fn read_exif_orientation_garbage() {
        // Random non-JPEG bytes
        assert_eq!(read_exif_orientation(b"garbage bytes"), None);
        // Truncated JPEG — half of SOI
        assert_eq!(read_exif_orientation(&[0xFF]), None);
        // Valid SOI then truncated (no EOI, no APP1)
        assert_eq!(read_exif_orientation(&[0xFF, 0xD8]), None);
        // Must not panic on empty slice
        let result = std::panic::catch_unwind(|| read_exif_orientation(b""));
        assert!(result.is_ok(), "read_exif_orientation panicked on empty input");
        assert_eq!(result.unwrap(), None);
    }

    // =======================================================================
    // Orientation transform: apply_orientation(DecodedRgba, u8) -> DecodedRgba
    // =======================================================================

    /// Build a small RGBA image where pixel at (x, y) has RGBA value
    /// (x*50+1, y*50+1, 128, 255) — every pixel is unique for small grids.
    fn make_test_rgba(width: u32, height: u32) -> DecodedRgba {
        let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
        for y in 0..height {
            for x in 0..width {
                pixels.push((x * 50 + 1) as u8); // R
                pixels.push((y * 50 + 1) as u8); // G
                pixels.push(128u8);               // B
                pixels.push(255u8);               // A
            }
        }
        DecodedRgba {
            width,
            height,
            pixels,
        }
    }

    /// Read the RGBA value of a single pixel from a tightly-packed buffer.
    fn pixel_at(rgba: &DecodedRgba, x: u32, y: u32) -> [u8; 4] {
        let idx = (y * rgba.width + x) as usize * 4;
        [
            rgba.pixels[idx],
            rgba.pixels[idx + 1],
            rgba.pixels[idx + 2],
            rgba.pixels[idx + 3],
        ]
    }

    /// Factory that maps a (x,y) position to its RGBA value, matching
    /// `make_test_rgba`.
    fn expected_rgba(x: u32, y: u32) -> [u8; 4] {
        [(x * 50 + 1) as u8, (y * 50 + 1) as u8, 128, 255]
    }

    #[test]
    fn apply_orientation_identity() {
        let input = make_test_rgba(2, 2);
        let expected_pixels = input.pixels.clone();
        let result = apply_orientation(input, 1);
        assert_eq!(result.width, 2);
        assert_eq!(result.height, 2);
        assert_eq!(result.pixels, expected_pixels);
    }

    #[test]
    fn apply_orientation_90_cw() {
        // 2 × 3  → 90° CW → 3 × 2
        // 90° CW: stored(x,y) → display(height-1-y, x)
        let input = make_test_rgba(2, 3);
        let result = apply_orientation(input, 6);
        assert_eq!(result.width, 3, "width should become original height (3)");
        assert_eq!(result.height, 2, "height should become original width (2)");

        assert_eq!(pixel_at(&result, 0, 0), expected_rgba(0, 2));
        assert_eq!(pixel_at(&result, 1, 0), expected_rgba(0, 1));
        assert_eq!(pixel_at(&result, 2, 0), expected_rgba(0, 0));
        assert_eq!(pixel_at(&result, 0, 1), expected_rgba(1, 2));
        assert_eq!(pixel_at(&result, 1, 1), expected_rgba(1, 1));
        assert_eq!(pixel_at(&result, 2, 1), expected_rgba(1, 0));
    }

    #[test]
    fn apply_orientation_90_ccw() {
        // 2 × 3  → 90° CCW → 3 × 2
        // 90° CCW: stored(x,y) → display(y, width-1-x)
        let input = make_test_rgba(2, 3);
        let result = apply_orientation(input, 8);
        assert_eq!(result.width, 3);
        assert_eq!(result.height, 2);

        assert_eq!(pixel_at(&result, 0, 0), expected_rgba(1, 0));
        assert_eq!(pixel_at(&result, 1, 0), expected_rgba(1, 1));
        assert_eq!(pixel_at(&result, 2, 0), expected_rgba(1, 2));
        assert_eq!(pixel_at(&result, 0, 1), expected_rgba(0, 0));
        assert_eq!(pixel_at(&result, 1, 1), expected_rgba(0, 1));
        assert_eq!(pixel_at(&result, 2, 1), expected_rgba(0, 2));
    }

    #[test]
    fn apply_orientation_180() {
        // 2 × 3  → 180° → 2 × 3
        // 180°: stored(x,y) → display(width-1-x, height-1-y)
        let input = make_test_rgba(2, 3);
        let result = apply_orientation(input, 3);
        assert_eq!(result.width, 2);
        assert_eq!(result.height, 3);

        assert_eq!(pixel_at(&result, 0, 0), expected_rgba(1, 2));
        assert_eq!(pixel_at(&result, 1, 0), expected_rgba(0, 2));
        assert_eq!(pixel_at(&result, 0, 1), expected_rgba(1, 1));
        assert_eq!(pixel_at(&result, 1, 1), expected_rgba(0, 1));
        assert_eq!(pixel_at(&result, 0, 2), expected_rgba(1, 0));
        assert_eq!(pixel_at(&result, 1, 2), expected_rgba(0, 0));
    }

    #[test]
    fn apply_orientation_mirror_horizontal() {
        // 2 × 3  → mirror horizontal → 2 × 3
        // Flip left-right: stored(x,y) → display(width-1-x, y)
        let input = make_test_rgba(2, 3);
        let result = apply_orientation(input, 2);
        assert_eq!(result.width, 2);
        assert_eq!(result.height, 3);

        assert_eq!(pixel_at(&result, 0, 0), expected_rgba(1, 0));
        assert_eq!(pixel_at(&result, 1, 0), expected_rgba(0, 0));
        assert_eq!(pixel_at(&result, 0, 1), expected_rgba(1, 1));
        assert_eq!(pixel_at(&result, 1, 1), expected_rgba(0, 1));
        assert_eq!(pixel_at(&result, 0, 2), expected_rgba(1, 2));
        assert_eq!(pixel_at(&result, 1, 2), expected_rgba(0, 2));
    }

    #[test]
    fn apply_orientation_mirror_vertical() {
        // 2 × 3  → mirror vertical → 2 × 3
        // Flip top-bottom: stored(x,y) → display(x, height-1-y)
        let input = make_test_rgba(2, 3);
        let result = apply_orientation(input, 4);
        assert_eq!(result.width, 2);
        assert_eq!(result.height, 3);

        assert_eq!(pixel_at(&result, 0, 0), expected_rgba(0, 2));
        assert_eq!(pixel_at(&result, 1, 0), expected_rgba(1, 2));
        assert_eq!(pixel_at(&result, 0, 1), expected_rgba(0, 1));
        assert_eq!(pixel_at(&result, 1, 1), expected_rgba(1, 1));
        assert_eq!(pixel_at(&result, 0, 2), expected_rgba(0, 0));
        assert_eq!(pixel_at(&result, 1, 2), expected_rgba(1, 0));
    }

    #[test]
    fn apply_orientation_mirror_rotate_combos() {
        // Orientation 5: Mirror horizontal + rotate 270° CW
        //   = rotate 90° CW then flip top-bottom
        // Formula for stored(x,y) → display:
        //   1. Rotate 90° CW: (x,y) → (h-1-y, x)    [output: width=h, height=w]
        //   2. Mirror v:     (x',y') → (x', h'-1-y') [h' = w]
        //   Total: (x,y) → (h-1-y, w-1-x)

        let input = make_test_rgba(2, 3);
        let result5 = apply_orientation(input, 5);
        assert_eq!(
            result5.width, 3,
            "orientation 5: width should become original height (3)"
        );
        assert_eq!(
            result5.height, 2,
            "orientation 5: height should become original width (2)"
        );

        assert_eq!(pixel_at(&result5, 0, 0), expected_rgba(1, 2));
        assert_eq!(pixel_at(&result5, 1, 0), expected_rgba(1, 1));
        assert_eq!(pixel_at(&result5, 2, 0), expected_rgba(1, 0));
        assert_eq!(pixel_at(&result5, 0, 1), expected_rgba(0, 2));
        assert_eq!(pixel_at(&result5, 1, 1), expected_rgba(0, 1));
        assert_eq!(pixel_at(&result5, 2, 1), expected_rgba(0, 0));

        // Orientation 7: Mirror horizontal + rotate 90° CW
        //   = rotate 90° CCW then flip top-bottom
        // Formula for stored(x,y) → display:
        //   1. Rotate 90° CCW: (x,y) → (y, w-1-x)   [output: width=h, height=w]
        //   2. Mirror v:       (x',y') → (x', h'-1-y') [h' = w]
        //   Total: (x,y) → (y, w-1-(w-1-x)) = (y, x)

        let input = make_test_rgba(2, 3);
        let result7 = apply_orientation(input, 7);
        assert_eq!(result7.width, 3);
        assert_eq!(result7.height, 2);

        assert_eq!(pixel_at(&result7, 0, 0), expected_rgba(0, 0));
        assert_eq!(pixel_at(&result7, 1, 0), expected_rgba(0, 1));
        assert_eq!(pixel_at(&result7, 2, 0), expected_rgba(0, 2));
        assert_eq!(pixel_at(&result7, 0, 1), expected_rgba(1, 0));
        assert_eq!(pixel_at(&result7, 1, 1), expected_rgba(1, 1));
        assert_eq!(pixel_at(&result7, 2, 1), expected_rgba(1, 2));
    }

    // =======================================================================
    // Parallel scheduler: schedule_parallel
    //
    // These tests drive a generic parallel-scheduling function that is the
    // testable core of the image-load offloading work.  The function does not
    // exist yet — the compiler errors below are the expected "red" state.
    //
    // Proposed signature for the production function:
    //
    // ```rust,ignore
    // pub(crate) fn schedule_parallel<W, D>(
    //     items: Vec<PathBuf>,
    //     work: W,
    //     deliver: D,
    // )
    // where
    //     W: Fn(&Path) -> Result<DecodedRgba, ImageLoadError> + Send + Sync + 'static,
    //     D: Fn(Result<DecodedRgba, ImageLoadError>) + Send + Sync + 'static,
    // ```

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// Dummy path for scheduling tests — the work function never opens the file.
    fn test_path() -> PathBuf {
        PathBuf::from("/tmp/test-image.jpg")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn schedule_parallel_dispatches_one_work_call_per_item() {
        let paths: Vec<PathBuf> = (0..4)
            .map(|i| PathBuf::from(format!("/tmp/test-image-{i}.jpg")))
            .collect();

        let work_count = Arc::new(AtomicUsize::new(0));
        let deliver_count = Arc::new(AtomicUsize::new(0));
        let results: Arc<Mutex<Vec<Result<DecodedRgba, ImageLoadError>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();

        let work = {
            let wc = Arc::clone(&work_count);
            move |_path: &Path| -> Result<DecodedRgba, ImageLoadError> {
                wc.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(10));
                Ok(DecodedRgba {
                    width: 1,
                    height: 1,
                    pixels: vec![0; 4],
                })
            }
        };

        let deliver = {
            let dc = Arc::clone(&deliver_count);
            let res = Arc::clone(&results);
            let tx = tx.clone();
            move |result: Result<DecodedRgba, ImageLoadError>| {
                dc.fetch_add(1, Ordering::SeqCst);
                res.lock().unwrap().push(result);
                let _ = tx.send(());
            }
        };

        schedule_parallel(paths, work, deliver);

        for _ in 0..4 {
            rx.recv()
                .await
                .expect("channel closed before all deliveries");
        }

        assert_eq!(
            work_count.load(Ordering::SeqCst),
            4,
            "work should be called exactly once per item"
        );
        assert_eq!(
            deliver_count.load(Ordering::SeqCst),
            4,
            "deliver should be called exactly once per item"
        );
        assert_eq!(
            results.lock().unwrap().len(),
            4,
            "all results should be collected"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn schedule_parallel_runs_items_in_parallel() {
        let paths: Vec<PathBuf> = (0..4)
            .map(|i| PathBuf::from(format!("/tmp/test-image-{i}.jpg")))
            .collect();

        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();

        let work = {
            let infl = Arc::clone(&in_flight);
            let max_infl = Arc::clone(&max_in_flight);
            move |_path: &Path| -> Result<DecodedRgba, ImageLoadError> {
                let prev = infl.fetch_add(1, Ordering::SeqCst);
                let cur = prev + 1;
                max_infl.fetch_max(cur, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(50));
                infl.fetch_sub(1, Ordering::SeqCst);
                Ok(DecodedRgba {
                    width: 1,
                    height: 1,
                    pixels: vec![0; 4],
                })
            }
        };

        let deliver = {
            let tx = tx.clone();
            move |_result: Result<DecodedRgba, ImageLoadError>| {
                let _ = tx.send(());
            }
        };

        schedule_parallel(paths, work, deliver);

        for _ in 0..4 {
            rx.recv()
                .await
                .expect("channel closed before all deliveries");
        }

        let max = max_in_flight.load(Ordering::SeqCst);
        assert!(
            max >= 2,
            "expected at least 2 concurrent work items, got {max}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn schedule_parallel_propagates_errors() {
        let paths = vec![
            PathBuf::from("/tmp/missing-0.jpg"),
            test_path(),
            PathBuf::from("/tmp/missing-2.jpg"),
        ];

        let results: Arc<Mutex<Vec<Result<DecodedRgba, ImageLoadError>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();

        let work = |_path: &Path| -> Result<DecodedRgba, ImageLoadError> {
            Err(ImageLoadError::FileNotFound)
        };

        let deliver = {
            let res = Arc::clone(&results);
            let tx = tx.clone();
            move |result: Result<DecodedRgba, ImageLoadError>| {
                res.lock().unwrap().push(result);
                let _ = tx.send(());
            }
        };

        schedule_parallel(paths, work, deliver);

        for _ in 0..3 {
            rx.recv()
                .await
                .expect("channel closed before all deliveries");
        }

        let guard = results.lock().unwrap();
        assert_eq!(guard.len(), 3, "all 3 results should arrive");
        for r in guard.iter() {
            assert!(r.is_err(), "expected Err, got Ok: {r:?}");
            assert_eq!(
                r.as_ref().unwrap_err(),
                &ImageLoadError::FileNotFound
            );
        }
    }

    // =======================================================================
    // Schedule outside tokio context (regression test)
    // =======================================================================

    #[test]
    fn schedule_parallel_works_outside_tokio_context() {
        // Regression test: schedule_parallel must work when called from a
        // context with no current Tokio runtime (e.g. the GTK main thread,
        // which is where image_widget → schedule_image_loads calls into it).
        // tokio::task::spawn panics with "there is no reactor running" in
        // that context; the production code path uses
        // crate::runtime::runtime().spawn(...) which works from any context.
        //
        // This test deliberately avoids #[tokio::test] so there's no
        // ambient runtime — the call would panic if the bug regressed.

        let paths: Vec<PathBuf> = (0..2)
            .map(|i| PathBuf::from(format!("/tmp/test-outside-tokio-{i}.jpg")))
            .collect();

        let (tx, rx) = std::sync::mpsc::channel::<Result<DecodedRgba, ImageLoadError>>();
        // std::sync::mpsc::Sender is Send but not Sync, so we wrap it in
        // Arc<Mutex<…>> to satisfy the D: Send + Sync bound on the deliver
        // closure (Fn, so all captures must be Sync).
        let tx = Arc::new(Mutex::new(tx));

        let work = |_path: &Path| -> Result<DecodedRgba, ImageLoadError> {
            std::thread::sleep(std::time::Duration::from_millis(10));
            Ok(DecodedRgba {
                width: 1,
                height: 1,
                pixels: vec![0; 4],
            })
        };

        let deliver = {
            let tx = Arc::clone(&tx);
            move |result: Result<DecodedRgba, ImageLoadError>| {
                let _ = tx.lock().unwrap().send(result);
            }
        };

        // This call would panic with "there is no reactor running" if
        // schedule_parallel still uses tokio::task::spawn.
        schedule_parallel(paths, work, deliver);

        // Receive all N results, with a generous timeout so a regression
        // surfaces as a test failure (not a hung test runner).
        let mut results = Vec::with_capacity(2);
        for _ in 0..2 {
            let result = rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("schedule_parallel should deliver results even outside a Tokio context");
            results.push(result);
        }

        assert_eq!(results.len(), 2, "expected 2 results, got {}", results.len());
        for (i, r) in results.iter().enumerate() {
            assert!(r.is_ok(), "result {i} should be Ok, got {r:?}");
        }
    }
}
