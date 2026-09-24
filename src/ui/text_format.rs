//! Text-formatting helpers: Pango-markup-safe URL linkification,
//! markup escaping, and system-browser URI opening.
//!
//! Extracted from [`super`](mod.rs). Pure text transformations — no GTK widget
//! construction, no global state beyond the cached URL regex.

use crate::store::MessageLinkPreview;
use gtk::gio::prelude::AppLaunchContextExt;
use regex::Regex;
use std::sync::OnceLock;

/// Regex that matches URLs at word boundaries.
static URL_RE: OnceLock<Regex> = OnceLock::new();

fn url_re() -> &'static Regex {
    URL_RE.get_or_init(|| {
        Regex::new(r#"(?i)\b(?:https?://|www\.)[^\s<>'"{}\[\]()]+[^\s<>'"{}\[\]()\.,:;!?)]"#).unwrap()
    })
}

/// Convert plain text containing URLs into Pango markup with clickable `<a>` tags.
/// Non-URL text is escaped for markup safety.
pub(super) fn text_to_markup(text: &str) -> String {
    let re = url_re();
    let mut result = String::with_capacity(text.len() + 64);
    let mut last_end = 0;
    for m in re.find_iter(text) {
        // Escape and append text before this URL
        if m.start() > last_end {
            result.push_str(&escape_markup(&text[last_end..m.start()]));
        }
        // Append the URL as a clickable link
        let url = m.as_str();
        result.push_str(&format!(
            r#"<a href="{}">{}</a>"#,
            escape_markup_attr(url),
            escape_markup(url)
        ));
        last_end = m.end();
    }
    // Append any remaining text
    if last_end < text.len() {
        result.push_str(&escape_markup(&text[last_end..]));
    }
    result
}

/// Remove only URL tokens represented by this message's rich-link preview.
/// All surrounding prose and any other linkified URLs remain untouched.
pub(super) fn text_without_preview_url(text: &str, preview: &MessageLinkPreview) -> String {
    let represented: Vec<String> = [preview.original_url.as_deref(), preview.url.as_deref()]
        .into_iter()
        .flatten()
        .map(normalize_preview_url)
        .collect();
    if represented.is_empty() {
        return text.to_string();
    }

    let mut result = String::with_capacity(text.len());
    let mut last_end = 0;
    for found in url_re().find_iter(text) {
        if represented
            .iter()
            .any(|url| *url == normalize_preview_url(found.as_str()))
        {
            result.push_str(&text[last_end..found.start()]);
            last_end = found.end();
        }
    }
    result.push_str(&text[last_end..]);
    result
}

fn normalize_preview_url(url: &str) -> String {
    let url = url.trim();
    let normalized = if url.get(..4).is_some_and(|s| s.eq_ignore_ascii_case("www.")) {
        format!("https://{url}")
    } else {
        url.to_string()
    };
    normalized
}

/// Escape a string for safe inclusion inside Pango markup.
fn escape_markup(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Escape a string for safe inclusion inside an XML attribute value.
fn escape_markup_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Open a URI in the system browser.
/// Uses GIO which routes through xdg-desktop-portal inside Flatpak,
/// and launches the default handler directly outside Flatpak.
pub(super) fn open_uri(uri: &str) {
    let uri = if uri.starts_with("www.") && !uri.starts_with("http") {
        format!("https://{uri}")
    } else {
        uri.to_string()
    };
    let context = gtk::gio::AppLaunchContext::new();
    context.unsetenv("LD_LIBRARY_PATH");
    if let Err(e) = gtk::gio::AppInfo::launch_default_for_uri(&uri, Some(&context)) {
        eprintln!("failed to open URI {}: {e}", uri);
    }
}
