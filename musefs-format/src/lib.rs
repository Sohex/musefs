mod error;
pub mod flac;
mod input;
mod layout;
pub mod mp3;

pub use error::{FormatError, Result};
pub use input::{ArtInput, TagInput};
pub use layout::{RegionLayout, Segment};
