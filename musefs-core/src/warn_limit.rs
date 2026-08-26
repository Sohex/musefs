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
//! since both crates above it feed the same budget.

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
}

impl WarnLimiter {
    const fn new() -> WarnLimiter {
        WarnLimiter {
            window_start: AtomicU64::new(0),
            in_window: AtomicU64::new(0),
            suppressed: AtomicU64::new(0),
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
            WarnDecision::Suppress
        }
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
}
