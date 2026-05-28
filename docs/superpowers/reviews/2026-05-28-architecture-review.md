# musefs Architecture Review

## Executive Summary

musefs has a strong architecture for its central invariant: original audio bytes
are not stored in SQLite, not rewritten during synthesis, and are served through
explicit backing-file segments. The crate layering is clean, `RegionLayout`
makes most byte ownership visible, FUSE and CLI stay thin, and the verification
surface is broader than usual for a filesystem project: property tests, fuzzing,
interop, and real FUSE tests all exist.

The main architectural risks are not broad design failures. They are narrow
places where an implicit assumption carries too much weight: one DB pool per
process, path metadata validation before opening a backing fd, path-stability
assumptions during keep-cache invalidation, and external writer behavior around
the SQLite contract. These are worth tightening before the codebase grows more
integrations.

## Review Route

This review traces the invariant from backing-file scan and external SQLite
writers, through the DB contract, format synthesis, read assembly, cache refresh,
FUSE exposure, CLI workflows, and the beets plugin.

## Findings

### High Priority

#### `DbPool` thread-local connections are keyed only by thread

**Risk:** A process that creates two file-backed `DbPool`s on the same thread can
reuse the first pool's read-only SQLite connection for the second pool. That
would cross library/mount boundaries and can resolve tracks against the wrong
database.

**Evidence:** `DbPool` documents a one-pool-per-process assumption, but the
thread-local is a single `Option<Db>` rather than keyed by pool or path
([musefs-core/src/db_pool.rs](/home/cfutro/git/musefs/musefs-core/src/db_pool.rs:9),
[musefs-core/src/db_pool.rs](/home/cfutro/git/musefs/musefs-core/src/db_pool.rs:25)).
`with` initializes the slot only when it is empty, regardless of the current
pool's path ([musefs-core/src/db_pool.rs](/home/cfutro/git/musefs/musefs-core/src/db_pool.rs:65)).

**Recommendation:** Either enforce the one-mount assumption explicitly at the
API boundary or key the thread-local by canonical DB path. Add a same-thread
two-pool regression test.

#### Open-handle reads can race backing-file replacement

**Risk:** `HeaderCache::resolve` validates backing path metadata before opening
the backing file, but `open_handle` then opens the path and stores the fd without
validating the opened fd's metadata. A replacement between validation and open
can serve bytes that do not match the resolved layout.

**Evidence:** `resolve` stats `track.backing_path` and compares size/mtime
([musefs-core/src/reader.rs](/home/cfutro/git/musefs/musefs-core/src/reader.rs:198)).
`open_handle` reuses that resolved layout, opens `resolved.backing_path`, and
stores both without an fd-level metadata check
([musefs-core/src/facade.rs](/home/cfutro/git/musefs/musefs-core/src/facade.rs:398)).
Subsequent reads use the stored fd and cached layout directly
([musefs-core/src/facade.rs](/home/cfutro/git/musefs/musefs-core/src/facade.rs:357)).

**Recommendation:** Validate the opened fd with `metadata()` before storing the
handle. The check should compare size and mtime to the resolved track contract,
and the remaining size/mtime limitations should be documented.

### Medium Priority

#### Keep-cache invalidation assumes path-stable changed tracks

**Risk:** `poll_refresh_notify` reports changed tracks by looking up their inode
in the new tree. If tags change the rendered path, the new inode may be reported
while the old inode's kernel page cache is left untouched until eviction.

**Evidence:** The comment says changed bytes are reported when path/inode is
stable ([musefs-core/src/facade.rs](/home/cfutro/git/musefs/musefs-core/src/facade.rs:213)),
but the loop only checks `content_version` changes and then calls
`tree.inode_of_track` on the rebuilt tree
([musefs-core/src/facade.rs](/home/cfutro/git/musefs/musefs-core/src/facade.rs:261)).

**Recommendation:** Compare old and new track-to-inode mappings, or narrow the
callback semantics to unchanged-path updates and document the limitation. Add
coverage for a retag that changes the virtual path while `--keep-cache` is on.

#### Refresh failures are debounced like successful polls

**Risk:** `poll_refresh_notify` updates `last_poll` before reading
`data_version` and rebuilding. A transient DB or rebuild error can delay retry
until the debounce interval expires.

