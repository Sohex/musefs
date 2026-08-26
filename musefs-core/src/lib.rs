mod byte_budget;
mod db_pool;
mod error;
mod facade;
pub mod freshness;
mod lock;
mod mapping;
pub mod metrics;
mod ogg_index;
mod readahead;
mod reader;
mod refresh_diff;
mod scan;
mod telemetry;
mod template;
mod tree;
mod warn_limit;

pub use db_pool::DbPool;
pub use error::{CoreError, Result};
pub use facade::{Attr, Fh, Mode, MountConfig, Musefs, PassthroughFd};
pub use musefs_db::convert;
pub use readahead::{BackingReader, ReadAhead, ReadAheadPool};
pub use reader::{HeaderCache, ResolvedFile, read_at, read_at_with_file};
pub use scan::scan_directory_full_oracle;
pub use scan::{
    ChecksumTier, MatchStrictness, ProgressSink, RevalidateStats, ScanOptions, ScanProgress,
    ScanStats, revalidate, revalidate_with, scan_directory, scan_directory_with,
};
pub use telemetry::{
    AllocatorStats, CoreTelemetry, FuseTelemetry, PassthroughTelemetry, ProcessStats,
    process_stats, render_prometheus,
};
pub use template::{Template, TemplateError};
pub use tree::{InodeAllocator, Node, NodeKind, VirtualTree};
pub use warn_limit::rate_limited_warn;

#[cfg(test)]
mod cross_layer_caps {
    #[test]
    fn structural_body_cap_matches_flac_block_limit() {
        assert_eq!(
            u64::try_from(musefs_db::limits::MAX_STRUCTURAL_BODY_LEN).unwrap(),
            musefs_format::flac::MAX_BLOCK_BODY,
            "db structural body cap must equal FLAC's 24-bit block limit",
        );
    }

    /// #644 put the `tags.value` cap *at* FLAC's block ceiling rather than below
    /// it, and that equality is load-bearing twice over. It is why the cap can
    /// be described as inherited from the format rather than invented, and it is
    /// what makes an over-cap tag structurally unreachable through a legal FLAC:
    /// the whole `VORBIS_COMMENT` body is length-prefixed with 24 bits, so no
    /// comment inside it can exceed the cap. Drop the cap below this and the
    /// crash class #644 reported becomes reachable again for stock FLAC files.
    #[test]
    fn tag_value_cap_is_not_below_the_flac_block_limit() {
        assert!(
            u64::try_from(musefs_db::limits::MAX_TAG_VALUE_LEN).unwrap()
                >= musefs_format::flac::MAX_BLOCK_BODY,
            "a legal FLAC comment must never be too large for the store to hold",
        );
    }
}
