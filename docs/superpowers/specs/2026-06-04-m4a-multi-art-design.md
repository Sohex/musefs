# M4A multi-art design

2026-06-04

## Problem

Multiple embedded pictures are supported by every format musefs serves, and by
the store: `track_art` is keyed `(track_id, ordinal)` with a `picture_type`
column, scan persists every extracted picture, and
`mapping.rs::track_art_to_inputs` returns them all. FLAC, MP3, WAV, and Ogg
synthesis all emit one picture per art row. M4A is the lone gap, in both
directions:

- **Synthesis** (`musefs-format/src/mp4.rs`, `synthesize_layout`):
  `arts.iter().find(|a| a.data_len > 0)` emits only the first non-empty art;
  the rest are silently dropped.
- **Scan** (`mp4.rs`, `read_pictures`): per `covr` atom, only the first `data`
  child is read. The iTunes convention for multiple artworks is multiple
  `data` sub-atoms inside one `covr` atom, so extras are missed at ingest.

Separately, the contrib plugins only sync a single front cover into the DB:
`Record.art` in `musefs_common/sync.py` is one `(bytes, mime)` tuple,
`store.replace_track_art` writes one row, and Picard's `front_cover` picks the
first front image even though Picard metadata can hold many.

## Decisions

- **Scope:** both MP4 directions (scan and synthesis) plus plugin multi-art
  sync (shared lib, Picard, beets).
- **Type policy:** synthesis emits every non-empty art row as a `covr` `data`
  atom in `track_art` ordinal order. Picture type and description are silently
  dropped — `covr` has no fields for them (format-inherent; scan already
  stamps MP4 pictures as type 3).
- **Wire format:** one `covr` atom containing N `data` sub-atoms (the
  iTunes/mutagen convention). Multiple `covr` atoms were rejected: nonstandard,
  and mutagen keys its tag dict by atom name so a second `covr` collides.

## Rust format layer (`musefs-format/src/mp4.rs` only)

### Scan — `read_pictures`

For each `covr` atom in the `ilst`, iterate **all** `data` children via
`child_boxes` instead of `find_box`-ing the first. Per-atom rules are
unchanged: type code 13 → `image/jpeg`, 14 → `image/png`, anything else
skipped; `picture_type: 3`; empty description. Pictures accumulate in file
order across all `covr` atoms (multiple `covr`s remain tolerated). The
function stays lenient: a malformed `data` child is skipped without losing its
siblings.

No change in `musefs-core/src/scan.rs` — it already stores every
`EmbeddedPicture` with its ordinal, and `upsert_art` dedupes by sha256.

### Synthesis — `build_udta` + `synthesize_layout`

`build_udta` changes signature from `art: Option<&ArtInput>` to
`arts: &[ArtInput]` and emits one `covr` atom with one `data` sub-atom per
art, in slice order:

- `covr_size = 8 + Σ (16 + data_len_i)` — covr header plus each data atom's
  `[size]["data"][type][locale]` framing plus image bytes.
- Per art: append the 16-byte data-atom framing to `ilst_inline`, flush as
  `Segment::Inline`, push `Segment::ArtImage { art_id, len }`, and add
  `data_len` to `streamed_total` — the same flush pattern the binary-tag loop
  already uses.

In `synthesize_layout`, the `.find()` becomes a filter: every art with
`data_len > 0` passes through (zero-byte art is still excluded — an empty
`ArtImage` segment fails layout validation). Input order is `track_art`
ordinal order from `mapping.rs`.

No changes to `musefs-core` reader/mapping, no schema change, no new error
variants. The existing `new_moov_size > u32::MAX` guard covers the larger
`covr`.

## Python side (`contrib/`)

### Shared lib (`contrib/python-musefs/`)

- `sync.py`: `Record.art` becomes a list of `ArtImage`s — a frozen dataclass
  `ArtImage(data: bytes, mime: str, picture_type: int = 3,
  description: str = "")` defined in `sync.py`. `None` and the empty list both
  mean "no art from the host" (existing scan-seeded rows left untouched).
