//! Comparable run reporting for the SP0 bench harness.

/// One measured run. `fsyncs`/`peak_rss_kib` are `None` when not applicable
/// (e.g. fsyncs need the SP0b passthrough FS; RSS is meaningful only in-process).
pub struct RunReport {
    pub label: String,
    pub tier: String,
    pub storage: String,
    pub wall_ms: u128,
    pub opens: u64,
    pub preads: u64,
    pub fsyncs: Option<u64>,
    pub peak_rss_kib: Option<u64>,
}

impl RunReport {
    pub fn header() -> String {
        format!(
            "{:<10} {:<14} {:<10} {:>10} {:>10} {:>10} {:>10} {:>12}",
            "label", "tier", "storage", "wall_ms", "opens", "preads", "fsyncs", "rss_kib"
        )
    }

    pub fn row(&self) -> String {
        let opt = |v: Option<u64>| v.map_or_else(|| "n/a".into(), |x| x.to_string());
        format!(
            "{:<10} {:<14} {:<10} {:>10} {:>10} {:>10} {:>10} {:>12}",
            self.label,
            self.tier,
            self.storage,
            self.wall_ms,
            self.opens,
            self.preads,
            opt(self.fsyncs),
            opt(self.peak_rss_kib),
        )
    }
}

/// Peak resident set size (high-water mark) in KiB, from `/proc/self/status`
/// `VmHWM`. Linux only; `None` elsewhere or if unreadable.
pub fn peak_rss_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            return rest.trim().trim_end_matches(" kB").trim().parse().ok();
        }
    }
    None
}
