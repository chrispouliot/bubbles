//! Link-preview card helpers extracted from [`super`](mod.rs).
//! Renders iMessage rich link cards: thumbnails, host captions, placeholders,
//! and the full card widget.

use super::*;
use super::builder::apply_text_scale;

// --- link preview card ---

/// Best-effort extraction of a host label from a URL for the small "example.com"
/// caption at the bottom of the card. We try to render something readable even
/// when the URL is malformed or uses a non-default scheme.
pub(super) fn host_caption(url: &str) -> String {
    // Strip the scheme.
    let after_scheme = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url);
    // Drop the path, query, and fragment; keep the host (and optional :port).
    let host_port = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    if host_port.is_empty() {
        url.to_string()
    } else {
        host_port.to_string()
    }
}

/// The sender's preview is sparse (`is_placeholder == true` or both title and
/// summary are empty). Render a compact "loading preview…" state instead of an
/// empty card — it's what the user actually sees while waiting for the fill-in
/// or for a sender that ships only a thumbnail + URL.
fn link_preview_placeholder_card(p: &MessageLinkPreview) -> gtk::Widget {
    let card = gtk::Button::builder()
        .has_frame(false)
        .halign(gtk::Align::Start)
        .build();
    card.set_focus_on_click(false);
    card.add_css_class("link-preview");
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .build();
    row.append(&link_preview_thumb(p, 224, 280));
    let text_col = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .valign(gtk::Align::Center)
        .hexpand(true)
        .build();
    let label = gtk::Label::builder()
        .label("Loading preview…")
        .xalign(0.0)
        .max_width_chars(28)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .build();
    label.add_css_class("link-preview-placeholder");
    text_col.append(&label);
    if let Some(u) = p.url.as_deref().or(p.original_url.as_deref()) {
        let host = gtk::Label::builder()
            .label(host_caption(u))
            .xalign(0.0)
            .max_width_chars(28)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .build();
        host.add_css_class("link-preview-host");
        text_col.append(&host);
    }
    row.append(&text_col);
    let content = adw::Clamp::builder()
        .maximum_size(224)
        .tightening_threshold(224)
        .unit(adw::LengthUnit::Px)
        .child(&row)
        .build();
    card.set_child(Some(&content));
    // Clicking the placeholder opens the URL too (best UX while we wait).
    if let Some(u) = p.url.as_deref().or(p.original_url.as_deref()) {
        let url = u.to_string();
        card.connect_clicked(move |_| open_uri(&url));
        card.set_cursor_from_name(Some("pointer"));
    }
    card.upcast()
}

/// A rounded thumbnail, loaded from `image_path` on disk. The
/// thumbnail bytes were just written there by the link-preview ingest, so
/// the synchronous read is fast and fresh. If the cached image can't be
/// decoded (HEIC on a system without gdk-pixbuf HEIC, or the file was
/// deleted), the cell is filled with a neutral chain-link icon.
fn link_preview_thumb(p: &MessageLinkPreview, width: i32, height: i32) -> gtk::Widget {
    if let Some(path) = p.image_path.as_deref() {
        if let Ok(texture) = gtk::gdk::Texture::from_filename(path) {
            let pic = gtk::Picture::new();
            pic.set_paintable(Some(&texture));
            // The overlay viewport owns the size requisition. Excluding the
            // picture from overlay measurement keeps the texture's natural
            // dimensions from widening the card.
            pic.set_content_fit(gtk::ContentFit::Cover);
            pic.set_can_shrink(true);
            pic.set_hexpand(true);
            pic.set_vexpand(true);
            pic.set_halign(gtk::Align::Fill);
            pic.set_valign(gtk::Align::Fill);

            let viewport = gtk::Overlay::new();
            viewport.set_overflow(gtk::Overflow::Hidden);
            viewport.add_css_class("link-preview-thumb");
            let size = gtk::Box::new(gtk::Orientation::Vertical, 0);
            size.set_size_request(width, height);
            viewport.set_child(Some(&size));
            viewport.add_overlay(&pic);
            viewport.set_measure_overlay(&pic, false);
            return viewport.upcast();
        }
    }
    // Fallback: neutral chain icon in a rounded box the same size as the thumb.
    let box_ = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();
    box_.set_size_request(width, height);
    box_.add_css_class("link-preview-thumb-fallback");
    let icon = gtk::Image::from_icon_name("insert-link-symbolic");
    icon.set_pixel_size(32);
    box_.append(&icon);
    box_.upcast()
}

/// A link-preview card for an inbound `MessageLinkPreview` — the sender's static
/// snapshot, already downloaded. Clicking opens the URL via the system browser.
pub(super) fn link_preview_card(p: &MessageLinkPreview) -> gtk::Widget {
    // Sparse (placeholder, or title+summary both empty): render the compact
    // loading state, not an empty card shell.
    if p.is_sparse() {
        return link_preview_placeholder_card(p);
    }
    let card = gtk::Button::builder()
        .has_frame(false)
        .halign(gtk::Align::Start)
        .build();
    card.set_focus_on_click(false);
    card.add_css_class("link-preview");

    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .build();
    row.append(&link_preview_thumb(p, 224, 280));

    let text_col = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .valign(gtk::Align::Center)
        .hexpand(true)
        .build();

    let title_text = p.title.clone().unwrap_or_default();
    if !title_text.is_empty() {
        let title = gtk::Label::builder()
            .label(&title_text)
            .xalign(0.0)
            .max_width_chars(28)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .build();
        title.add_css_class("link-preview-title");
        apply_text_scale(&title, 13.0);
        text_col.append(&title);
    }
    let summary_text = p.summary.clone().unwrap_or_default();
    if !summary_text.is_empty() {
        let summary = gtk::Label::builder()
            .label(&summary_text)
            .xalign(0.0)
            .max_width_chars(32)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .build();
        summary.add_css_class("link-preview-desc");
        apply_text_scale(&summary, 11.0);
        text_col.append(&summary);
    }
    if let Some(u) = p.url.as_deref().or(p.original_url.as_deref()) {
        let host = gtk::Label::builder()
            .label(host_caption(u))
            .xalign(0.0)
            .max_width_chars(28)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .build();
        host.add_css_class("link-preview-host");
        apply_text_scale(&host, 10.0);
        text_col.append(&host);
    }
    row.append(&text_col);
    let content = adw::Clamp::builder()
        .maximum_size(224)
        .tightening_threshold(224)
        .unit(adw::LengthUnit::Px)
        .child(&row)
        .build();
    card.set_child(Some(&content));

    // Open the URL when clicked. Use the original URL (what the sender typed)
    // when it differs from the canonical one — that's the link the sender
    // intended the user to follow.
    if let Some(u) = p.original_url.as_deref().or(p.url.as_deref()) {
        let url = u.to_string();
        card.connect_clicked(move |_| open_uri(&url));
        card.set_cursor_from_name(Some("pointer"));
    }

    card.upcast()
}