**Evidence:** `last_poll` is stamped before `data_version` and `rebuild`
([musefs-core/src/facade.rs](/home/cfutro/git/musefs/musefs-core/src/facade.rs:222),
[musefs-core/src/facade.rs](/home/cfutro/git/musefs/musefs-core/src/facade.rs:232),
[musefs-core/src/facade.rs](/home/cfutro/git/musefs/musefs-core/src/facade.rs:254)).
The version stamp is correctly committed only after success
([musefs-core/src/facade.rs](/home/cfutro/git/musefs/musefs-core/src/facade.rs:276)).

**Recommendation:** Track attempted poll time separately from successful
refresh, or allow failed refresh attempts to retry without debounce suppression.

#### FUSE refresh and invalidation errors are silent

**Risk:** If refresh or `inval_inode` fails, the mount can keep serving stale
structure or cached bytes without any operator-visible signal.

**Evidence:** The FUSE refresh helper discards results from
`poll_refresh_notify`, `poll_refresh`, and `inval_inode`
([musefs-fuse/src/lib.rs](/home/cfutro/git/musefs/musefs-fuse/src/lib.rs:153),
[musefs-fuse/src/lib.rs](/home/cfutro/git/musefs/musefs-fuse/src/lib.rs:156),
[musefs-fuse/src/lib.rs](/home/cfutro/git/musefs/musefs-fuse/src/lib.rs:161)).

**Recommendation:** Keep the adapter thin, but add minimal logging or metrics
for refresh and invalidation failures. A focused test seam would make this easier
to verify without requiring real FUSE failure injection.

#### Ogg art length guard does not cover the full comment value

**Risk:** For Opus/Vorbis cover art, the guard checks only base64 image length,
but the emitted VorbisComment value length also includes the key and base64
picture prefix before casting to `u32`.

**Evidence:** `build_packets_with_art` checks `b64_len(a.meta.data_len)` against
`u32::MAX` ([musefs-format/src/ogg/mod.rs](/home/cfutro/git/musefs/musefs-format/src/ogg/mod.rs:294)).
`comment_packet_chunks` computes `KEY.len() + b64_prefix.len() + b64_len(...)`
and writes `value_len as u32`
([musefs-format/src/ogg/mod.rs](/home/cfutro/git/musefs/musefs-format/src/ogg/mod.rs:341)).

**Recommendation:** Guard the full comment value length before emitting the
length field. This is a small correctness hardening change in a high-risk parser
area.

#### Ogg memory accounting omits resident page indexes

**Risk:** The header cache budget counts inline layout bytes, but an Ogg
`ResolvedFile` can also retain a lazily built page index. Page-dense files can
therefore consume memory outside the advertised cache budget.

**Evidence:** `ResolvedFile` holds an `ogg_index: OnceCell<Arc<OggPageIndex>>`
([musefs-core/src/reader.rs](/home/cfutro/git/musefs/musefs-core/src/reader.rs:27)).
`OggPageIndex` owns a `Vec<IndexedPage>`
([musefs-core/src/ogg_index.rs](/home/cfutro/git/musefs/musefs-core/src/ogg_index.rs:25)).
`cache_bytes` counts only `Segment::Inline` bytes
([musefs-core/src/reader.rs](/home/cfutro/git/musefs/musefs-core/src/reader.rs:330)).

**Recommendation:** Account estimated Ogg index bytes in cache sizing or add a
separate bounded policy for indexes.

#### SQLite structural rows are more permissive than the safe writer path

**Risk:** The DB schema is the external contract, but SQL constraints do not
fully express safe structural values for `tracks` or art lengths. Rust scanning
and Beets currently stay on safe paths, but a future external writer could write
invalid offsets, lengths, or format values.

**Evidence:** The schema stores `audio_offset`, `audio_length`, and `format`
without SQL-level checks
([musefs-db/src/schema.rs](/home/cfutro/git/musefs/musefs-db/src/schema.rs:5)).
The `art` table also stores `byte_len` separately from `data` without a
SQL-level consistency check
([musefs-db/src/schema.rs](/home/cfutro/git/musefs/musefs-db/src/schema.rs:31)).
Beets avoids structural writes by invoking `musefs scan` before syncing metadata
([contrib/beets/beetsplug/musefs.py](/home/cfutro/git/musefs/contrib/beets/beetsplug/musefs.py:63)).

**Recommendation:** Keep Beets and other integrations on the scan path for
structural rows. Before blessing any direct structural writer, add contract tests
or migration checks for offset/length/format validity.

#### Beets prune is whole-DB rather than sync-scope limited

**Risk:** `prune_missing` can remove any `tracks` row whose backing path is
currently absent, even if it is unrelated to the current Beets command or import
hook. This preserves original files, but can remove virtual-library rows for
temporarily unavailable storage.

