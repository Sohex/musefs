use std::io::IsTerminal;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};
use musefs_core::{ProgressSink, ScanProgress};

const STEP: u64 = 5;

pub(crate) fn next_milestone(prev_done: u64, done: u64, total: u64) -> Option<u64> {
    if total == 0 || done <= prev_done {
        return None;
    }
    if done >= total {
        return Some(100);
    }
    let bucket = done * 100 / total / STEP;
    let prev_bucket = prev_done * 100 / total / STEP;
    if bucket > prev_bucket {
        Some(bucket * STEP)
    } else {
        None
    }
}

enum Mode {
    Quiet,
    Tty(ProgressBar),
    Plain,
}

struct Renderer {
    mode: Mode,
    prev_done: AtomicU64,
}

impl Renderer {
    fn handle(&self, ev: ScanProgress<'_>) {
        match (&self.mode, ev) {
            (Mode::Tty(bar), ScanProgress::Discovered { found }) => {
                bar.set_message(format!("discovering files… {found} found"));
            }
            (Mode::Tty(bar), ScanProgress::Walked { total }) => {
                bar.set_style(
                    ProgressStyle::with_template(
                        "{spinner} [{elapsed_precise}] [{bar:30}] {pos}/{len} ({percent}%) {wide_msg}",
                    )
                    .expect("static template")
                    .progress_chars("##-"),
                );
                bar.set_length(total);
                bar.set_position(0);
            }
            (Mode::Tty(bar), ScanProgress::Ingested { done, path, .. }) => {
                bar.set_position(done);
                bar.set_message(basename(path));
            }
            (Mode::Plain, ScanProgress::Ingested { done, total, .. }) => {
                let prev = self.prev_done.load(Ordering::Relaxed);
                if let Some(pct) = next_milestone(prev, done, total) {
                    eprintln!("ingested {done}/{total} ({pct}%)");
                    self.prev_done.store(done, Ordering::Relaxed);
                }
            }
            _ => {}
        }
    }
}

fn basename(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map_or_else(|| path.to_string(), |n| n.to_string_lossy().into_owned())
}

pub(crate) struct ScanReporter {
    inner: Arc<Renderer>,
}

impl ScanReporter {
    pub(crate) fn new(quiet: bool) -> Self {
        let mode = if quiet {
            Mode::Quiet
        } else if std::io::stderr().is_terminal() {
            let bar = ProgressBar::new_spinner();
            bar.enable_steady_tick(Duration::from_millis(120));
            bar.set_message("discovering files…");
            Mode::Tty(bar)
        } else {
            Mode::Plain
        };
        ScanReporter {
            inner: Arc::new(Renderer {
                mode,
                prev_done: AtomicU64::new(0),
            }),
        }
    }

    pub(crate) fn sink(&self) -> Option<ProgressSink> {
        if matches!(self.inner.mode, Mode::Quiet) {
            return None;
        }
        let inner = Arc::clone(&self.inner);
        Some(ProgressSink::new(move |ev| inner.handle(ev)))
    }

    pub(crate) fn start_target(&self) {
        self.inner.prev_done.store(0, Ordering::Relaxed);
        if let Mode::Tty(bar) = &self.inner.mode {
            bar.set_style(ProgressStyle::default_spinner());
            bar.set_position(0);
            bar.set_message("discovering files…");
        }
    }

    pub(crate) fn finish(&self) {
        if let Mode::Tty(bar) = &self.inner.mode {
            bar.finish_and_clear();
        }
    }
}

#[cfg(test)]
mod milestone_tests {
    use super::next_milestone;

    #[test]
    fn zero_total_is_none() {
        assert_eq!(next_milestone(0, 0, 0), None);
    }

    #[test]
    fn no_advance_is_none() {
        assert_eq!(next_milestone(3, 3, 100), None);
    }

    #[test]
    fn single_file_is_hundred() {
        assert_eq!(next_milestone(0, 1, 1), Some(100));
    }

    #[test]
    fn final_step_below_granularity_still_fires() {
        assert_eq!(next_milestone(29, 30, 30), Some(100));
    }

    #[test]
    fn crossing_first_five_percent() {
        assert_eq!(next_milestone(0, 1, 20), Some(5));
    }

    #[test]
    fn within_a_bucket_is_none() {
        assert_eq!(next_milestone(5, 6, 100), None);
    }

    #[test]
    fn crossing_into_ten_percent() {
        assert_eq!(next_milestone(9, 10, 100), Some(10));
    }
}
