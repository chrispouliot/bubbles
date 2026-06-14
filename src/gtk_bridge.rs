//! Bridge between the tokio runtime (where `rustpush` lives) and the GTK/glib
//! main loop (where widgets live).
//!
//! Get this one helper right and every backend call in [`crate::setup::flow`]
//! becomes: clone the handles you need out of the shared state, [`spawn`] the
//! future, and write the result back in the `on_done` closure — which runs on
//! the GTK main thread, so it can touch widgets freely.

use std::future::Future;

use crate::runtime::runtime;

/// Run `fut` on the tokio runtime; call `on_done` with its output back on the
/// GTK main thread once it resolves.
///
/// `fut` must be `Send` (it crosses to a tokio worker). `on_done` runs on the
/// main thread and may be `!Send`.
pub fn spawn<F, T>(fut: F, on_done: impl FnOnce(T) + 'static)
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = async_channel::bounded::<T>(1);

    runtime().spawn(async move {
        let _ = tx.send(fut.await).await;
    });

    glib::spawn_future_local(async move {
        if let Ok(value) = rx.recv().await {
            on_done(value);
        }
    });
}
