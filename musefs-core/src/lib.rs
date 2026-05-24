mod error;
mod mapping;
mod reader;
mod scan;
mod template;
mod tree;
mod facade;

pub use error::{CoreError, Result};
// Re-exports below are uncommented as each module is implemented in later tasks:
// pub use facade::{Attr, MountConfig, Musefs};
pub use reader::{read_at, HeaderCache, ResolvedFile};
// pub use scan::{scan_directory, ScanStats};
pub use template::render_path;
pub use tree::{Node, NodeKind, VirtualTree};
