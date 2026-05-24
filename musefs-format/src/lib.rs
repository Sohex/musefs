mod error;
mod input;
mod layout;
pub mod flac;

pub use error::{FormatError, Result};
pub use input::{ArtInput, TagInput};
pub use layout::{RegionLayout, Segment};
