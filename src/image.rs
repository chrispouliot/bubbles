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

/// Try to read pixel dimensions from a PNG or JPEG byte buffer.
///
/// Returns `(width, height)` for recognised formats, or `None` if the format
/// is not recognised or the buffer is too short.  Never panics.
fn read_image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    // PNG: 8-byte signature (0x8950 4E47 0D0A 1A0A) followed by IHDR chunk.
    if bytes.len() >= 24 && &bytes[0..8] == b"\x89PNG\r\n\x1a\n" {
        let chunk_len =
            u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        if chunk_len == 13 && &bytes[12..16] == b"IHDR" {
            let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
            let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
            return Some((w, h));
        }
        return None;
    }

    // JPEG: must begin with SOI marker (0xFF 0xD8).
    if bytes.len() < 2 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }

    // Scan markers for SOF0 / SOF1 / SOF2 which carry the dimensions.
    let mut pos = 2usize;
    while pos + 7 < bytes.len() {
        if bytes[pos] != 0xFF {
            return None;
        }
        let marker = bytes[pos + 1];
        match marker {
            0xC0..=0xC2 => {
                // SOF: marker(2) + length(2) + precision(1) + height(2) + width(2)
                let seg_len =
                    u16::from_be_bytes([bytes[pos + 2], bytes[pos + 3]]) as usize;
                if seg_len < 7 || pos + 2 + seg_len > bytes.len() {
                    return None;
                }
                let h = u16::from_be_bytes([bytes[pos + 5], bytes[pos + 6]]);
                let w = u16::from_be_bytes([bytes[pos + 7], bytes[pos + 8]]);
                return Some((w as u32, h as u32));
            }
            0xD9 | 0xDA => {
                // EOI (end of image) or SOS (start of scan) — no more metadata.
                return None;
            }
            0xD0..=0xD8 => {
                // RST markers — no length field.
                pos += 2;
            }
            _ => {
                // All other markers carry a 2-byte length field.
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

/// Downscale a decoded RGBA image so the longer edge ≤ `max_edge`,
/// preserving aspect ratio.  Does nothing when `max_edge` is `None` or
/// when the image already fits within the cap (no upscaling).
fn apply_max_edge(decoded: DecodedRgba, max_edge: Option<u32>) -> DecodedRgba {
    let Some(cap) = max_edge else { return decoded; };
    let long_edge = decoded.width.max(decoded.height);
    if long_edge <= cap {
        return decoded;
    }

    let scale = cap as f64 / long_edge as f64;
    let new_w = (decoded.width as f64 * scale).round() as i32;
    let new_h = (decoded.height as f64 * scale).round() as i32;

    // Reconstruct a Pixbuf from the tightly-packed RGBA data, scale it, and
    // read back tightly-packed pixels.  This is the simplest approach that
    // works uniformly for both the gdk-pixbuf and libheif code paths.
    let stride = decoded.width as usize * 4;
    let bytes = glib::Bytes::from_owned(decoded.pixels);
    let src_pb = gtk::gdk_pixbuf::Pixbuf::from_bytes(
        &bytes,
        gtk::gdk_pixbuf::Colorspace::Rgb,
        true, // has_alpha — DecodedRgba is always RGBA
        8,    // bits per sample
        decoded.width as i32,
        decoded.height as i32,
        stride as i32,
    );

    let scaled = src_pb
        .scale_simple(new_w, new_h, gtk::gdk_pixbuf::InterpType::Bilinear)
        .expect("scale_simple should succeed for valid dimensions");

    let w = scaled.width() as u32;
    let h = scaled.height() as u32;
    let scaled_stride = scaled.rowstride() as usize;
    let src = scaled.read_pixel_bytes();
    let src = src.as_ref();

    let mut pixels = Vec::with_capacity(w as usize * h as usize * 4);
    for y in 0..h as usize {
        let row = &src[y * scaled_stride..y * scaled_stride + w as usize * 4];
        pixels.extend_from_slice(row);
    }

    DecodedRgba { width: w, height: h, pixels }
}

/// Extract a rectangular sub-region from a tightly-packed RGBA source.
///
/// Returns a new [`DecodedRgba`] with `width = w`, `height = h`, and pixels
/// copied from the region `(x, y)` .. `(x + w, y + h)` of `src`.
///
/// # Errors
///
/// Returns [`ImageLoadError::DecodeFailed`] when any part of the requested
/// region falls outside the source image bounds.
#[allow(dead_code)]
pub fn crop_rgba(
    src: &DecodedRgba,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Result<DecodedRgba, ImageLoadError> {
    let src_w = src.width;
    let src_h = src.height;

    if x + w > src_w || y + h > src_h {
        return Err(ImageLoadError::DecodeFailed(format!(
            "crop region ({x},{y}) {w}×{h} exceeds source {src_w}×{src_h}"
        )));
    }

    let stride = src_w as usize * 4;
    let row_byte_len = w as usize * 4;
    let mut pixels = Vec::with_capacity(w as usize * h as usize * 4);

    for row in 0..h {
        let src_offset = ((y + row) as usize * stride) + (x as usize * 4);
        pixels.extend_from_slice(&src.pixels[src_offset..src_offset + row_byte_len]);
    }

    Ok(DecodedRgba {
        width: w,
        height: h,
        pixels,
    })
}

/// Zero the alpha channel of every pixel whose centre lies strictly outside a
/// centred circle.
///
/// The circle is centred at `(width/2, height/2)` with radius
/// `min(width, height) / 2.0`.  Pixels inside the circle keep their original
/// alpha; pixels outside have their alpha set to zero.  RGB values are
/// unchanged.  Operates in place — no allocation.
#[allow(dead_code)]
pub fn apply_circle_mask(buf: &mut DecodedRgba) {
    let w = buf.width;
    let h = buf.height;
    let cx = w as f64 / 2.0;
    let cy = h as f64 / 2.0;
    let radius = w.min(h) as f64 / 2.0;
    let radius_sq = radius * radius;

    for py in 0..h {
        for px in 0..w {
            let dx = px as f64 - cx;
            let dy = py as f64 - cy;
            if dx * dx + dy * dy > radius_sq {
                let idx = ((py * w + px) * 4 + 3) as usize;
                buf.pixels[idx] = 0;
            }
        }
    }
}

/// Crop region in source-image coordinates (pixels of the picked image before
/// any scaling). The crop is a square centered at (cx, cy) with half-side
/// length r. The corresponding source rectangle is (cx - r, cy - r) to
/// (cx + r, cy + r), clamped to the source image bounds.
#[allow(dead_code)]
pub struct CropParams {
    pub cx: f64,
    pub cy: f64,
    pub r: f64,
}

/// Render a chat avatar from a source image and a crop selection. Returns a
/// 256×256 RGBA buffer with the cropped circle, ready to feed into `save_png`.
///
/// Steps:
///   1. Compute the source rectangle from `params`, clamped to [0, src_w] ×
///      [0, src_h]. The rectangle must be non-empty (positive width and
///      height).
///   2. Crop the source to that rectangle (using the existing `crop_rgba`).
///   3. Apply a circle mask centered in the cropped square (using the
///      existing `apply_circle_mask`).
///   4. Scale the masked square to 256×256 (use gdk-pixbuf bilinear scaling
///      — see `apply_max_edge` for the pattern).
///   5. Return the 256×256 `DecodedRgba`.
///
/// # Errors
///
/// Returns `ImageLoadError::DecodeFailed` when:
///   - The crop is zero-area (e.g. `r == 0.0`, or `r` so small that the
///     clamped rectangle is empty).
///   - The crop is entirely outside the source (the clamped rectangle is
///     empty).
///   - Any underlying I/O / decode step fails (gdk-pixbuf allocation, etc.).
#[allow(dead_code)]
pub fn render_avatar(
    src: &DecodedRgba,
    params: &CropParams,
) -> Result<DecodedRgba, ImageLoadError> {
    // Step 1: clamp the crop rectangle to the source bounds.
    let src_w = src.width as f64;
    let src_h = src.height as f64;

    let left = (params.cx - params.r).max(0.0);
    let top = (params.cy - params.r).max(0.0);
    let right = (params.cx + params.r).min(src_w);
    let bottom = (params.cy + params.r).min(src_h);

    if left >= right || top >= bottom {
        return Err(ImageLoadError::DecodeFailed(
            "crop region is empty after clamping to source bounds".to_string(),
        ));
    }

    let x = left.floor() as u32;
    let y = top.floor() as u32;
    let w = right.ceil() as u32 - x;
    let h = bottom.ceil() as u32 - y;

    if w == 0 || h == 0 {
        return Err(ImageLoadError::DecodeFailed(
            "crop region is empty after clamping to source bounds".to_string(),
        ));
    }

    // Step 2: crop
    let mut cropped = crop_rgba(src, x, y, w, h)?;

    // Step 3: apply circle mask
    apply_circle_mask(&mut cropped);

    // Step 4: scale to 256×256 via gdk-pixbuf bilinear
    let target = 256i32;
    let stride = cropped.width as usize * 4;
    let bytes = glib::Bytes::from_owned(cropped.pixels);
    let src_pb = gtk::gdk_pixbuf::Pixbuf::from_bytes(
        &bytes,
        gtk::gdk_pixbuf::Colorspace::Rgb,
        true, // has_alpha
        8,    // bits per sample
        cropped.width as i32,
        cropped.height as i32,
        stride as i32,
    );

    let scaled = src_pb
        .scale_simple(target, target, gtk::gdk_pixbuf::InterpType::Bilinear)
        .ok_or_else(|| {
            ImageLoadError::DecodeFailed("gdk-pixbuf scale_simple failed".to_string())
        })?;

    let w_out = scaled.width() as u32;
    let h_out = scaled.height() as u32;
    let scaled_stride = scaled.rowstride() as usize;
    let src_bytes = scaled.read_pixel_bytes();
    let src_bytes = src_bytes.as_ref();

    let mut pixels = Vec::with_capacity(w_out as usize * h_out as usize * 4);
    for row in 0..h_out as usize {
        let slice = &src_bytes[row * scaled_stride..row * scaled_stride + w_out as usize * 4];
        pixels.extend_from_slice(slice);
    }

    Ok(DecodedRgba {
        width: w_out,
        height: h_out,
        pixels,
    })
}

/// Write a [`DecodedRgba`] as a PNG file at `path`.
///
/// Uses gdk-pixbuf to encode the tightly-packed RGBA data as a PNG.
#[allow(dead_code)]
pub fn save_png(rgba: &DecodedRgba, path: &Path) -> Result<(), ImageLoadError> {
    let stride = rgba.width as usize * 4;
    let bytes = glib::Bytes::from_owned(rgba.pixels.clone());
    let pb = gtk::gdk_pixbuf::Pixbuf::from_bytes(
        &bytes,
        gtk::gdk_pixbuf::Colorspace::Rgb,
        true, // has_alpha — DecodedRgba is always RGBA
        8,    // bits per sample
        rgba.width as i32,
        rgba.height as i32,
        stride as i32,
    );

    let png_bytes = pb
        .save_to_bufferv("png", &[])
        .map_err(|e| ImageLoadError::DecodeFailed(e.to_string()))?;

    std::fs::write(path, png_bytes)
        .map_err(|e| ImageLoadError::DecodeFailed(e.to_string()))?;

    Ok(())
}

/// Read the file at `path`, decode it (JPEG/PNG/etc. via gdk-pixbuf; HEIC/HEIF
/// via libheif), apply EXIF orientation (JPEG/PNG only — libheif applies it
/// internally), optionally cap the longer edge to `max_edge`, and return
/// tightly-packed RGBA pixels.
///
/// When `max_edge` is `Some(n)` the decoded image is downscaled so its longer
/// side ≤ `n`, preserving aspect ratio.  Never upscales.  When `max_edge` is
/// `None` the image is returned at full resolution.
///
/// This is a synchronous, CPU/memory-bound function intended to be called from
/// `tokio::task::spawn_blocking` — never call it on the GTK main thread.
pub fn decode_image_rgba(
    path: &Path,
    max_edge: Option<u32>,
) -> Result<DecodedRgba, ImageLoadError> {
    // HEIC/HEIF: delegate to libheif (applies EXIF orientation internally).
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    if matches!(ext.as_deref(), Some("heic") | Some("heif")) {
        let decoded = decode_heic_to_rgba(path)?;
        return Ok(apply_max_edge(decoded, max_edge));
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
    if let Some(cap) = max_edge {
        // Best-effort: ask the loader to decode at the target size,
        // preserving aspect ratio.  We first read the source dimensions
        // from the file header so we can compute the correct target
        // width/height — this avoids upscaling or stretching.
        //
        // If we cannot determine the source dimensions (unrecognised
        // format) or the image already fits within the cap, we skip
        // set_size entirely; apply_max_edge at the end of this function
        // guarantees the output respects the cap.
        if let Some((img_w, img_h)) = read_image_dimensions(&bytes) {
            let long_edge = img_w.max(img_h);
            if long_edge > cap {
                let scale = cap as f64 / long_edge as f64;
                let target_w = (img_w as f64 * scale).round() as i32;
                let target_h = (img_h as f64 * scale).round() as i32;
                loader.set_size(target_w, target_h);
            }
        }
    }
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
    Ok(apply_max_edge(apply_orientation(decoded, orientation), max_edge))
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
    const MAX_CONCURRENT_DECODES: usize = 3;

    let work = Arc::new(work);
    let deliver = Arc::new(deliver);
    let sem = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_DECODES));
    for path in items {
        let work = Arc::clone(&work);
        let deliver = Arc::clone(&deliver);
        let sem = Arc::clone(&sem);
        crate::runtime::runtime().spawn(async move {
            let _permit = sem.acquire().await.expect("semaphore should not be closed");
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
/// `max_edge` is forwarded to each [`decode_image_rgba`] call — see that
/// function's documentation for details.
///
/// `on_each` does not need to be `Send` — delivery is always on the main
/// thread.  The outer dispatcher uses a channel to ferry results from the
/// tokio worker threads to the GTK main loop.
pub fn schedule_image_loads<F>(items: Vec<PathBuf>, max_edge: Option<u32>, on_each: F)
where
    F: Fn(Result<DecodedRgba, ImageLoadError>) + Clone + 'static,
{
    // Channel ferries results from tokio workers → GTK main thread.
    // async_channel::Sender is Send + Sync, so the deliver closure below
    // satisfies schedule_parallel's D: Send + Sync bound.
    let (tx, rx) = async_channel::unbounded::<Result<DecodedRgba, ImageLoadError>>();

    schedule_parallel(
        items,
        move |path: &Path| decode_image_rgba(path, max_edge),
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
    #![allow(clippy::identity_op, clippy::erasing_op)]
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

        let result = decode_image_rgba(temp.path(), None);

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
    // max_edge parameter: decode_image_rgba(path, max_edge)
    //
    // The `decode_image_rgba` and `schedule_image_loads` functions gain a
    // new `max_edge: Option<u32>` parameter.  When Some(n), the decoded
    // image is downscaled so the longer edge ≤ n, preserving aspect ratio.
    // When None, the image is decoded at full resolution.  Do NOT upscale
    // when the source is already smaller than n.
    //
    // The test below synthesises a 200×400 PNG, then calls
    // decode_image_rgba with several max_edge values and asserts the
    // returned dimensions.
    //
    // These tests will not compile until the max_edge parameter is added
    // to the production signature — that compile error IS the expected
    // red state.
    // =======================================================================

    #[test]
    fn decode_image_rgba_respects_max_edge() {
        // ---------- Create a known-size test PNG (200 × 400) ----------
        let pb = gtk::gdk_pixbuf::Pixbuf::new(
            gtk::gdk_pixbuf::Colorspace::Rgb,
            true, // has_alpha
            8,    // bits per sample
            200,  // width
            400,  // height
        )
        .expect("create 200×400 test pixbuf");

        // Fill with a simple non-zero pattern via the unsafe pixel buffer.
        let buf = unsafe { pb.pixels() };
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (i % 256) as u8;
        }

        // Save as PNG bytes and write to a temp file.
        let png_bytes = pb
            .save_to_bufferv("png", &[])
            .expect("encode test pixbuf as PNG");
        let temp = tempfile::Builder::new()
            .suffix(".png")
            .tempfile()
            .expect("create temp .png file");
        std::fs::write(temp.path(), &png_bytes)
            .expect("write PNG bytes to temp file");

        // ---------- max_edge = Some(100) → cap at 100 on long edge ----------
        // Long edge is 400, cap is 100 → scale factor = 100/400 = 0.25.
        // width: 200 * 0.25 = 50,  height: 400 * 0.25 = 100.
        let result = decode_image_rgba(temp.path(), Some(100));
        assert!(
            result.is_ok(),
            "expected Ok for max_edge=Some(100), got Err: {:?}",
            result
        );
        let decoded = result.unwrap();
        assert_eq!(
            decoded.width, 50,
            "width should be scaled to 50 (200 × 100÷400)"
        );
        assert_eq!(
            decoded.height, 100,
            "height should be capped at 100 (400 × 100÷400)"
        );

        // ---------- max_edge = None → full resolution ----------
        let result = decode_image_rgba(temp.path(), None);
        assert!(
            result.is_ok(),
            "expected Ok for max_edge=None, got Err: {:?}",
            result
        );
        let decoded = result.unwrap();
        assert_eq!(decoded.width, 200, "width should be 200 (full resolution)");
        assert_eq!(
            decoded.height, 400,
            "height should be 400 (full resolution)"
        );

        // ---------- max_edge = Some(1000) → no upscale ----------
        // Source (200×400) is already smaller than 1000 on the long edge.
        let result = decode_image_rgba(temp.path(), Some(1000));
        assert!(
            result.is_ok(),
            "expected Ok for max_edge=Some(1000), got Err: {:?}",
            result
        );
        let decoded = result.unwrap();
        assert_eq!(decoded.width, 200, "width should be 200 (no upscale)");
        assert_eq!(
            decoded.height, 400,
            "height should be 400 (no upscale)"
        );
    }

    #[test]
    fn decode_image_rgba_respects_max_edge_for_large_images() {
        // ---------- Create a large 2000×2000 test PNG (~16 MB raw) ----------
        let pb = gtk::gdk_pixbuf::Pixbuf::new(
            gtk::gdk_pixbuf::Colorspace::Rgb,
            true, // has_alpha
            8,    // bits per sample
            2000, // width
            2000, // height
        )
        .expect("create 2000×2000 test pixbuf");

        // Fill with a non-zero pattern.
        let buf = unsafe { pb.pixels() };
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (i % 256) as u8;
        }

        // Encode as PNG and write to a temp file.
        let png_bytes = pb
            .save_to_bufferv("png", &[])
            .expect("encode large test pixbuf as PNG");
        let temp = tempfile::Builder::new()
            .suffix(".png")
            .tempfile()
            .expect("create temp .png file");
        std::fs::write(temp.path(), &png_bytes)
            .expect("write large PNG bytes to temp file");

        // ---------- max_edge = Some(100) → cap at 100 ----------
        // Square 2000×2000, long edge = 2000, cap = 100 → scale = 100/2000 = 0.05
        // → width = 2000 * 0.05 = 100, height = 2000 * 0.05 = 100.
        let result = decode_image_rgba(temp.path(), Some(100));
        assert!(
            result.is_ok(),
            "expected Ok for max_edge=Some(100) on large image, got Err: {:?}",
            result
        );
        let decoded = result.unwrap();
        assert_eq!(
            decoded.width, 100,
            "width should be scaled to 100 (2000 × 100÷2000)"
        );
        assert_eq!(
            decoded.height, 100,
            "height should be scaled to 100 (2000 × 100÷2000)"
        );

        // ---------- max_edge = None → full resolution ----------
        let result = decode_image_rgba(temp.path(), None);
        assert!(
            result.is_ok(),
            "expected Ok for max_edge=None on large image, got Err: {:?}",
            result
        );
        let decoded = result.unwrap();
        assert_eq!(
            decoded.width, 2000,
            "width should be 2000 (full resolution)"
        );
        assert_eq!(
            decoded.height, 2000,
            "height should be 2000 (full resolution)"
        );

        // ---------- max_edge = Some(5000) → no upscale ----------
        // Source 2000×2000 is already smaller than 5000 on the long edge.
        let result = decode_image_rgba(temp.path(), Some(5000));
        assert!(
            result.is_ok(),
            "expected Ok for max_edge=Some(5000) on large image, got Err: {:?}",
            result
        );
        let decoded = result.unwrap();
        assert_eq!(
            decoded.width, 2000,
            "width should be 2000 (no upscale)"
        );
        assert_eq!(
            decoded.height, 2000,
            "height should be 2000 (no upscale)"
        );
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

    // =======================================================================
    // Concurrency cap: schedule_parallel is limited to MAX_IN_FLIGHT in-flight
    // work items at any time
    // =======================================================================

    const EXPECTED_MAX_IN_FLIGHT: usize = 3;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn schedule_parallel_caps_concurrent_work_and_completes_all_items() {
        // N = 10 items — well above the cap of 3, so the cap is exercised.
        let n = 10usize;
        let paths: Vec<PathBuf> = (0..n)
            .map(|i| PathBuf::from(format!("/tmp/test-capped-{i}.jpg")))
            .collect();

        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let deliver_count = Arc::new(AtomicUsize::new(0));
        let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

        let work = {
            let infl = Arc::clone(&in_flight);
            let max_infl = Arc::clone(&max_in_flight);
            move |_path: &Path| -> Result<DecodedRgba, ImageLoadError> {
                let prev = infl.fetch_add(1, Ordering::SeqCst);
                let cur = prev + 1;
                max_infl.fetch_max(cur, Ordering::SeqCst);
                // Sleep long enough that the cap window is observable.
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
            let dc = Arc::clone(&deliver_count);
            let done_tx = done_tx.clone();
            move |_result: Result<DecodedRgba, ImageLoadError>| {
                dc.fetch_add(1, Ordering::SeqCst);
                let _ = done_tx.send(());
            }
        };

        schedule_parallel(paths, work, deliver);

        // Wait for all N deliveries with a generous timeout.
        for _ in 0..n {
            tokio::time::timeout(std::time::Duration::from_secs(5), done_rx.recv())
                .await
                .expect("timeout waiting for delivery")
                .expect("channel closed before all deliveries");
        }

        let max = max_in_flight.load(Ordering::SeqCst);
        assert!(
            max <= EXPECTED_MAX_IN_FLIGHT,
            "expected at most {} concurrent work items, got {max}",
            EXPECTED_MAX_IN_FLIGHT,
        );

        let delivered = deliver_count.load(Ordering::SeqCst);
        assert_eq!(
            delivered, n,
            "all {n} items should be delivered, got {delivered}"
        );
    }

    // =======================================================================
    // Unit 2 – custom chat avatar photo: image operations
    // (crop_rgba, apply_circle_mask, save_png)
    // =======================================================================

    /// Build a 4×4 source buffer where each pixel's R encodes its column
    /// and G encodes its row (R=0..3, G=0..3), B=0, A=255.
    fn make_4x4_grid() -> DecodedRgba {
        let mut pixels = Vec::with_capacity(4 * 4 * 4);
        for row in 0..4 {
            for col in 0..4 {
                pixels.push(col as u8); // R = column
                pixels.push(row as u8); // G = row
                pixels.push(0);         // B = 0
                pixels.push(255);       // A = 255
            }
        }
        DecodedRgba {
            width: 4,
            height: 4,
            pixels,
        }
    }

    #[test]
    fn crop_rgba_extracts_subregion() {
        let src = make_4x4_grid();
        let result = super::crop_rgba(&src, 1, 1, 2, 2).expect("crop should succeed");
        assert_eq!(result.width, 2);
        assert_eq!(result.height, 2);
        assert_eq!(result.pixels.len(), 16);

        // Pixel (col=1, row=1) → R=1, G=1
        assert_eq!(result.pixels[0], 1, "pixel (0,0) in crop (source pos 1,1) R should be 1");
        assert_eq!(result.pixels[1], 1, "pixel (0,0) in crop (source pos 1,1) G should be 1");
        // Pixel (col=2, row=1) → R=2, G=1
        assert_eq!(result.pixels[4], 2, "pixel (1,0) in crop (source pos 2,1) R should be 2");
        assert_eq!(result.pixels[5], 1, "pixel (1,0) in crop (source pos 2,1) G should be 1");
        // Pixel (col=1, row=2) → R=1, G=2
        assert_eq!(result.pixels[8], 1, "pixel (0,1) in crop (source pos 1,2) R should be 1");
        assert_eq!(result.pixels[9], 2, "pixel (0,1) in crop (source pos 1,2) G should be 2");
        // Pixel (col=2, row=2) → R=2, G=2
        assert_eq!(result.pixels[12], 2, "pixel (1,1) in crop (source pos 2,2) R should be 2");
        assert_eq!(result.pixels[13], 2, "pixel (1,1) in crop (source pos 2,2) G should be 2");

        // Source buffer is unchanged
        assert_eq!(src.width, 4, "source width unchanged");
        assert_eq!(src.height, 4, "source height unchanged");
        assert_eq!(src.pixels.len(), 64, "source pixel count unchanged");
        // Quick spot-check on source
        assert_eq!(src.pixels[0], 0, "source pixel (0,0) R unchanged");
        assert_eq!(src.pixels[1], 0, "source pixel (0,0) G unchanged");
        assert_eq!(src.pixels[3], 255, "source pixel (0,0) A unchanged");
    }

    #[test]
    fn crop_rgba_at_origin() {
        let src = make_4x4_grid();
        let result = super::crop_rgba(&src, 0, 0, 2, 2).expect("crop at origin should succeed");
        assert_eq!(result.width, 2);
        assert_eq!(result.height, 2);
        assert_eq!(result.pixels.len(), 16);

        // Top-left quadrant: (0,0), (1,0), (0,1), (1,1)
        assert_eq!(result.pixels[0], 0, "pixel (0,0) R");
        assert_eq!(result.pixels[1], 0, "pixel (0,0) G");
        assert_eq!(result.pixels[4], 1, "pixel (1,0) R");
        assert_eq!(result.pixels[5], 0, "pixel (1,0) G");
        assert_eq!(result.pixels[8], 0, "pixel (0,1) R");
        assert_eq!(result.pixels[9], 1, "pixel (0,1) G");
        assert_eq!(result.pixels[12], 1, "pixel (1,1) R");
        assert_eq!(result.pixels[13], 1, "pixel (1,1) G");
    }

    #[test]
    fn crop_rgba_at_edge() {
        let src = make_4x4_grid();
        let result = super::crop_rgba(&src, 2, 2, 2, 2).expect("crop at edge should succeed");
        assert_eq!(result.width, 2);
        assert_eq!(result.height, 2);
        assert_eq!(result.pixels.len(), 16);

        // Bottom-right quadrant: (2,2), (3,2), (2,3), (3,3)
        assert_eq!(result.pixels[0], 2, "pixel (0,0) in crop (source pos 2,2) R");
        assert_eq!(result.pixels[1], 2, "pixel (0,0) in crop (source pos 2,2) G");
        assert_eq!(result.pixels[4], 3, "pixel (1,0) in crop (source pos 3,2) R");
        assert_eq!(result.pixels[5], 2, "pixel (1,0) in crop (source pos 3,2) G");
        assert_eq!(result.pixels[8], 2, "pixel (0,1) in crop (source pos 2,3) R");
        assert_eq!(result.pixels[9], 3, "pixel (0,1) in crop (source pos 2,3) G");
        assert_eq!(result.pixels[12], 3, "pixel (1,1) in crop (source pos 3,3) R");
        assert_eq!(result.pixels[13], 3, "pixel (1,1) in crop (source pos 3,3) G");
    }

    #[test]
    fn crop_rgba_out_of_bounds_errors() {
        let src = make_4x4_grid();
        // Crop starting at (3,3) with size 2×2 would need pixels at columns
        // 3-4 and rows 3-4, but source is only 4×4 (valid columns 0-3, rows 0-3).
        let result = super::crop_rgba(&src, 3, 3, 2, 2);
        assert!(
            result.is_err(),
            "expected Err for out-of-bounds crop, got Ok: {:?}",
            result
        );
    }

    /// Build a 4×4 fully-opaque white buffer (R=G=B=A=255).
    fn make_4x4_opaque_white() -> DecodedRgba {
        DecodedRgba {
            width: 4,
            height: 4,
            pixels: vec![255u8; 4 * 4 * 4],
        }
    }

    #[test]
    fn apply_circle_mask_zeroes_corner_alpha() {
        let mut buf = make_4x4_opaque_white();
        super::apply_circle_mask(&mut buf);

        // For a 4×4 square, center = (1.5, 1.5), radius = 2.0.
        // Pixel (1,1) is at distance ≈ 0.71 < 2 → inside circle → alpha unchanged (255).
        let center_idx = (1 * 4 + 1) * 4 + 3;
        assert_eq!(
            buf.pixels[center_idx], 255,
            "center pixel (1,1) at distance ≈0.71 < radius 2 should keep alpha=255"
        );

        // Pixel (0,0) is at distance ≈ 2.12 > 2 → outside circle → alpha set to 0.
        let corner_idx = (0 * 4 + 0) * 4 + 3;
        assert_eq!(
            buf.pixels[corner_idx], 0,
            "corner pixel (0,0) at distance ≈2.12 > radius 2 should have alpha=0"
        );

        // All RGB values must be unchanged (still 255) everywhere.
        for y in 0..4 {
            for x in 0..4 {
                let idx = (y * 4 + x) * 4;
                assert_eq!(buf.pixels[idx], 255, "R at ({x},{y}) unchanged");
                assert_eq!(buf.pixels[idx + 1], 255, "G at ({x},{y}) unchanged");
                assert_eq!(buf.pixels[idx + 2], 255, "B at ({x},{y}) unchanged");
            }
        }
    }

    #[test]
    fn apply_circle_mask_preserves_circle_pixel_alpha() {
        // For a 4×4 buffer: center=(1.5, 1.5), radius=min(4,4)/2 = 2.0.
        //
        //   (1,1):  distance = sqrt((1-1.5)² + (1-1.5)²) = sqrt(0.5) ≈ 0.71  < 2 → inside
        //   (0,0):  distance = sqrt((0-1.5)² + (0-1.5)²) = sqrt(4.5) ≈ 2.12 > 2 → outside
        let mut buf = make_4x4_opaque_white();
        super::apply_circle_mask(&mut buf);

        let inside_idx = (1 * 4 + 1) * 4 + 3;
        assert_eq!(
            buf.pixels[inside_idx], 255,
            "pixel (1,1) at distance ≈0.71 < radius 2 should keep alpha=255"
        );

        let outside_idx = (0 * 4 + 0) * 4 + 3;
        assert_eq!(
            buf.pixels[outside_idx], 0,
            "pixel (0,0) at distance ≈2.12 > radius 2 should have alpha=0"
        );
    }

    #[test]
    fn save_png_round_trip_preserves_pixels() {
        // 8×8 buffer with a pattern: R = column, G = row, B = 0, A = 255.
        let mut pixels = Vec::with_capacity(8 * 8 * 4);
        for row in 0..8 {
            for col in 0..8 {
                pixels.push(col as u8); // R
                pixels.push(row as u8); // G
                pixels.push(0);         // B
                pixels.push(255);       // A
            }
        }
        let src = DecodedRgba {
            width: 8,
            height: 8,
            pixels,
        };

        let temp = tempfile::Builder::new()
            .suffix(".png")
            .tempfile()
            .expect("create temp .png file");

        super::save_png(&src, temp.path()).expect("save_png should succeed");

        let decoded = super::decode_image_rgba(temp.path(), None)
            .expect("decode_image_rgba should load the saved PNG");

        assert_eq!(decoded.width, 8, "width should round-trip");
        assert_eq!(decoded.height, 8, "height should round-trip");
        assert_eq!(decoded.pixels, src.pixels, "pixel data should round-trip exactly");
    }

    #[test]
    fn save_png_round_trip_preserves_alpha() {
        // 4×4 buffer: checkerboard of opaque (A=255) and transparent (A=0) pixels.
        let mut pixels = Vec::with_capacity(4 * 4 * 4);
        for row in 0..4 {
            for col in 0..4 {
                pixels.push(128);                               // R
                pixels.push(64);                                // G
                pixels.push(32);                                // B
                pixels.push(if (row + col) % 2 == 0 { 255 } else { 0 }); // A
            }
        }
        let src = DecodedRgba {
            width: 4,
            height: 4,
            pixels,
        };

        let temp = tempfile::Builder::new()
            .suffix(".png")
            .tempfile()
            .expect("create temp .png file");

        super::save_png(&src, temp.path()).expect("save_png should succeed");

        let decoded = super::decode_image_rgba(temp.path(), None)
            .expect("decode_image_rgba should load the saved PNG");

        assert_eq!(decoded.width, 4, "width should round-trip");
        assert_eq!(decoded.height, 4, "height should round-trip");
        assert_eq!(decoded.pixels, src.pixels, "alpha values should round-trip exactly");
    }

    // =======================================================================
    // Unit 4b-math – custom chat avatar photo: render_avatar
    // (combines crop_rgba + apply_circle_mask + scale-to-256)
    // =======================================================================

    /// Build a 100×100 source buffer where each pixel's R = column mod 256,
    /// G = row mod 256, B = 0, A = 255.
    fn make_100x100_grid() -> DecodedRgba {
        let mut pixels = Vec::with_capacity(100 * 100 * 4);
        for row in 0..100 {
            for col in 0..100 {
                pixels.push((col % 256) as u8); // R = column
                pixels.push((row % 256) as u8); // G = row
                pixels.push(0);                 // B = 0
                pixels.push(255);               // A = 255
            }
        }
        DecodedRgba {
            width: 100,
            height: 100,
            pixels,
        }
    }

    /// Build a 100×100 fully-opaque white buffer (R=G=B=A=255).
    fn make_100x100_opaque_white() -> DecodedRgba {
        DecodedRgba {
            width: 100,
            height: 100,
            pixels: vec![255u8; 100 * 100 * 4],
        }
    }

    /// Build a 64×64 source buffer with the same column/row pattern.
    fn make_64x64_grid() -> DecodedRgba {
        let mut pixels = Vec::with_capacity(64 * 64 * 4);
        for row in 0..64 {
            for col in 0..64 {
                pixels.push((col % 256) as u8); // R = column
                pixels.push((row % 256) as u8); // G = row
                pixels.push(0);                 // B = 0
                pixels.push(255);               // A = 255
            }
        }
        DecodedRgba {
            width: 64,
            height: 64,
            pixels,
        }
    }

    #[test]
    fn render_avatar_centered_full_size() {
        let src = make_100x100_grid();
        let result = super::render_avatar(
            &src,
            &super::CropParams {
                cx: 50.0,
                cy: 50.0,
                r: 50.0,
            },
        )
        .expect("centered full-size crop should succeed");

        assert_eq!(result.width, 256, "output width should be 256");
        assert_eq!(result.height, 256, "output height should be 256");
        assert_eq!(
            result.pixels.len(),
            256 * 256 * 4,
            "pixel buffer should be 256×256×4 bytes"
        );
    }

    #[test]
    fn render_avatar_offset_crop() {
        let src = make_100x100_grid();
        let result = super::render_avatar(
            &src,
            &super::CropParams {
                cx: 25.0,
                cy: 25.0,
                r: 25.0,
            },
        )
        .expect("offset crop should succeed");

        assert_eq!(result.width, 256, "output width should be 256");
        assert_eq!(result.height, 256, "output height should be 256");

        // The crop is non-empty — at least one corner pixel is non-zero
        // (the bilinear scaling may mix values, but the output isn't all
        // transparent/zero).
        let non_zero = result.pixels.iter().any(|&b| b != 0);
        assert!(non_zero, "output should contain non-zero pixel data");
    }

    #[test]
    fn render_avatar_clamps_to_source_bounds() {
        let src = make_100x100_grid();
        let result = super::render_avatar(
            &src,
            &super::CropParams {
                cx: 0.0,
                cy: 0.0,
                r: 50.0,
            },
        )
        .expect("crop clamped to source should succeed");

        assert_eq!(result.width, 256, "output width should be 256");
        assert_eq!(result.height, 256, "output height should be 256");
    }

    #[test]
    fn render_avatar_circular_alpha_in_output() {
        let src = make_100x100_opaque_white();
        let result = super::render_avatar(
            &src,
            &super::CropParams {
                cx: 50.0,
                cy: 50.0,
                r: 50.0,
            },
        )
        .expect("centered crop should succeed");

        assert_eq!(result.width, 256, "output width should be 256");
        assert_eq!(result.height, 256, "output height should be 256");

        // After scaling the centered 100×100 crop to 256×256, the circle
        // centre is at (128, 128).  The pixel at (128, 128) should be inside
        // the circle → alpha ≈ 255.
        let center_idx = (128 * 256 + 128) * 4 + 3;
        assert_eq!(
            result.pixels[center_idx], 255,
            "centre pixel (128,128) should be inside circle with alpha=255"
        );

        // Corner pixel (0,0) is far outside the circle → alpha = 0.
        let corner_idx = (0 * 256 + 0) * 4 + 3;
        assert_eq!(
            result.pixels[corner_idx], 0,
            "corner pixel (0,0) should be outside circle with alpha=0"
        );
    }

    #[test]
    fn render_avatar_zero_area_errors() {
        let src = make_100x100_grid();
        let result = super::render_avatar(
            &src,
            &super::CropParams {
                cx: 50.0,
                cy: 50.0,
                r: 0.0,
            },
        );

        assert!(
            result.is_err(),
            "expected Err for zero-radius crop, got Ok: {:?}",
            result
        );
    }

    #[test]
    fn render_avatar_fully_outside_source_errors() {
        let src = make_100x100_grid();
        let result = super::render_avatar(
            &src,
            &super::CropParams {
                cx: 1000.0,
                cy: 1000.0,
                r: 50.0,
            },
        );

        assert!(
            result.is_err(),
            "expected Err for fully-outside-source crop, got Ok: {:?}",
            result
        );
    }

    #[test]
    fn render_avatar_round_trip_via_save_png() {
        let src = make_64x64_grid();
        let rendered = super::render_avatar(
            &src,
            &super::CropParams {
                cx: 32.0,
                cy: 32.0,
                r: 32.0,
            },
        )
        .expect("centered crop for round-trip should succeed");

        let temp = tempfile::Builder::new()
            .suffix(".png")
            .tempfile()
            .expect("create temp .png file");

        super::save_png(&rendered, temp.path()).expect("save_png should succeed");

        let decoded =
            super::decode_image_rgba(temp.path(), None).expect("decode_image_rgba should load PNG");

        assert_eq!(decoded.width, 256, "round-tripped width should be 256");
        assert_eq!(decoded.height, 256, "round-tripped height should be 256");
    }
}
