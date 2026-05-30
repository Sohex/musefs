# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **crates.io distribution:** the `musefs` binary is published to crates.io as of
  this release and installable with `cargo install musefs`. A new thin `musefs` wrapper crate
  owns the binary (`musefs-cli` is now a library crate), and a tag-triggered
  release workflow publishes all crates in dependency order.
- **Fuzzing & property tests:** coverage-guided `cargo-fuzz` targets for every
  format parser (FLAC, MP3, MP4, Ogg, WAV) and the byte-level primitives (Ogg
  page parsing, base64 windowing, VorbisComment), plus `proptest` invariants —
  panic-freedom, the byte-identical audio guarantee, and tag round-trip — an
  end-to-end read-fidelity property, and a `mutagen` interop test asserting an
  independent reader sees the tags we synthesize.

### Fixed

- **VorbisComment parse OOM (DoS):** a crafted comment block declaring a huge
  entry count made `Vec::with_capacity` attempt a multi-gigabyte allocation; the
  pre-allocation is now bounded by the readable byte count. Found by the new
  `vorbiscomment` fuzz target.
- **MP4 box-bounds integer overflow:** an untrusted 64-bit extended box size made
  the box-bounds check (`pos + total`) overflow `usize` — a panic in debug and a
  silent wrap in release that accepted a bogus box length. The addition is now
  checked. Found by the `mp4` fuzz target.
- **ID3v2 parsing unbounded allocation (DoS):** the `id3` crate eagerly allocates
  a frame's declared size (ID3v2.3 frame sizes are plain 32-bit, up to 4 GiB), so
  a crafted tag could exhaust memory at scan time — via an MP3 or a WAV embedded
  `id3 ` chunk. Parsing is now gated on validated ID3v2 frame bounds and an
  ID3v2 tag at offset 0 (the `id3` reader scans forward). Found by the `mp3` and
  `wav` fuzz targets.

## [0.2.0] - 2026-05-27

First public release.

### Added

- **Formats:** synthesis for M4A/M4B (MP4), Ogg (Opus, Vorbis, FLAC-in-Ogg), and
  WAV, alongside the existing FLAC and MP3 — metadata generated on the fly from
  the SQLite store and spliced in front of byte-identical backing audio.
- **Arbitrary tag support:** a single canonical tag vocabulary maps common fields
  to each format's native slot (ID3 frame / MP4 atom / Vorbis field); any other
  tag round-trips through the format's extension slot (ID3 `TXXX`, MP4 `----`
  freeform, raw Vorbis field). User-defined key casing is preserved.
- **beets plugin** (`contrib/beets/`): syncs beets' canonical tags and cover art
  into the store keyed by each file's real path, with no remount and no audio
  rewrite.
- **Performance, concurrency & caching pass:** worker-pool offload of blocking
  reads, lock-free virtual-tree swap, per-handle I/O, a bounded LRU header-layout
  cache, debounced single-flighted refresh with stable inodes, kernel/mount
  tuning flags, bounded-memory MP4 resolves, and opt-in `--keep-cache` with
  auto-invalidation.

### Notes

- Read-only mount; tag edits happen out-of-band against the SQLite store and are
  picked up automatically (`PRAGMA data_version` polling). See the README "Tag
  handling" section for round-trip limitations.

## [0.1.0]

- Initial MVP (FLAC and MP3 synthesis, virtual tree with beets-style templates,
  `synthesis` / `structure-only` mount modes, auto-refresh, `scan` /
  `scan --revalidate`). Never published publicly; superseded by 0.2.0.