**Evidence:** Beets calls prune after command sync and passive reconcile
([contrib/beets/beetsplug/musefs.py](/home/cfutro/git/musefs/contrib/beets/beetsplug/musefs.py:72),
[contrib/beets/beetsplug/musefs.py](/home/cfutro/git/musefs/contrib/beets/beetsplug/musefs.py:103)).
`prune_missing` deletes every missing backing path
([contrib/beets/beetsplug/_core.py](/home/cfutro/git/musefs/contrib/beets/beetsplug/_core.py:147)).

**Recommendation:** Add a regression test for a scoped Beets sync with an
unrelated missing row. Then either document whole-DB prune as intentional or
constrain pruning to the sync/scan scope.

#### Beets is shipped but not visible in CI

**Risk:** The README and roadmap treat Beets as a shipped integration and public
SQLite writer, but CI does not appear to run the Python plugin tests. Drift in
this path can break the external-writer contract without Rust CI noticing.

**Evidence:** CI runs Rust checks, interop, and real FUSE tests
([.github/workflows/ci.yml](/home/cfutro/git/musefs/.github/workflows/ci.yml:16),
[.github/workflows/ci.yml](/home/cfutro/git/musefs/.github/workflows/ci.yml:35),
[.github/workflows/ci.yml](/home/cfutro/git/musefs/.github/workflows/ci.yml:54)).
The public docs advertise Beets as delivered
([docs/ROADMAP.md](/home/cfutro/git/musefs/docs/ROADMAP.md:32)).

**Recommendation:** Add a narrow Beets CI job or a documented manual release
gate that exercises sync, retag, prune, and art writes against the SQLite
contract.

#### Mutagen interop does not verify byte preservation

**Risk:** The independent-reader test proves synthesized tags are readable, but
does not compare the emitted output's audio payload to the source. A public
compatibility test should also protect the core invariant.

**Evidence:** The emitter writes source and synthesized files
([musefs-core/tests/interop_emit.rs](/home/cfutro/git/musefs/musefs-core/tests/interop_emit.rs:119),
[musefs-core/tests/interop_emit.rs](/home/cfutro/git/musefs/musefs-core/tests/interop_emit.rs:140)).
The Python test asserts tag visibility
([tests/interop/test_mutagen_roundtrip.py](/home/cfutro/git/musefs/tests/interop/test_mutagen_roundtrip.py:57)).

**Recommendation:** Extend the interop manifest with enough per-format audio
bounds or payload metadata to compare source and synthesized audio bytes.

### Low Priority / Future Refactors

#### `RegionLayout` permits invalid layouts by construction

**Risk:** `RegionLayout::new` accepts any segment vector, so ordering and
coverage invariants are enforced by producers and tests rather than by the type.

**Evidence:** The segment model is public and `RegionLayout::new` only stores
the vector ([musefs-format/src/layout.rs](/home/cfutro/git/musefs/musefs-format/src/layout.rs:50)).

**Recommendation:** Add a lightweight validated constructor or shared debug
assertion at synthesis boundaries. Keep the existing simple representation.

#### Ogg's invariant should be named differently from backing-audio passthrough

**Risk:** `OggAudio` preserves packet payload bytes but patches page sequence
numbers and CRCs. Treating it as identical to `BackingAudio` in documentation or
tests can obscure where mutation is intentional.

**Evidence:** `OggAudio` carries a `seq_delta`
([musefs-format/src/layout.rs](/home/cfutro/git/musefs/musefs-format/src/layout.rs:10)).
The shared fuzz property treats `OggAudio` as contiguous backing coverage
([musefs-format/src/fuzz_check.rs](/home/cfutro/git/musefs/musefs-format/src/fuzz_check.rs:21)).

**Recommendation:** Document and test the narrower Ogg invariant: original packet
payload bytes are preserved, while page headers may be patched.

#### MP4 silently uses only the first art input

**Risk:** MP4 behavior diverges from formats that iterate all art inputs. This is
not an audio-byte risk, but it is a contract clarity issue for external writers.

**Evidence:** MP4 passes `arts.first()` into `build_udta`
([musefs-format/src/mp4.rs](/home/cfutro/git/musefs/musefs-format/src/mp4.rs:630)).

**Recommendation:** Document single-cover MP4 behavior or reject multiple MP4 art
inputs until multiple covers are intentionally supported.

#### CLI mount config mapping is hard to unit-test without mounting

**Risk:** CLI parsing is tested, but the conversion from parsed flags into
`MountConfig`/`FuseConfig` sits in `run_mount`, which then calls real FUSE.

