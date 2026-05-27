mod error;
pub mod flac;
mod input;
mod layout;
pub mod mp3;
pub mod mp4;
pub mod ogg;
mod tagmap;
mod vorbiscomment;
pub mod wav;

#[cfg(any(test, feature = "fuzzing"))]
pub mod fuzz_check;

pub use error::{FormatError, Result};
pub use input::{ArtInput, EmbeddedPicture, TagInput};
pub use layout::{RegionLayout, Segment};
pub use ogg::{Codec, OggHeader, OggScan};
