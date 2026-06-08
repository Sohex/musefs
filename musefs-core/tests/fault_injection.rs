//! Verifies the programmatic per-pread fault setter. Its own single-test binary:
//! the per-pread fault cell is a process-global OnceLock, so a dedicated binary
//! guarantees `set_fault_pread` runs before any `on_pread` reads/seeds the cell.
#![cfg(feature = "metrics")]

use std::time::{Duration, Instant};

#[test]
fn set_fault_pread_injects_latency_without_env() {
    musefs_core::metrics::set_fault_pread(Some(Duration::from_millis(20)));
    let t = Instant::now();
    musefs_core::metrics::on_pread(0);
    assert!(
        t.elapsed() >= Duration::from_millis(15),
        "on_pread should sleep for the programmatically-set fault duration"
    );
}