**Evidence:** The config construction happens in `run_mount`
([musefs-cli/src/lib.rs](/home/cfutro/git/musefs/musefs-cli/src/lib.rs:131)).
Current CLI tests are parse-oriented
([musefs-cli/tests/cli.rs](/home/cfutro/git/musefs/musefs-cli/tests/cli.rs:61)).

**Recommendation:** Extract a tiny pure config builder or add a non-mounting test
seam for argument-to-config mapping.

## SQLite Contract and External Writers

### Strengths

- The DB contract keeps audio bytes out of SQLite. `tracks` stores backing path,
  format, audio bounds, size, and mtime; mutable metadata lives in `tags`, `art`,
  and `track_art`
  ([musefs-db/src/schema.rs](/home/cfutro/git/musefs/musefs-db/src/schema.rs:5)).
- `content_version` is DB-enforced through triggers for tag and art-link
  mutations, so external SQLite writers participate in cache invalidation without
  calling Rust APIs
  ([musefs-db/src/schema.rs](/home/cfutro/git/musefs/musefs-db/src/schema.rs:44),
  [musefs-db/src/schema.rs](/home/cfutro/git/musefs/musefs-db/src/schema.rs:60)).
- Beets runs `musefs scan` for structural ingestion and then writes only
  metadata/art tables, which is the right split for preserving audio-byte
  ownership
  ([contrib/beets/beetsplug/musefs.py](/home/cfutro/git/musefs/contrib/beets/beetsplug/musefs.py:63),
  [contrib/beets/beetsplug/_core.py](/home/cfutro/git/musefs/contrib/beets/beetsplug/_core.py:301)).
- Art is content-addressed and chunk-readable, matching the lazy read path
  ([musefs-db/src/art.rs](/home/cfutro/git/musefs/musefs-db/src/art.rs:17),
  [musefs-db/src/art.rs](/home/cfutro/git/musefs/musefs-db/src/art.rs:70)).

### Risks and Recommendations

The key risks in this area are contract strictness and integration coverage:
structural DB rows are permissive, Beets prune scope is broad, and Beets tests
should be part of the release gate. These do not undermine the current invariant
path, but they are likely future integration failure points.

## Format Synthesis and Layout

### Strengths

- `RegionLayout` makes byte ownership legible: `Inline` owns generated bytes,
  `ArtImage`/`OggArtSlice` reference art, and `BackingAudio`/`OggAudio` reference
  the backing file
  ([musefs-format/src/layout.rs](/home/cfutro/git/musefs/musefs-format/src/layout.rs:3)).
- FLAC, MP3, WAV, and MP4 all append original audio as a backing segment rather
  than rebuilding the payload
  ([musefs-format/src/flac.rs](/home/cfutro/git/musefs/musefs-format/src/flac.rs:176),
  [musefs-format/src/mp3.rs](/home/cfutro/git/musefs/musefs-format/src/mp3.rs:250),
  [musefs-format/src/wav.rs](/home/cfutro/git/musefs/musefs-format/src/wav.rs:211),
  [musefs-format/src/mp4.rs](/home/cfutro/git/musefs/musefs-format/src/mp4.rs:666)).
- MP4 has a strong bounded-memory boundary: `read_structure_from` skips `mdat`
  payload and reads structural boxes only
  ([musefs-format/src/mp4.rs](/home/cfutro/git/musefs/musefs-format/src/mp4.rs:237)).
- Shared fuzz/property checks protect contiguous backing coverage
  ([musefs-format/src/fuzz_check.rs](/home/cfutro/git/musefs/musefs-format/src/fuzz_check.rs:7)).

### Risks and Recommendations

The format layer is generally well aligned with the invariant. The most concrete
hardening item is the Ogg full-value length guard. The main maintainability items
are naming Ogg's patched-page invariant precisely, validating `RegionLayout`
producer assumptions, and documenting format-specific art behavior.

## Core Read Assembly and Refresh

### Strengths

- Scan stores backing identity and byte bounds while keeping tags/art separate
  ([musefs-core/src/scan.rs](/home/cfutro/git/musefs/musefs-core/src/scan.rs:137),
  [musefs-core/src/scan.rs](/home/cfutro/git/musefs/musefs-core/src/scan.rs:156)).
- `revalidate` preserves external metadata edits when backing size/mtime are
  unchanged, which supports SQLite as metadata authority
  ([musefs-core/src/scan.rs](/home/cfutro/git/musefs/musefs-core/src/scan.rs:220)).
