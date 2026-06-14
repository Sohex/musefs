//! Runtime telemetry surface: plain-data snapshot types and Prometheus
//! exposition-format rendering for the `.musefs-metrics/metrics` virtual file
//! (#394). All rendering lives here (most of the data is core-owned and this is
//! unit-testable without a mount); `musefs-fuse` gathers the fuse-side half and
//! the optional allocator/syscall probes, then calls [`render_prometheus`].

use std::fmt::Write;

/// Core-owned telemetry: the file-handle slab count, header/size caches, the
/// virtual-tree footprint, and refresh health. Produced by `Musefs::telemetry`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CoreTelemetry {
    pub handles_open: u64,
    pub cache_header_entries: u64,
    pub cache_header_bytes: u64,
    pub cache_header_bytes_max: u64,
    pub cache_header_hits: u64,
    pub cache_header_misses: u64,
    pub cache_size_entries: u64,
    pub readahead_budget_bytes: u64,
    pub readahead_charged_bytes: u64,
    pub tree_nodes: u64,
    pub inode_paths: u64,
    pub refresh_generation: u64,
    pub refresh_gap_fallbacks: u64,
    pub refresh_needs_rebuild: bool,
}

/// Passthrough sub-telemetry; `None` (in [`FuseTelemetry`]) off Linux.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PassthroughTelemetry {
    pub disabled: bool,
    pub active: u64,
}

/// Fuse-owned telemetry: uptime, the read/dir-handle gates and their caps, the
/// worker pool, and (Linux only) passthrough state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FuseTelemetry {
    pub uptime_seconds: u64,
    pub reads_inflight: u64,
    pub reads_inflight_max: u64,
    pub dir_handles: u64,
    pub dir_handles_max: u64,
    pub pool_workers: u64,
    pub pool_active: u64,
    pub pool_queued: u64,
    pub passthrough: Option<PassthroughTelemetry>,
}

/// jemalloc allocator stats (present only on a `jemalloc`-feature build).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AllocatorStats {
    pub allocated: u64,
    pub resident: u64,
    pub active: u64,
    pub retained: u64,
}

fn gauge(out: &mut String, name: &str, help: &str, val: u64) {
    let _ = write!(
        out,
        "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {val}\n"
    );
}

fn counter(out: &mut String, name: &str, help: &str, val: u64) {
    let _ = write!(
        out,
        "# HELP {name} {help}\n# TYPE {name} counter\n{name} {val}\n"
    );
}

