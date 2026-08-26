//! Process-wide rate limit for serve-path failure warns (#650).
//!
//! Both the FUSE errno-reply path and the synthesis paths inside this crate
//! log per-file failures, and both scale with library size: one warn per file
//! over a 200,000-track walk is tens of MB of log for a single enumeration
//! (#631). One budget covers all of them — the operator's concern is total
//! serve-path log volume, not per-crate volume — so `musefs-fuse` routes
//! `reply_errno` through [`serve_warn!`](crate::serve_warn) rather than owning
//! a limiter of its own.
//!
//! Lives in `musefs-core` (the integration layer) next to [`crate::telemetry`],
//! since both crates above it feed the same budget — which is also what makes
//! [`serve_warns_suppressed`] a local read for
//! `musefs_serve_warns_suppressed_total` (#653).
//!
//! The emit side is a **macro**, not a function, and deliberately so: a `log`
//! record takes its target from `module_path!()` at the expansion site, so a
//! shared helper function would stamp every serve-path warn with this module's
//! path. That would collapse provenance — a synthesis warn and an errno reply
//! would be indistinguishable by target — and break the per-crate `RUST_LOG`
//! filtering the troubleshooting guide documents
//! (`RUST_LOG=warn,musefs_fuse=debug`). Sharing the *decision* while expanding
//! the *emit* at the call site keeps one budget and one honest target per site.

use std::sync::atomic::{AtomicU64, Ordering};

/// `log` re-export so [`serve_warn!`](crate::serve_warn) expands in crates that
/// do not name `log` themselves. Implementation detail of the macro.
#[doc(hidden)]
pub use log as __log;

/// Burst of serve-path failure warns allowed per [`WARN_WINDOW_SECS`] window;
/// the rest of the window is downgraded to debug and counted.
const WARN_BURST: u64 = 10;
/// Length of one warn rate-limit window, in seconds. Public because
/// [`serve_warn!`](crate::serve_warn) names it in the suppressed-count
/// parenthetical it formats at the call site.
pub const WARN_WINDOW_SECS: u64 = 30;

/// What [`decide`] told the caller to do with one warn. Public as the macro's
/// expansion target; call [`serve_warn!`](crate::serve_warn) rather than
/// matching on this by hand.
#[derive(Debug, PartialEq, Eq)]
pub enum WarnDecision {
    /// Emit the warn. `suppressed` is the count dropped since the last window
    /// opened (nonzero only on the first warn of a new window) so the log
    /// still reflects the true failure volume.
    Log { suppressed: u64 },
    /// Over budget for this window: downgrade to debug.
    Suppress,
}

/// Rate limit for serve-path failure warns. Without it, a library enumeration
/// over a corpus whose backing files are missing emits one warn per file —
/// 200,000 lines (tens of MB of log) for a single walk (#631). Failures stay
/// visible — a burst per window plus a suppressed-count on each new window —
/// without the log scaling with library size. Lock-free; a lost race under
/// concurrent windows-rollover merely logs one extra line.
struct WarnLimiter {
    /// Unix seconds when the current window opened.
    window_start: AtomicU64,
    /// Warns emitted in the current window (the burst budget).
    in_window: AtomicU64,
    /// Warns downgraded in the current window, reported when the next opens.
    suppressed: AtomicU64,
    /// Every warn ever downgraded, never reset: the monotonic source for
    /// `musefs_serve_warns_suppressed_total` (#653). Distinct from
    /// `suppressed`, which each new window drains into its first log line.
    suppressed_total: AtomicU64,
}

impl WarnLimiter {
    const fn new() -> WarnLimiter {
        WarnLimiter {
            window_start: AtomicU64::new(0),
            in_window: AtomicU64::new(0),
            suppressed: AtomicU64::new(0),
            suppressed_total: AtomicU64::new(0),
        }
    }

    /// Decide one warn's fate at `now_secs` (Unix seconds; injected so tests
    /// don't sleep).
    fn decide(&self, now_secs: u64) -> WarnDecision {
        let start = self.window_start.load(Ordering::Relaxed);
        if now_secs >= start.saturating_add(WARN_WINDOW_SECS)
            && self
                .window_start
                .compare_exchange(start, now_secs, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            self.in_window.store(1, Ordering::Relaxed);
            return WarnDecision::Log {
                suppressed: self.suppressed.swap(0, Ordering::Relaxed),
            };
        }
        if self.in_window.fetch_add(1, Ordering::Relaxed) < WARN_BURST {
            WarnDecision::Log { suppressed: 0 }
        } else {
            self.suppressed.fetch_add(1, Ordering::Relaxed);
            self.suppressed_total.fetch_add(1, Ordering::Relaxed);
            WarnDecision::Suppress
        }
    }

    /// Warns downgraded to debug over the limiter's whole lifetime.
    fn suppressed_total(&self) -> u64 {
        self.suppressed_total.load(Ordering::Relaxed)
    }
}

