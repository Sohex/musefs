//! `install_logger` wraps the caller's sink so scan progress and log records
//! stop clobbering each other (#648) — but the wrapping must be transparent:
//! every record the sink would have emitted still reaches it, and the caller's
//! level ceiling is still what `log` filters on.
//!
//! Only one test may live here: installing a global logger is a once-per-process
//! operation, and integration test binaries share a process.

use std::sync::Mutex;

use log::{Level, LevelFilter, Log, Metadata, Record};

static SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// A sink that filters the way `env_logger` does: `enabled` on level, `log`
/// re-checking it.
struct Sink;

impl Log for Sink {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= Level::Debug
    }

    fn log(&self, record: &Record<'_>) {
        if self.enabled(record.metadata()) {
            SEEN.lock()
                .unwrap()
                .push(format!("{} {}", record.level(), record.args()));
        }
    }

    fn flush(&self) {}
}

#[test]
fn installed_logger_forwards_records_and_honours_the_level_ceiling() {
    musefs_cli::install_logger(Box::new(Sink), LevelFilter::Debug);
    assert_eq!(
        log::max_level(),
        LevelFilter::Debug,
        "the caller's ceiling must reach `log::set_max_level` verbatim"
    );

    log::warn!("warned");
    log::debug!("debugged");
    log::trace!("traced"); // above the ceiling: never even reaches the sink

    assert_eq!(*SEEN.lock().unwrap(), ["WARN warned", "DEBUG debugged"]);
}
