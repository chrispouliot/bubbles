//! The tokio runtime that `rustpush` futures run on. Mirrors the multi-thread
//! `RUNTIME` in upstream `rust/src/lib.rs`. GTK runs on its own (glib) main
//! loop on the main thread; see [`crate::gtk_bridge`] for the hand-off.

use std::sync::OnceLock;

use tokio::runtime::Runtime;

pub fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("tokio-rustpush")
            .enable_all()
            .build()
            .expect("failed to build tokio runtime")
    })
}
