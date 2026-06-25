use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use gtk::{cairo, gdk, gdk_pixbuf};
use gtk::gdk::prelude::GdkCairoContextExt;
use gtk::prelude::*;

pub fn compute_initials(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let upper = trimmed.to_uppercase();
    let first = upper.chars().next().unwrap();

    if let Some(pos) = upper.rfind(' ') {
        let after_space = &upper[pos + 1..];
        if let Some(second) = after_space.chars().next() {
            if second == first {
                return first.to_string();
            }
            return format!("{}{}", first, second);
        }
    }

    first.to_string()
}

pub fn avatar_color_index(text: &str) -> usize {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    (hasher.finish() as usize) % 14
}

/// Returns the font pixel size for drawing initials in an avatar of the given size.
/// Single-character initials get ~45% of `size`; multi-character initials get ~38%,
/// so two-letter initials render smaller and fit the circle comfortably.
pub fn initials_font_size(initials: &str, size: i32) -> f64 {
    let s = size as f64;
    if initials.len() > 1 {
        s * 0.38
    } else {
        s * 0.45
    }
}

/// 14 avatar background colors, taken from libadwaita's `_avatar.scss` gradient bottom colors.
const AVATAR_PALETTE: [(u8, u8, u8); 14] = [
    (0x33, 0x7f, 0xdc), // blue
    (0x0f, 0x9a, 0xc8), // cyan
    (0x29, 0xae, 0x74), // green
    (0x6a, 0xb8, 0x5b), // lime
    (0xd2, 0x9d, 0x09), // yellow
    (0xd6, 0x84, 0x00), // gold
    (0xed, 0x5b, 0x00), // orange
    (0xe6, 0x2d, 0x42), // raspberry
    (0xe3, 0x3b, 0x6a), // magenta
    (0x99, 0x45, 0xb5), // purple
    (0x7a, 0x59, 0xca), // violet
    (0xb0, 0x89, 0x52), // beige
    (0x78, 0x53, 0x36), // brown
    (0x6e, 0x6d, 0x71), // gray
];

struct AvatarState {
    initials: String,
    color_index: usize,
    custom_image: Option<gdk::Texture>,
}

/// A custom avatar widget that renders initials truly centered using Cairo text extents
/// (visual glyph bounds) instead of Pango's logical ascent+descent.
///
/// Wraps a [`gtk::DrawingArea`] and supports a custom image via [`set_custom_image`](Self::set_custom_image).
pub struct AvatarWidget {
    drawing_area: gtk::DrawingArea,
    state: Rc<RefCell<AvatarState>>,
}

impl AvatarWidget {
    /// Create a new avatar with the given `size` in pixels and display text.
    pub fn new(size: i32, text: &str) -> Self {
        let initials = compute_initials(text);
        let color_index = avatar_color_index(text);

        let state = Rc::new(RefCell::new(AvatarState {
            initials,
            color_index,
            custom_image: None,
        }));

        let drawing_area = gtk::DrawingArea::new();
        drawing_area.set_size_request(size, size);

        let state_clone = state.clone();
        drawing_area.set_draw_func(move |_area, cr, _width, _height| {
            Self::draw_avatar(cr, &state_clone.borrow(), size);
        });

        AvatarWidget { drawing_area, state }
    }

    /// Set or clear the custom image texture.
    pub fn set_custom_image(&self, texture: Option<&gdk::Texture>) {
        self.state.borrow_mut().custom_image = texture.cloned();
        self.drawing_area.queue_draw();
    }

    /// Return a reference to the inner widget for adding to a container.
    pub fn widget(&self) -> &gtk::Widget {
        self.drawing_area.upcast_ref::<gtk::Widget>()
    }

    fn draw_avatar(cr: &cairo::Context, state: &AvatarState, size: i32) {
        let s = size as f64;

        // Clip to circle
        let _ = cr.save();
        cr.arc(s / 2.0, s / 2.0, s / 2.0, 0.0, 2.0 * std::f64::consts::PI);
        cr.clip();

        if let Some(texture) = &state.custom_image {
            Self::draw_custom_image(cr, texture, size);
        } else {
            // Fill the circle with the avatar color
            let (r, g, b) = AVATAR_PALETTE[state.color_index];
            cr.set_source_rgba(r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0, 1.0);
            let _ = cr.paint();

            // Draw initials centered using Cairo text extents (visual glyph bounds)
            if !state.initials.is_empty() {
                Self::draw_initials_centered(cr, &state.initials, size);
            }
        }

        let _ = cr.restore();
    }