/// The one process-wide limiter for serve-path failure warns.
static SERVE_WARN_LIMITER: WarnLimiter = WarnLimiter::new();

/// Ask [`SERVE_WARN_LIMITER`] what to do with one serve-path warn, now.
/// Public as the decision half of [`serve_warn!`](crate::serve_warn), which
/// owns the emit half; nothing else should need it.
pub fn decide() -> WarnDecision {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    SERVE_WARN_LIMITER.decide(now)
}

/// Emit a serve-path failure warn through the process-wide limiter, at the
/// caller's own log target. Takes the same arguments as [`log::warn!`].
/// Over-budget messages drop to debug so `RUST_LOG=debug` still sees every
/// failure.
///
/// A macro rather than a function so `module_path!()` — and therefore the log
/// record's target — resolves to the *call site*: `reply_errno` still logs
/// under `musefs_fuse`, synthesis still logs under `musefs_core::reader` /
/// `musefs_core::mapping`, and `RUST_LOG=warn,musefs_fuse=debug` still reaches
/// exactly what it did before the limiter was shared. Only the budget is
/// global; the attribution is not.
#[macro_export]
macro_rules! serve_warn {
    ($($arg:tt)+) => {
        match $crate::warn_limit::decide() {
            $crate::warn_limit::WarnDecision::Log { suppressed: 0 } => {
                $crate::warn_limit::__log::warn!($($arg)+);
            }
            $crate::warn_limit::WarnDecision::Log { suppressed } => {
                $crate::warn_limit::__log::warn!(
                    "{} ({} similar serve-path warnings suppressed in the last {}s)",
                    ::core::format_args!($($arg)+),
                    suppressed,
                    $crate::warn_limit::WARN_WINDOW_SECS,
                );
            }
            $crate::warn_limit::WarnDecision::Suppress => {
                $crate::warn_limit::__log::debug!(
                    "{} (over warn budget)",
                    ::core::format_args!($($arg)+),
                );
            }
        }
    };
}

/// Serve-path warns downgraded to debug since process start, rendered as
/// `musefs_serve_warns_suppressed_total` (#653). Neither a gauge nor the
/// parenthetical on an admitted log line can stand in for it: suppression is
/// bursty by construction, so a scrape landing between bursts sees nothing and
/// the operator cannot tell a quiet serve path from a throttled one.
pub(crate) fn serve_warns_suppressed() -> u64 {
    SERVE_WARN_LIMITER.suppressed_total()
}

/// Log-capture harness shared by the target-attribution tests. Any module that
/// hosts a `serve_warn!` call site can pin the target it logs under from its
/// own test module; `log` permits one global logger per test binary, so the
/// installer is `Once`-guarded and lookups filter the shared buffer by message.
#[cfg(test)]
pub(crate) mod log_capture {
    use std::sync::Mutex;

    static CAPTURED: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
    static CAPTURE_LOGGER: CaptureLogger = CaptureLogger;

    /// Global logger keeping every record's target and rendered message.
    struct CaptureLogger;

    impl log::Log for CaptureLogger {
        fn enabled(&self, _: &log::Metadata<'_>) -> bool {
            true
        }
        fn log(&self, record: &log::Record<'_>) {
            CAPTURED
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((record.target().to_string(), record.args().to_string()));
        }
        fn flush(&self) {}
    }