- `HeaderCache::resolve` self-invalidates by `content_version` and validates
  backing size/mtime before cache hits
  ([musefs-core/src/reader.rs](/home/cfutro/git/musefs/musefs-core/src/reader.rs:190)).
- Refresh has good architecture: `data_version` polling, single-flight rebuilds,
  cache pruning, and stable path-keyed inodes
  ([musefs-core/src/facade.rs](/home/cfutro/git/musefs/musefs-core/src/facade.rs:213),
  [musefs-core/src/tree.rs](/home/cfutro/git/musefs/musefs-core/src/tree.rs:3)).

### Risks and Recommendations

Core owns the highest-risk assumptions: DB pool identity, fd validation, changed
inode reporting, refresh retry semantics, and memory accounting for Ogg indexes.
These are focused enough to address without changing crate boundaries.

## FUSE and CLI Boundaries

### Strengths

- FUSE is thin: lookup, attrs, open, read, and directory entries delegate to
  `musefs-core` rather than duplicating synthesis logic
  ([musefs-fuse/src/lib.rs](/home/cfutro/git/musefs/musefs-fuse/src/lib.rs:185),
  [musefs-fuse/src/lib.rs](/home/cfutro/git/musefs/musefs-fuse/src/lib.rs:237)).
- The read-only boundary is explicit through mount options and attrs
  ([musefs-fuse/src/lib.rs](/home/cfutro/git/musefs/musefs-fuse/src/lib.rs:82),
  [musefs-fuse/src/lib.rs](/home/cfutro/git/musefs/musefs-fuse/src/lib.rs:301)).
- Blocking work is intentionally offloaded from the dispatch path
  ([musefs-fuse/src/lib.rs](/home/cfutro/git/musefs/musefs-fuse/src/lib.rs:146)).
- CLI parses inputs, opens the DB, constructs configs, and delegates
  ([musefs-cli/src/lib.rs](/home/cfutro/git/musefs/musefs-cli/src/lib.rs:95),
  [musefs-cli/src/main.rs](/home/cfutro/git/musefs/musefs-cli/src/main.rs:4)).

### Risks and Recommendations

FUSE should keep its current thin shape. The main improvements are visibility for
refresh/invalidation failures and a small CLI config test seam.

## Verification and Documentation Coverage

### Strengths

- CI runs Rust formatting, Clippy, workspace tests, format property tests,
  mutagen interop, and real FUSE tests
  ([.github/workflows/ci.yml](/home/cfutro/git/musefs/.github/workflows/ci.yml:26),
  [.github/workflows/ci.yml](/home/cfutro/git/musefs/.github/workflows/ci.yml:35),
  [.github/workflows/ci.yml](/home/cfutro/git/musefs/.github/workflows/ci.yml:54)).
- Fuzzing covers all supported format parsers plus byte-level primitives, with PR
  smoke and scheduled corpus runs
  ([.github/workflows/fuzz.yml](/home/cfutro/git/musefs/.github/workflows/fuzz.yml:21),
  [fuzz/Cargo.toml](/home/cfutro/git/musefs/fuzz/Cargo.toml:26)).
- Public docs repeat the central invariant clearly
  ([README.md](/home/cfutro/git/musefs/README.md:5),
  [docs/ROADMAP.md](/home/cfutro/git/musefs/docs/ROADMAP.md:5)).

### Risks and Recommendations

The biggest verification gap is not Rust-side coverage; it is the shipped Beets
integration and the fact that interop verifies tag readability but not audio
payload preservation. Adding those checks would make the release gate align more
closely with the invariant route.

## Strengths to Preserve

- Keep the crate layering strict: DB and format remain independent lower layers,
  core integrates, and FUSE/CLI stay adapters.
- Keep SQLite as the external writer contract, but route structural file facts
  through scan.
- Preserve `RegionLayout` as the central byte-ownership vocabulary.
- Preserve lazy art streaming and positioned backing reads as the default read
  model.
- Keep property tests, fuzzing, mutagen interop, and real FUSE tests as separate
  verification layers; they catch different failure modes.

## Recommended Follow-Up Plan

1. Fix or explicitly enforce the `DbPool` one-pool assumption.
2. Validate opened backing fds before storing per-handle reads.
3. Tighten keep-cache invalidation semantics for path-changing retags.
4. Add visibility for FUSE refresh/invalidation failures.
5. Harden Ogg full comment-value length checks and clarify the patched-page
   invariant.
6. Add Beets CI or a release gate for the external-writer contract.
7. Extend mutagen interop to verify audio payload preservation, not only tag
   readability.