    fn draw_initials_centered(cr: &cairo::Context, initials: &str, size: i32) {
        let s = size as f64;
        let font_size = initials_font_size(initials, size);

        cr.select_font_face("sans-serif", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
        cr.set_font_size(font_size);

        // Cairo's text_extents returns the visual glyph bounds (ink extents equivalent).
        let extents = match cr.text_extents(initials) {
            Ok(e) => e,
            Err(_) => return,
        };

        // Center the ink rect within the widget:
        //   x = (size - ink_width) / 2 - ink_rect.x_bearing
        //   y = (size - ink_height) / 2 - ink_rect.y_bearing
        let x = (s - extents.width()) / 2.0 - extents.x_bearing();
        let y = (s - extents.height()) / 2.0 - extents.y_bearing();

        cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
        cr.move_to(x, y);
        let _ = cr.show_text(initials);
    }

    fn draw_custom_image(cr: &cairo::Context, texture: &gdk::Texture, size: i32) {
        let s = size as f64;
        let (tw, th) = (texture.width() as f64, texture.height() as f64);
        if tw <= 0.0 || th <= 0.0 {
            return;
        }

        // Scale to fit within the circle (longer dimension = size)
        let scale = s / tw.max(th);
        let (sw, sh) = ((tw * scale) as i32, (th * scale) as i32);
        let ox = (s - sw as f64) / 2.0;
        let oy = (s - sh as f64) / 2.0;

        // Get a pixbuf from the texture and draw it centered
        if let Some(pixbuf) = texture_to_pixbuf(texture) {
            if let Some(scaled) = pixbuf.scale_simple(sw, sh, gdk_pixbuf::InterpType::Bilinear) {
                cr.set_source_pixbuf(&scaled, ox, oy);
                let _ = cr.paint();
            }
        }
    }
}

/// Convert a `gdk::Texture` to a `gdk_pixbuf::Pixbuf` via a PNG round-trip.
fn texture_to_pixbuf(texture: &gdk::Texture) -> Option<gdk_pixbuf::Pixbuf> {
    let png_bytes = texture.save_to_png_bytes();
    let loader = gdk_pixbuf::PixbufLoader::new();
    loader.write(&png_bytes).ok()?;
    loader.close().ok()?;
    loader.pixbuf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_initials_single_word() {
        assert_eq!(compute_initials("Alice"), "A");
    }

    #[test]
    fn test_initials_two_words() {
        assert_eq!(compute_initials("Alice Bob"), "AB");
    }

    #[test]
    fn test_initials_lowercase() {
        assert_eq!(compute_initials("alice bob"), "AB");
    }

    #[test]
    fn test_initials_whitespace_stripped() {
        assert_eq!(compute_initials("  Alice  "), "A");
    }

    #[test]
    fn test_initials_empty() {
        assert_eq!(compute_initials(""), "");
    }

    #[test]
    fn test_initials_hyphen_no_space() {
        assert_eq!(compute_initials("Jean-Claude"), "J");
    }

    #[test]
    fn test_initials_multiple_spaces() {
        assert_eq!(compute_initials("Alice   Bob"), "AB");
    }

    #[test]
    fn test_initials_three_names() {
        assert_eq!(compute_initials("Alice Bob Carol"), "AC");
    }

    #[test]
    fn test_color_index_range() {
        let idx = avatar_color_index("Alice");
        assert!(idx < 14, "index {} should be < 14", idx);
    }

    #[test]
    fn test_color_index_deterministic() {
        let a = avatar_color_index("Alice");
        let b = avatar_color_index("Alice");
        assert_eq!(a, b);
    }

    #[test]
    fn test_color_index_other_input() {
        let idx = avatar_color_index("Bob");
        assert!(idx < 14, "index {} should be < 14", idx);
    }

    #[test]
    fn test_font_size_two_letters_smaller_than_one() {
        let one_letter = initials_font_size("A", 36);
        let two_letters = initials_font_size("AB", 36);
        assert!(
            two_letters < one_letter,
            "two-letter initials ({two_letters}) should get a smaller font than one-letter ({one_letter})"
        );
    }

    #[test]
    fn test_font_size_scales_with_size() {
        let small = initials_font_size("AB", 28);
        let large = initials_font_size("AB", 56);
        assert!(
            large > small,
            "font size for size=56 ({large}) should be larger than for size=28 ({small})"
        );
    }

    #[test]
    fn test_font_size_empty_returns_value() {
        let result = initials_font_size("", 36);
        assert!(result > 0.0, "empty initials should return a positive value, got {result}");
    }
}