/// Render a full Prometheus exposition-format document. Feature-gated blocks
/// (`alloc`, `syscalls`) are omitted entirely when their `Option` is `None`.
pub fn render_prometheus(
    core: &CoreTelemetry,
    fuse: &FuseTelemetry,
    alloc: Option<&AllocatorStats>,
    syscalls: Option<&crate::metrics::Snapshot>,
) -> String {
    let mut out = String::with_capacity(4096);

    gauge(
        &mut out,
        "musefs_uptime_seconds",
        "Seconds since the mount started.",
        fuse.uptime_seconds,
    );
    gauge(
        &mut out,
        "musefs_handles_open",
        "Open file handles in the core slab.",
        core.handles_open,
    );

    gauge(
        &mut out,
        "musefs_reads_inflight",
        "Foreground reads queued/in-flight.",
        fuse.reads_inflight,
    );
    gauge(
        &mut out,
        "musefs_reads_inflight_max",
        "Cap before reads are rejected with EAGAIN.",
        fuse.reads_inflight_max,
    );
    gauge(
        &mut out,
        "musefs_dir_handles",
        "Open directory-listing snapshots.",
        fuse.dir_handles,
    );
    gauge(
        &mut out,
        "musefs_dir_handles_max",
        "Cap before opendir is rejected with ENFILE.",
        fuse.dir_handles_max,
    );

    gauge(
        &mut out,
        "musefs_pool_workers",
        "Worker-pool size.",
        fuse.pool_workers,
    );
    gauge(
        &mut out,
        "musefs_pool_active",
        "Workers currently running a job.",
        fuse.pool_active,
    );
    gauge(
        &mut out,
        "musefs_pool_queued",
        "Jobs waiting in the worker-pool queue.",
        fuse.pool_queued,
    );

    gauge(
        &mut out,
        "musefs_cache_header_entries",
        "Resolved-file entries in the header cache.",
        core.cache_header_entries,
    );
    gauge(
        &mut out,
        "musefs_cache_header_bytes",
        "Resident inline bytes in the header cache.",
        core.cache_header_bytes,
    );
    gauge(
        &mut out,
        "musefs_cache_header_bytes_max",
        "Header-cache byte budget.",
        core.cache_header_bytes_max,
    );
    counter(
        &mut out,
        "musefs_cache_header_hits_total",
        "Raw header-cache key hits; a hit may still trigger a content-version rebuild.",
        core.cache_header_hits,
    );
    counter(
        &mut out,
        "musefs_cache_header_misses_total",
        "Raw header-cache key misses.",
        core.cache_header_misses,
    );
    gauge(
        &mut out,
        "musefs_cache_size_entries",
        "Entries in the getattr size cache.",
        core.cache_size_entries,
    );

    gauge(
        &mut out,
        "musefs_readahead_budget_bytes",
        "Backing read-ahead RAM budget (0 when read-ahead is off).",
        core.readahead_budget_bytes,
    );
    gauge(
        &mut out,
        "musefs_readahead_charged_bytes",
        "Bytes currently held across all read-ahead buffers.",
        core.readahead_charged_bytes,
    );

    gauge(
        &mut out,
        "musefs_tree_nodes",
        "Live virtual-tree inodes.",
        core.tree_nodes,
    );
    gauge(
        &mut out,
        "musefs_inode_paths",
        "Interned paths in the inode allocator.",
        core.inode_paths,
    );

    gauge(
        &mut out,
        "musefs_refresh_generation",
        "Refresh generation (bumped on each non-empty refresh).",
        core.refresh_generation,
    );
    counter(
        &mut out,
        "musefs_refresh_gap_fallbacks_total",
        "Polls that took the changelog-gap full-rebuild path.",
        core.refresh_gap_fallbacks,
    );
    gauge(
        &mut out,
        "musefs_refresh_needs_rebuild",
        "1 if a poisoned-lock recovery left a full rebuild pending.",
        u64::from(core.refresh_needs_rebuild),
    );

    if let Some(pt) = fuse.passthrough {
        gauge(
            &mut out,
            "musefs_passthrough_disabled",
            "1 if kernel passthrough is sticky-disabled.",
            u64::from(pt.disabled),
        );
        gauge(
            &mut out,
            "musefs_passthrough_active",
            "Live kernel-passthrough backing registrations.",
            pt.active,
        );
    }

    if let Some(a) = alloc {
        gauge(
            &mut out,
            "musefs_alloc_allocated_bytes",
            "jemalloc bytes allocated and in use.",
            a.allocated,
        );
        gauge(
            &mut out,
            "musefs_alloc_resident_bytes",
            "jemalloc resident bytes (RSS proxy).",
            a.resident,
        );
        gauge(
            &mut out,
            "musefs_alloc_active_bytes",
            "jemalloc bytes in active pages.",
            a.active,
        );
        gauge(
            &mut out,
            "musefs_alloc_retained_bytes",
            "jemalloc retained (lazily-purgeable) bytes.",
            a.retained,
        );
    }

    if let Some(s) = syscalls {
        counter(
            &mut out,
            "musefs_backing_opens_total",
            "Serve-path backing-file opens.",
            s.opens,
        );
        counter(
            &mut out,
            "musefs_backing_stats_total",
            "Serve-path metadata syscalls.",
            s.stats,
        );
        counter(
            &mut out,
            "musefs_backing_preads_total",
            "Serve-path positioned backing reads.",
            s.preads,
        );
        counter(
            &mut out,
            "musefs_backing_pread_bytes_total",
            "Serve-path backing bytes attempted.",
            s.pread_bytes,
        );
        counter(
            &mut out,
            "musefs_art_chunks_total",
            "Art-blob chunks streamed from the DB.",
            s.art_chunks,
        );
        counter(
            &mut out,
            "musefs_binary_tag_chunks_total",
            "Binary-tag chunks streamed from the DB.",
            s.binary_tag_chunks,
        );
        counter(
            &mut out,
            "musefs_scan_opens_total",
            "Scan-path backing-file opens.",
            s.scan_opens,
        );
        counter(
            &mut out,
            "musefs_scan_preads_total",
            "Scan-path positioned reads.",
            s.scan_preads,
        );
        counter(
            &mut out,
            "musefs_scan_bytes_total",
            "Scan-path bytes read.",
            s.scan_bytes_read,
        );
        counter(
            &mut out,
            "musefs_readahead_hits_total",
            "Reads served wholly from a read-ahead buffer (no backing pread).",
            s.readahead_hits,
        );
        counter(
            &mut out,
            "musefs_readahead_misses_total",
            "Reads that missed the read-ahead buffer and hit the backing file.",
            s.readahead_misses,
        );
    }

    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_core() -> CoreTelemetry {
        CoreTelemetry {
            handles_open: 3,
            cache_header_entries: 7,
            cache_header_bytes: 4096,
            cache_header_bytes_max: 64 * 1024 * 1024,
            cache_header_hits: 100,
            cache_header_misses: 5,
            cache_size_entries: 9,
            readahead_budget_bytes: 67_108_864,
            readahead_charged_bytes: 8192,
            tree_nodes: 42,
            inode_paths: 50,
            refresh_generation: 2,
            refresh_gap_fallbacks: 1,
            refresh_needs_rebuild: false,
        }
    }

    fn sample_fuse() -> FuseTelemetry {
        FuseTelemetry {
            uptime_seconds: 60,
            reads_inflight: 1,
            reads_inflight_max: 1024,
            dir_handles: 2,
            dir_handles_max: 1024,
            pool_workers: 8,
            pool_active: 1,
            pool_queued: 0,
            passthrough: Some(PassthroughTelemetry {
                disabled: false,
                active: 4,
            }),
        }
    }

    #[test]
    fn renders_core_and_fuse_gauges() {
        let out = render_prometheus(&sample_core(), &sample_fuse(), None, None);
        assert!(out.contains("# TYPE musefs_handles_open gauge\nmusefs_handles_open 3\n"));
        assert!(out.contains("musefs_reads_inflight 1\n"));
        assert!(out.contains("musefs_reads_inflight_max 1024\n"));
        assert!(out.contains("musefs_pool_queued 0\n"));
        assert!(out.contains("musefs_readahead_budget_bytes 67108864\n"));
        assert!(out.contains("musefs_readahead_charged_bytes 8192\n"));
        assert!(out.contains("musefs_tree_nodes 42\n"));
        // counter type for hit/miss
        assert!(out.contains(
            "# TYPE musefs_cache_header_hits_total counter\nmusefs_cache_header_hits_total 100\n"
        ));
    }

    #[test]
    fn passthrough_block_present_when_some_absent_when_none() {
        let with = render_prometheus(&sample_core(), &sample_fuse(), None, None);
        assert!(with.contains("musefs_passthrough_active 4\n"));
        assert!(with.contains("musefs_passthrough_disabled 0\n"));

        let mut f = sample_fuse();
        f.passthrough = None;
        let without = render_prometheus(&sample_core(), &f, None, None);
        assert!(!without.contains("musefs_passthrough"));
    }

    #[test]
    fn alloc_and_syscall_blocks_are_omitted_when_none() {
        let out = render_prometheus(&sample_core(), &sample_fuse(), None, None);
        assert!(!out.contains("musefs_alloc_"));
        assert!(!out.contains("musefs_backing_"));
    }

    #[test]
    fn alloc_block_present_when_some() {
        let a = AllocatorStats {
            allocated: 1,
            resident: 2,
            active: 3,
            retained: 4,
        };
        let out = render_prometheus(&sample_core(), &sample_fuse(), Some(&a), None);
        assert!(out.contains("musefs_alloc_resident_bytes 2\n"));
        assert!(out.contains("musefs_alloc_retained_bytes 4\n"));
    }

    #[test]
    fn syscall_block_present_when_some() {
        let s = crate::metrics::Snapshot {
            opens: 11,
            preads: 22,
            readahead_hits: 33,
            readahead_misses: 44,
            ..crate::metrics::Snapshot::default()
        };
        let out = render_prometheus(&sample_core(), &sample_fuse(), None, Some(&s));
        assert!(out.contains(
            "# TYPE musefs_backing_opens_total counter\nmusefs_backing_opens_total 11\n"
        ));
        assert!(out.contains("musefs_backing_preads_total 22\n"));
        assert!(out.contains(
            "# TYPE musefs_readahead_hits_total counter\nmusefs_readahead_hits_total 33\n"
        ));
        assert!(out.contains("musefs_readahead_misses_total 44\n"));
    }

    #[test]
    fn refresh_needs_rebuild_true_renders_as_one() {
        let mut c = sample_core();
        c.refresh_needs_rebuild = true;
        let out = render_prometheus(&c, &sample_fuse(), None, None);
        assert!(out.contains("musefs_refresh_needs_rebuild 1\n"));
    }
}
