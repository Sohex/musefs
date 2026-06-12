mod convert;
mod error;
pub mod flac;
mod input;
mod layout;
pub mod mp3;
pub mod mp4;
pub mod ogg;
pub mod probe;
mod size;
mod tagmap;
mod vorbiscomment;
pub mod wav;

#[cfg(any(test, feature = "fuzzing"))]
pub mod fuzz_check;

pub use error::{FormatError, Result};
pub use input::{
    ArtInput, BinaryTagInput, BlobLen, EmbeddedBinaryTag, EmbeddedPicture, PictureType, TagInput,
};
pub use layout::{LayoutError, RegionLayout, Segment};
pub use ogg::{Codec, OggHeader, OggScan};
pub use probe::Extent;
pub use vorbiscomment::is_valid_key as is_valid_vorbis_key;

// tagmap is pure &str→key mapping with no byte parsing and is already exercised
// indirectly by the per-format fuzz targets (which pass arbitrary tag keys through
// synthesize_layout), so no dedicated tagmap fuzz target is needed.
#[cfg(feature = "fuzzing")]
pub use mp3::build_id3v2_segments;
#[cfg(feature = "fuzzing")]
pub use vorbiscomment::parse as parse_vorbis_comment;