    /// Install the capture logger for this test binary (idempotent).
    pub(crate) fn install() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            log::set_logger(&CAPTURE_LOGGER)
                .expect("no other logger may be installed in this test binary");
            // Over-budget warns drop to debug; the target must be pinned on
            // those too, so the capture has to see them.
            log::set_max_level(log::LevelFilter::Debug);
        });
    }

    /// Every captured message containing `needle`, in emission order.
    pub(crate) fn messages_containing(needle: &str) -> Vec<String> {
        CAPTURED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(_, m)| m.contains(needle))
            .map(|(_, m)| m.clone())
            .collect()
    }

    /// The target of the one captured record whose message contains `needle`.
    pub(crate) fn target_of(needle: &str) -> String {
        let captured = CAPTURED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let hits: Vec<&(String, String)> = captured
            .iter()
            .filter(|(_, m)| m.contains(needle))
            .collect();
        assert_eq!(hits.len(), 1, "expected exactly one record for {needle:?}");
        hits[0].0.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::warn_limit::log_capture::{
        install as install_capture_logger, messages_containing, target_of,
    };

    #[test]
    fn warn_limiter_allows_a_burst_then_suppresses() {
        let l = WarnLimiter::new();
        // First call opens the window; the burst budget covers WARN_BURST warns.
        for i in 0..WARN_BURST {
            assert_eq!(
                l.decide(100),
                WarnDecision::Log { suppressed: 0 },
                "warn {i} is within the burst"
            );
        }
        assert_eq!(l.decide(100), WarnDecision::Suppress);
        assert_eq!(l.decide(100), WarnDecision::Suppress);
    }

    #[test]
    fn warn_limiter_new_window_reports_suppressed_count() {
        let l = WarnLimiter::new();
        for _ in 0..WARN_BURST {
            l.decide(100);
        }
        for _ in 0..5 {
            assert_eq!(l.decide(100), WarnDecision::Suppress);
        }
        // Window rolls over: log again, carrying the dropped count once.
        assert_eq!(
            l.decide(100 + WARN_WINDOW_SECS),
            WarnDecision::Log { suppressed: 5 }
        );
        assert_eq!(
            l.decide(100 + WARN_WINDOW_SECS),
            WarnDecision::Log { suppressed: 0 }
        );
    }

    #[test]
    fn suppressed_total_is_monotonic_across_windows() {
        // The per-window count is drained into the next window's first log
        // line; the total is not, so the metric survives a rollover (#653).
        let l = WarnLimiter::new();
        for _ in 0..WARN_BURST + 5 {
            l.decide(100);
        }
        assert_eq!(l.suppressed_total(), 5);
        assert_eq!(
            l.decide(100 + WARN_WINDOW_SECS),
            WarnDecision::Log { suppressed: 5 }
        );
        assert_eq!(l.suppressed_total(), 5, "rollover must not reset the total");
        // That rollover call spent one of the new window's budget, so these
        // WARN_BURST + 3 leave 4 over budget: 5 + 4.
        for _ in 0..WARN_BURST + 3 {
            l.decide(100 + WARN_WINDOW_SECS);
        }
        assert_eq!(l.suppressed_total(), 9);
    }

    #[test]
    fn admitted_warns_never_count_as_suppressed() {
        let l = WarnLimiter::new();
        for _ in 0..WARN_BURST {
            assert_eq!(l.decide(100), WarnDecision::Log { suppressed: 0 });
        }
        assert_eq!(l.suppressed_total(), 0);
    }

    #[test]
    fn process_wide_counter_advances_through_serve_warn() {
        // End-to-end over the real static: a burst past the budget must move
        // the counter the metric reads, whichever crate emitted the warns.
        // Delta-based and `>=`, since the static is shared with the rest of
        // the suite and the window may already be part-spent.
        let before = serve_warns_suppressed();
        for i in 0..=WARN_BURST * 2 {
            crate::serve_warn!("warn-limit self-test {i}");
        }
        assert!(
            serve_warns_suppressed() > before,
            "an over-budget burst must be counted"
        );
    }

    /// A second module so the assertion is "the record follows the call site",
    /// not "every record carries one fixed string".
    mod elsewhere {
        /// Returns `(this module's path, the message it logged)`.
        pub fn emit(tag: &str) -> (&'static str, String) {
            let msg = format!("serve-warn target probe from elsewhere {tag}");
            crate::serve_warn!("{msg}");
            (module_path!(), msg)
        }
    }

    #[test]
    fn serve_warn_is_attributed_to_the_call_site_not_the_limiter() {
        // The whole reason `serve_warn!` is a macro: a helper *function* would
        // stamp every serve-path record with `musefs_core::warn_limit`,
        // silently breaking per-crate `RUST_LOG` filtering and collapsing
        // synthesis warns together with errno-path warns.
        install_capture_logger();
        let here = format!("serve-warn target probe from {}", module_path!());
        crate::serve_warn!("{here}");
        let (there_module, there) = elsewhere::emit("a");

        assert_eq!(target_of(&here), module_path!());
        assert_eq!(target_of(&there), there_module);
        assert_ne!(
            target_of(&here),
            target_of(&there),
            "two call sites in different modules must not share a target"
        );
        assert!(
            !target_of(&here).ends_with("warn_limit"),
            "no record may be attributed to the limiter's own module"
        );
    }

    #[test]
    fn over_budget_records_keep_the_debug_suffix() {
        // The macro must render the downgraded form exactly as the function it
        // replaced did: the caller's message, then " (over warn budget)".
        install_capture_logger();
        let tag = "over-budget suffix probe";
        for _ in 0..WARN_BURST * 3 {
            crate::serve_warn!("{tag} payload");
        }
        let msgs = messages_containing(tag);
        assert!(
            msgs.iter()
                .any(|m| m == "over-budget suffix probe payload (over warn budget)"),
            "expected a downgraded record with the exact suffix, got {msgs:?}"
        );
    }

    #[test]
    fn art_read_failure_is_attributed_to_the_mapping_module() {
        // The real production site (`DbArtSource::read_window`, #650), not a
        // synthetic call: an art id that does not exist fails the blob read.
        install_capture_logger();
        let db = musefs_db::Db::open_in_memory().unwrap();
        let src = crate::mapping::DbArtSource(&db);
        let mut buf = [0u8; 4];
        let err = musefs_format::ogg::ArtSource::read_window(&src, 424_242, 0, &mut buf);
        assert!(err.is_err(), "a missing art blob must fail the read");
        assert_eq!(
            target_of("ogg synthesis: art 424242 read failed"),
            "musefs_core::mapping"
        );
    }
}
