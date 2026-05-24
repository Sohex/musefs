mod error;
pub mod flac;
pub mod mp3;
mod input;
mod layout;

pub use error::{FormatError, Result};
pub use input::{ArtInput, TagInput};
pub use layout::{RegionLayout, Segment};
