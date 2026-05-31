//! Shared outcome type for *bounded* metadata probing: a format parser is given
//! only a `prefix` of the file (plus the true `file_len`) and either completes,
//! or reports the exact byte offset it must reach to continue.

/// Result of a bounded metadata probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Extent<T> {
    /// The metadata region is fully present in the prefix; here is the parse.
    Complete(T),
    /// The prefix is too short. Read at least up to `up_to` bytes (capped at the
    /// file length) and retry. `up_to` is strictly greater than the current
    /// prefix length unless the parser cannot bound its need, in which case the
    /// caller falls back to reading the whole file.
    NeedMore { up_to: u64 },
}
