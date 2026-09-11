//! Global `log` sink installation.
//!
//! The scan progress bar (see [`crate::progress`]) and the `log` facade both
//! write to stderr. Left uncoordinated, a record emitted while the bar is live
//! lands mid-frame and the next tick overwrites it — the common case rather
//! than an edge one, because the end-of-walk skip tally warns about the
//! `cover.jpg`/`.cue`/`.log` files essentially every real library contains
//! (#648).
//!
//! [`install_logger`] therefore wraps the caller's sink so every record is
//! emitted while the shared progress draw target is suspended: the bar is
//! cleared, the record is written, the bar is redrawn beneath it. The binary
//! keeps ownership of *what* the sink is and of the verbosity mapping; this
//! module only owns the interleaving.

use log::{LevelFilter, Log, Metadata, Record};

/// A `log::Log` that emits through the shared progress draw target, so records
/// never interleave with a bar redraw.
struct SuspendingLogger {
    inner: Box<dyn Log>,
}

impl Log for SuspendingLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.inner.enabled(metadata)
    }

    fn log(&self, record: &Record<'_>) {
        // Cheap pre-filter: suspending takes the draw lock, which is wasted work
        // for a record the sink will drop anyway. `enabled` is never stricter
        // than the sink's own check, so this cannot suppress a record that would
        // otherwise have printed.
        if !self.inner.enabled(record.metadata()) {
            return;
        }
        crate::progress::suspend(|| self.inner.log(record));
    }

    fn flush(&self) {
        self.inner.flush();
    }
}

/// Install `logger` as the global `log` sink, bridged to the scan progress bar.
///
/// `level` is the maximum level the sink can emit (for `env_logger`, that is
/// `Logger::filter()`); it is forwarded verbatim to [`log::set_max_level`], so
/// the caller's verbosity policy is preserved exactly.
///
/// # Panics
///
/// If a global logger is already installed, mirroring `env_logger::init`.
pub fn install_logger(logger: Box<dyn Log>, level: LevelFilter) {
    log::set_boxed_logger(Box::new(SuspendingLogger { inner: logger }))
        .expect("a global logger is already installed");
    log::set_max_level(level);
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use log::Level;

    use super::{Log, Metadata, Record, SuspendingLogger};

    #[derive(Default)]
    struct Seen {
        lines: Mutex<Vec<String>>,
        flushes: Mutex<usize>,
    }

    /// A sink that records what it was asked to emit, filtering the way
    /// `env_logger` does: `enabled` on level, `log` re-checking it.
    #[derive(Clone)]
    struct Recorder {
        max: Level,
        seen: Arc<Seen>,
    }

    impl Log for Recorder {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            metadata.level() <= self.max
        }

        fn log(&self, record: &Record<'_>) {
            if self.enabled(record.metadata()) {
                self.seen
                    .lines
                    .lock()
                    .unwrap()
                    .push(record.args().to_string());
            }
        }

        fn flush(&self) {
            *self.seen.flushes.lock().unwrap() += 1;
        }
    }

    fn wrapped(max: Level) -> (SuspendingLogger, Arc<Seen>) {
        let rec = Recorder {
            max,
            seen: Arc::new(Seen::default()),
        };
        let seen = Arc::clone(&rec.seen);
        (
            SuspendingLogger {
                inner: Box::new(rec),
            },
            seen,
        )
    }

    #[test]
    fn records_reach_the_wrapped_sink() {
        let (wrapper, seen) = wrapped(Level::Warn);
        wrapper.log(
            &Record::builder()
                .level(Level::Warn)
                .args(format_args!("skipped 3 non-audio files"))
                .build(),
        );
        assert_eq!(*seen.lines.lock().unwrap(), ["skipped 3 non-audio files"]);
    }

    #[test]
    fn filtered_records_are_dropped() {
        let (wrapper, seen) = wrapped(Level::Warn);
        wrapper.log(
            &Record::builder()
                .level(Level::Debug)
                .args(format_args!("chatter"))
                .build(),
        );
        assert!(seen.lines.lock().unwrap().is_empty());
    }

    #[test]
    fn enabled_and_flush_delegate() {
        let (wrapper, seen) = wrapped(Level::Info);
        assert!(wrapper.enabled(&Metadata::builder().level(Level::Warn).build()));
        assert!(!wrapper.enabled(&Metadata::builder().level(Level::Debug).build()));
        wrapper.flush();
        assert_eq!(*seen.flushes.lock().unwrap(), 1);
    }
}
