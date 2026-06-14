//! Vendored from upstream `rust/src/lib.rs`: the tokio `RUNTIME` and
//! `init_logger` that the api subset (`do_first_time_init`, internal spawns)
//! depends on. The Android branch is dropped — this is a desktop client.
//!
//! Note: the crate already has `crate::runtime` (the tokio runtime backing the
//! GTK bridge). This second runtime mirrors upstream so the vendored api code
//! is a faithful copy; you can later unify the two if you like.

use std::path::Path;
use std::sync::LazyLock;

use flexi_logger::{opt_format, Age, Cleanup, Criterion, FileSpec, Logger, Naming, WriteMode};
use log::info;

pub static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    info!("creating runner");
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("tokio-rustpush")
        .enable_all()
        .build()
        .unwrap()
});

pub fn init_logger(path: &Path) {
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "debug");
    }
    let system = pretty_env_logger::formatted_builder().build();

    let (logger, _) = Logger::try_with_str("debug")
        .expect("No logger?")
        .log_to_file(
            FileSpec::default()
                .directory(path.join("logs"))
                .suppress_timestamp(),
        )
        .append()
        .format(opt_format)
        .cleanup_in_background_thread(false)
        .rotate(
            Criterion::AgeOrSize(Age::Day, 1024 * 1024 * 10 /* 10 MB */),
            Naming::Numbers,
            Cleanup::KeepLogFiles(1),
        )
        .write_mode(WriteMode::BufferAndFlush)
        .build()
        .unwrap();

    multi_log::MultiLogger::init(vec![Box::new(system), logger], log::Level::Trace)
        .expect("No init?");
}
