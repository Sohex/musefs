//! Process-wide rate limit for serve-path failure warns (#650).
//!
//! Both the FUSE errno-reply path and the synthesis paths inside this crate
//! log per-file failures, and both scale with library size: one warn per file
//! over a 200,000-track walk is tens of MB of log for a single enumeration
//! (#631). One budget covers all of them — the operator's concern is total
//! serve-path log volume, not per-crate volume — so `musefs-fuse` routes
//! `reply_errno` through [`rate_limited_warn`] rather than owning a limiter of
//! its own.
//!
//! Lives in `musefs-core` (the integration layer) next to [`crate::telemetry`],
//! since both crates above it feed the same budget — which is also what makes
//! [`serve_warns_suppressed`] a local read for
//! `musefs_serve_warns_suppressed_total` (#653).

use std::sync::atomic::{AtomicU64, Ordering};

/// Burst of serve-path failure warns allowed per [`WARN_WINDOW_SECS`] window;
/// the rest of the window is downgraded to debug and counted.
const WARN_BURST: u64 = 10;
/// Length of one warn rate-limit window, in seconds.
const WARN_WINDOW_SECS: u64 = 30;

/// What [`WarnLimiter::decide`] told the caller to do with one warn.
#[derive(Debug, PartialEq, Eq)]
enum WarnDecision {
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

/// Emit a serve-path failure warn through [`SERVE_WARN_LIMITER`]. Over-budget
/// messages drop to debug so `RUST_LOG=debug` still sees every failure. Callers
/// pass `format_args!(...)`, so the message is formatted once, at whichever
/// level it ends up on.
pub fn rate_limited_warn(message: std::fmt::Arguments<'_>) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    match SERVE_WARN_LIMITER.decide(now) {
        WarnDecision::Log { suppressed: 0 } => log::warn!("{message}"),
        WarnDecision::Log { suppressed } => log::warn!(
            "{message} ({suppressed} similar serve-path warnings suppressed in the last {WARN_WINDOW_SECS}s)"
        ),
        WarnDecision::Suppress => log::debug!("{message} (over warn budget)"),
    }
}

/// Serve-path warns downgraded to debug since process start, rendered as
/// `musefs_serve_warns_suppressed_total` (#653). Neither a gauge nor the
/// parenthetical on an admitted log line can stand in for it: suppression is
/// bursty by construction, so a scrape landing between bursts sees nothing and
/// the operator cannot tell a quiet serve path from a throttled one.
pub(crate) fn serve_warns_suppressed() -> u64 {
    SERVE_WARN_LIMITER.suppressed_total()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn process_wide_counter_advances_through_rate_limited_warn() {
        // End-to-end over the real static: a burst past the budget must move
        // the counter the metric reads, whichever crate emitted the warns.
        // Delta-based and `>=`, since the static is shared with the rest of
        // the suite and the window may already be part-spent.
        let before = serve_warns_suppressed();
        for i in 0..=WARN_BURST * 2 {
            rate_limited_warn(format_args!("warn-limit self-test {i}"));
        }
        assert!(
            serve_warns_suppressed() > before,
            "an over-budget burst must be counted"
        );
    }
}
