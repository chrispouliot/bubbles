mod gtk_bridge;
mod protocol;
mod runtime;
mod setup;
mod store;
mod ui;

mod demo;

#[cfg(feature = "rustpush")]
mod api;

// The Mac-hardware protobuf, compiled by build.rs into OUT_DIR. Referenced as
// `crate::bbhwinfo` by the vendored api subset.
#[cfg(feature = "rustpush")]
pub mod bbhwinfo {
    include!(concat!(env!("OUT_DIR"), "/bbhwinfo.rs"));
}

use std::sync::Arc;

use adw::prelude::*;

use protocol::Backend;

const APP_ID: &str = "app.openbubbles.Gtk.Devel";

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .build();

    app.connect_activate(|app| {
        let window = if std::env::var_os("OPENBUBBLES_DEMO").is_some() {
            demo::build_demo_window(app)
        } else {
            setup::view::build_window(app, make_backend(), make_store())
        };
        window.present();
    });

    app.run()
}

/// Open (creating if needed) the message store under the app data dir.
fn make_store() -> store::Store {
    let dir = glib::user_data_dir().join("openbubbles-gtk");
    std::fs::create_dir_all(&dir).ok();
    let path = dir.join("messages.db");
    runtime::runtime()
        .block_on(store::Store::open(path))
        .expect("failed to open message store")
}

/// Real backend: initialises the rustpush state dir + logger, then hands the
/// onboarding flow a live `RustpushBackend`.
#[cfg(feature = "rustpush")]
fn make_backend() -> Arc<dyn Backend> {
    let dir = glib::user_data_dir().join("openbubbles-gtk");
    std::fs::create_dir_all(&dir).ok();
    let path = dir.to_string_lossy().into_owned();
    api::do_first_time_init(path.clone());
    Arc::new(protocol::rustpush_backend::RustpushBackend::new(path))
}

/// Default backend: canned values so the flow is click-through-able without
/// rustpush linked.
#[cfg(not(feature = "rustpush"))]
fn make_backend() -> Arc<dyn Backend> {
    Arc::new(protocol::stub::StubBackend)
}
