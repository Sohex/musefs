mod byte_budget;
mod db_pool;
mod error;
mod facade;
pub mod freshness;
mod lock;
mod mapping;
pub mod metrics;
mod ogg_index;
#[allow(dead_code)]
mod readahead;
mod reader;
mod refresh_diff;
mod scan;
mod template;
mod tree;

pub use db_pool::DbPool;
pub use error::{CoreError, Result};
pub use facade::{Attr, Fh, Mode, MountConfig, Musefs, PassthroughFd};
pub use musefs_db::convert;
pub use reader::{HeaderCache, ResolvedFile, read_at, read_at_with_file};
pub use scan::scan_directory_full_oracle;
pub use scan::{
    RevalidateStats, ScanOptions, ScanStats, revalidate, revalidate_with, scan_directory,
    scan_directory_with,
};
pub use template::{Template, TemplateError};
pub use tree::{Node, NodeKind, VirtualTree};

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
}
