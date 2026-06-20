//! Decode HEIC/HEIF images to RGBA pixel data.
//!
//! This module provides the low-level decode step that turns an Apple HEIC photo
//! on disk into tightly-packed RGBA pixels so the caller can wrap them in a
//! [`gdk::MemoryTexture`] and render the image in the chat.
//!
//! The production implementation uses [`libheif-rs`] under the hood.

use std::path::Path;

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
    // Test 2 – missing file returns Err
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
}