- `sync_one` over-cap rule: filter images individually against
  `MAX_ART_BYTES`; each over-cap image bumps `skipped_art` (now an **image**
  count). If at least one image survives, `replace_track_art` writes the
  survivors; if images were provided but all are over cap, leave existing
  `track_art` untouched (same don't-clobber semantics as today). `art_linked`
  stays a **track** count.
- `store.py`: `replace_track_art(conn, track_id, arts)` takes a list of
  `(art_id, picture_type, description)`; `DELETE` then `INSERT` one row per
  entry with `ordinal = index`. `upsert_art` is unchanged.

### Picard (`contrib/picard/musefs/_core.py`)

`front_cover(metadata)` is replaced by `images(metadata)` returning a list of
`ArtImage`s (imported from the vendored `musefs._common.sync`) for all
syncable images in Picard order. Duck-typed as today: iterate
`metadata.images`, skip images where
`getattr(img, "can_be_saved_to_tags", True)` is false. Map Picard's
`maintype` string to an ID3 picture type with a module-level table mirroring
Picard's own ID3 mapping — `front→3, back→4, booklet→5, medium→6`, everything
else → 0 (Other) — falling back to 3 when the image has no `maintype` but
`is_front_image()` is true. `comment`, when present, becomes `description`.
Re-vendor with `vendor_to_picard.py`; the drift-guard test enforces
freshness.

### beets (`contrib/beets/beetsplug/_core.py`)

Behavior unchanged in substance — an album has one `artpath`, so
`_read_album_art` wraps its single cover as `[ArtImage(data, mime, 3, "")]`.
The realpath cache and per-cover skip counting keep working; `skipped_art`'s
shift to image-count is a no-op here.

No schema or generated-`schema.py` change — `track_art` already supports all
of this.

## Testing

### Rust

- `mp4.rs` unit tests: `read_pictures` on a `covr` with two `data` atoms
  (mixed jpeg/png; a malformed or unknown-type-code child skipped without
  losing the rest); `synthesize_layout` with two arts → exactly two
  `ArtImage` segments in order with correct `covr`/`ilst`/`moov` size
  accounting; zero-byte art still filtered.
- Round-trip: synthesize with two arts, materialize the head, `read_pictures`
  it back → two pictures, same order and mimes.
- `proptest_mp4.rs`: replace the hardcoded empty `arts` with 0–3 generated
  arts, keeping the byte-identical-audio property.
- In-diff mutation gate (CI parity: `-j2`, output on /tmp, default TMPDIR,
  sanity-check the diff is non-empty first).
- Fuzz: public signatures don't change, but smoke-build
  `cargo +nightly fuzz build mp4` (the fuzz crate is outside the workspace);
  extend the mp4 seed in `generate_seeds` with a multi-`data` `covr`.

### Interop (Property 5)

Give `richer_m4a` in `musefs-core/tests/interop_emit.rs` a second cover (one
jpeg + one png); assert in `tests/interop/test_mutagen_roundtrip.py` that
mutagen sees `covr` as a 2-element list with the right `imageformat`s and
bytes.

### Python

- `python-musefs`: `replace_track_art` writes N rows with sequential ordinals
  and replaces prior rows; `sync_one` per-image cap semantics (one over + one
  under cap → survivor written, `skipped_art == 1`; all over cap → existing
  rows untouched).
- Picard: `images()` type-mapping table, `can_be_saved_to_tags` skip,
  `comment` → description, front fallback; re-vendor and let the drift-guard
  test confirm. Qt-fixture tests still skip without pytest-qt.
- beets: adapt art tests to the `ArtImage` list shape; run via the project
  venv.

Full verification: `cargo test` (workspace), `cargo fmt --all --check`,
`cargo clippy --all-targets`, the three Python suites, and the `--ignored`
interop emit + pytest pass per CLAUDE.md.

## Out of scope

- Picture types/descriptions in the M4A view (no `covr` representation
  exists).
- `covr` type codes beyond 13/14 (e.g. BMP) on scan or synthesis.
- Any schema change or `schema.py` regeneration.
