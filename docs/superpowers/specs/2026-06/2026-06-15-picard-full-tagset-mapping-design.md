# Picard plugin: full tag-set mapping (issue #424)

## Problem

The Picard plugin (`contrib/picard/musefs/_core.py`) maps a fixed ~20-field
whitelist (`DIRECT_FIELDS`). Among MusicBrainz identifiers it forwards only
`musicbrainz_albumid` and `musicbrainz_artistid`, dropping recording / track /
release-group / work / album-artist IDs. It also omits sort fields
(`artistsort`, `albumartistsort`, …), performer/credit fields, movement, totals,
and anything else Picard sets. A musefs view produced by Picard therefore loses
much of the metadata Picard itself generates.

The beets sibling (`contrib/beets/beetsplug/_core.py`) already maps the host's
full tag set dynamically. This change brings the Picard plugin to the same level
of coverage.

## Goal

Replace the static `DIRECT_FIELDS` whitelist with dynamic enumeration of
Picard's entire metadata, so every tag Picard sets reaches the store under the
correct **canonical store key** — the on-disk tag name a real tagged file
carries.

## Key principle: store keys are on-disk tag names

musefs's Rust format layer canonicalizes only the ~22 keys in
`musefs-format/src/tagmap.rs::VOCAB`; every other key round-trips **verbatim**
through each format's extension slot, i.e. the store key becomes the synthesized
file's field name directly. So for non-VOCAB fields the store key must be the
**standard on-disk tag name**.

That standard is what *both* taggers write to real files:

- Picard writes sort tags as `ARTISTSORT` / `ALBUMARTISTSORT` and compilation as
  `COMPILATION` (Picard's `vorbis.py` passes them through verbatim; not in its
  `__translate` table).
- beets writes the *same* on-disk tags: MediaFile maps its internal attributes
  `artist_sort → ARTISTSORT`, `albumartist_sort → ALBUMARTISTSORT`,
  `comp → COMPILATION` (`mediafile/__init__.py`).

The beets *plugin* currently emits its host's **internal attribute** spellings
(`artist_sort`, `albumartist_sort`, `comp`) as store keys, which diverge from the
on-disk standard. **This work fixes both siblings**: the Picard plugin emits the
standard on-disk names from the start (§1–§5), and the beets plugin is realigned
to the same standard (§6), so the two converge on one canonical key set.

The MusicBrainz ID swap is the same in both Picard's and beets' on-disk output,
so the two siblings agree there once this change lands.

## Design

### 1. Enumeration

`map_fields(metadata, extra_fields=None)` iterates `metadata.rawitems()` (real
Picard returns `(name, [values])` pairs). For each `(name, values)`:

- **Skip** if `name.startswith("~")` — Picard's hidden/internal vars (`~length`,
  `~rating`, file facts). Picard has no `_media_tag_fields`-style boundary like
  beets; the `~` prefix is the *only* gate, and it is sufficient: every Picard
  file-fact / derived var is `~`-prefixed, and all enumerable non-`~` keys are
  genuine tags (`picard/util/tags.py::TAG_NAMES`) or user-defined tags that
  should round-trip. A test enumerates known `~` vars to lock this in (§Tests).
- Derive the output key from `name` (§2).
- **Expand every value** to its own `(key, value)` row — the store has set
  semantics, matching beets. This **retires `_MULTI_VALUE_KEYS`**: *all* fields
  multi-value-expand, not just a hardcoded allowlist of four.
- Drop empty / whitespace-only values.
- Apply numeric drop-if-zero (§3).

**Collision model: accumulate, do not first-writer-win.** The `emitted` dict
maps output key → list of values, and each source **extends** (appends to) its
output key's list. This differs deliberately from the beets sibling's
first-writer-wins rule, which exists there only to let a *plural twin* claim its
singular key. Picard needs accumulation instead because multiple distinct source
keys legitimately collapse to one output key — chiefly per-role performers
(`performer:Piano`, `performer:Guitar` → several `performer` rows) and
multi-description comments (`comment:eng`, `comment:deu`). First-writer-wins
would silently drop the later roles/comments. After the §2 RENAME swaps, no two
*non-colon* source vars map to the same output key (the MB-ID and movement
renames are clean bijective swaps), so accumulation never produces spurious
duplicates for ordinary fields.

**No twin-collapse (unlike beets).** Picard exposes `artist` (the credited
join-phrase string, e.g. `"Alice & Bob"`) and `artists` (the individual-name
list) as **distinct on-disk tags** (`ARTIST` vs `ARTISTS`; same for
`albumartist`/`albumartists`). Picard writes both to a file, so — under the
on-disk-fidelity principle — the plugin emits both as separate keys (`artist`,
`artists`, `albumartist`, `albumartists`) rather than collapsing the plural into
the singular. `artists`/`albumartists` are not in VOCAB and round-trip verbatim
(`ARTISTS` in Vorbis, `TXXX:ARTISTS` in ID3 — matching Picard). This is an
*intentional, acceptable* representation divergence from the beets sibling (which
collapses `artists → artist`); both produce valid standard tags, just structured
per host. It is **not** in scope to change beets' twin handling.

### 2. Output key derivation (`_output_key`)

Applies Picard's own universal var→tag conventions (the format-agnostic subset
of Picard's `vorbis.py` `__rtranslate` plus the colon-suffix handling):

**RENAME map** (Picard var → canonical store key):

```
musicbrainz_recordingid -> musicbrainz_trackid
musicbrainz_trackid     -> musicbrainz_releasetrackid
movementnumber          -> movement
movement                -> movementname
totaltracks             -> tracktotal
totaldiscs              -> disctotal
```

**Colon-suffixed keys.** Picard's `Metadata.normalize_tag()` is
`name.rstrip(':')` (`/usr/lib/picard/picard/metadata.py:459`), so the keys that
actually arrive via `rawitems()` are **either bare** (`comment`, `lyrics`,
`performer` — the empty-description case) **or carry a non-empty description**
(`comment:eng`, `performer:Piano`). A trailing-colon-only key (`comment:`) is
*never* yielded; the spec and tests must use the post-normalize forms. Detect a
description by splitting on the first `:` and checking the suffix is non-empty:

- `performer:<role>` → key `performer`, value folded to Picard's own form:
  `"<value> (<role>)"`. Bare `performer` → key `performer`, value verbatim. (Per
  the settled decision: preserve the role by folding it into the value, exactly
  how Picard renders performers to a file.) Each value in the list folds
  independently; multiple roles **accumulate** under `performer` (per §1).
- `comment:<desc>` → key `comment`, value **verbatim** (description dropped).
  Bare `comment` → key `comment`, value verbatim.
- `lyrics:<desc>` → key `lyrics`, value **verbatim**. Bare `lyrics` → `lyrics`.

  Rationale for *collapse, don't fold* on comment/lyrics: folding a description
  into the value (`"text (eng)"`) is a Vorbis-specific rendering that would
  corrupt the structured ID3 `COMM` / MP4 `©cmt` frames the format layer
  produces. The description is empty in the overwhelmingly common case (the key
  arrives bare), so collapsing to the VOCAB-canonical base key loses nothing in
  practice while keeping comments correct across all formats. Multiple
  descriptions accumulate as separate `comment`/`lyrics` rows (per §1).

**Everything else**: the name lowercased verbatim (Picard names are already
lowercase) — so `artistsort`, `albumartistsort`, `albumsort`, `composersort`,
`titlesort`, `compilation`, `isrc`, `label`, `barcode`, `acoustid_id`, all other
`musicbrainz_*` IDs, etc. pass straight through with their standard on-disk
names.

### 3. Numeric drop-if-zero

For these **output** keys, drop any value that parses to numeric zero (a
float-aware check, so both `"0"` and `"0.0"` drop), so Picard's "0"
placeholders don't leak as tags:

```
{tracknumber, discnumber, tracktotal, disctotal, compilation, bpm}
```

Picard stores track/disc numbers and totals as **scalar** strings (it splits the
combined `"1/12"` form into separate `tracknumber`/`totaltracks` vars), so
`_to_int` never sees a slash form here and is safe on these keys.

### 4. `extra_fields` override

Unchanged in spirit: the options-page field map remains a final
`picard_field → store_key` override layer, last-wins. Implemented by reading
`metadata.getall(picard_field)` for each mapping and **replacing**
`emitted[store_key]` with those (non-empty) values, overriding any natural
mapping. (`getall` is retained on `metadata` for this path; it applies
`normalize_tag`, so a source field written with a trailing colon resolves to its
normalized key.)

Override values are taken **verbatim**: the override path does *not* apply the
performer/comment colon-fold, the §3 numeric drop-if-zero, or the §2 RENAME — the
user has explicitly named both the source field and the target store key, so the
plugin honors that mapping literally. (Consequence: mapping a source like
`performer:Piano` yields the unfolded player name under the chosen key; documented
so it is not mistaken for a bug.)

### 5. Functions removed / changed

- `DIRECT_FIELDS`, `_MULTI_VALUE_KEYS` — removed (superseded by enumeration).
- `_first_value` — removed (no longer single-value-per-field).
- `_values` — removed; value iteration/stripping is inlined into the enumeration
  loop (every key multi-value-expands now, so the per-field helper is redundant).
- `_NUMERIC_KEYS` → replaced by the §3 drop-if-zero set keyed on output keys.
- New: `RENAME` map, `_output_key` (RENAME + colon split), and a small
  performer value-fold helper.

### 6. beets sibling alignment (in scope)

`contrib/beets/beetsplug/_core.py` — realign the three outlier output keys to the
on-disk standard so both plugins emit identical store keys:

- Add to `RENAME` (applied by `_output_key` after TWINS collapse):
  ```
  artist_sort      -> artistsort
  albumartist_sort -> albumartistsort
  comp             -> compilation
  ```
  This also covers the plural twins automatically: `artists_sort →
  artist_sort → artistsort`, `albumartists_sort → albumartist_sort →
  albumartistsort` (TWINS collapse runs first in `_output_key`).
- Update `_DROP_IF_ZERO`: replace `comp` with `compilation` (the set is keyed on
  the **output** key, which is now `compilation`). `tracktotal` / `disctotal`
  already match the standard and stay.
- The `artist_credit` / `albumartist_credit` keys are left unchanged: Picard has
  no credit field, so there is no cross-sibling conflict and no established
  on-disk standard to realign to.

**Convergence audit (verified):** beyond the three keys above, the siblings
already agree on every shared field — track/disc numbers and totals, ReplayGain,
the MB-ID swap, `comments → comment`, and the artist/albumartist/genre/composer
families. beets has no per-role performer concept and no `lyrics:<desc>` form, so
the §2 colon-collapse introduces no new divergence. The one accepted,
intentional difference is artist/artists representation (§1, "No twin-collapse"):
Picard emits both `artist` and `artists`; beets collapses to `artist`. Both are
valid standard tags; reconciling that is explicitly out of scope.

**Migration note:** existing stores written by the old beets plugin hold the old
keys (`comp`, `artist_sort`, `albumartist_sort`), recorded in each item's
accumulating `musefs_managed` flexattr. After this change the plugin emits the new
keys, and `build_records` computes `delete_keys = prev - keys_now`
(`contrib/beets/beetsplug/_core.py`), so on the next sync the old keys are placed
in `delete_keys` and cleared by `merge_tags`. The convergence is therefore clean
by design — no stale duplicates — with the old keys lingering only as harmless
re-deleted tombstones in the managed set (until `restore_backing` forgets them).
No data loss; the realigned values reappear under the new keys in the same sync.

## Tests

`contrib/picard/tests/`:

- **`conftest.py`**: extend `FakeMetadata` to be enumerable — add
  `rawitems()` returning `self._tags.items()` (it already stores
  `{key: [values]}`); keep `getall` for the `extra_fields` path. **Note:** the
  stub stores keys verbatim and does *not* replicate Picard's
  `normalize_tag` trailing-colon stripping — so test authors must write the
  post-normalize key form (`comment`, `performer`), never `comment:`, to match
  what real Picard yields.
- **`test_map_fields.py`**: rework existing assertions to the new dynamic shape
  and add coverage for:
  - MB ID swap: `musicbrainz_recordingid → musicbrainz_trackid`,
    `musicbrainz_trackid → musicbrainz_releasetrackid`, and the unchanged
    `musicbrainz_albumid/artistid/albumartistid/releasegroupid/workid`.
  - **MB IDs both present together:** with `musicbrainz_recordingid` *and*
    `musicbrainz_trackid` both set on one metadata, assert the output has both
    `musicbrainz_trackid` (from recording) and `musicbrainz_releasetrackid`
    (from track), distinct and not clobbered — guards the swap ordering.
  - Sort fields pass through (`artistsort`, `albumartistsort`).
  - **artist/artists both emitted:** `artist="Alice & Bob"`, `artists=["Alice","Bob"]`
    → key `artist` = `"Alice & Bob"` and key `artists` = `["Alice","Bob"]`, kept
    separate (no twin-collapse). Same for `albumartist`/`albumartists`.
  - Performer role-fold: `performer:Piano = "Joe Barr"` →
    `("performer", "Joe Barr (Piano)")`; bare `performer` → value only.
  - **Performer multi-role accumulation:** `performer:Piano=["Joe Barr"]` and
    `performer:Guitar=["Ann Lee"]` both present → two `performer` rows
    (`"Joe Barr (Piano)"`, `"Ann Lee (Guitar)"`), neither dropped; and a single
    role with two players folds each independently.
  - Comment/lyrics collapse: bare `comment` and `comment:eng` both → key
    `comment`, value verbatim (description dropped); multiple descriptions
    accumulate as separate rows; same for `lyrics`.
  - Totals rename + zero-drop: `totaltracks → tracktotal`, etc.
  - Arbitrary field multi-value expansion (e.g. two `mood` values).
  - `~`-prefixed keys skipped (e.g. `~length`, `~rating`) — and a positive case
    asserting non-`~` tags alongside them still emit, confirming `~`-prefix is
    the correct, sufficient gate.
  - `compilation` / `bpm` zero-drop.
  - **`extra_fields` semantics:** override replaces the natural mapping
    (last-wins) and takes the value **verbatim** — no colon-fold, no zero-drop
    (e.g. mapping a `performer:*` source yields the unfolded player name).
- **`test_contract.py`**: the shared `musefs_common/contract.py` contract
  (title/artist/albumartist/album/genre/composer only) must keep passing
  unchanged — a guard that the rework preserves the shared behavior.

`contrib/beets/tests/`:

- **`test_map_fields.py`**: update assertions that reference the realigned keys —
  `test_comp_one_kept_zero_dropped` now asserts the `compilation` output key
  (`comp=True → ("compilation", "1")`, zero dropped); add a sort-rename test
  asserting `artist_sort → artistsort` and `albumartist_sort → albumartistsort`.
- Grep the rest of `contrib/beets/tests/` (e.g. `test_build_records.py`,
  `test_reconcile.py`, `test_managed_state.py`, `test_contract.py`) for the
  literals `"comp"`, `"artist_sort"`, `"albumartist_sort"` and update any that
  assert the old output keys. The shared contract test is unaffected (it doesn't
  cover these fields).

Run the real-Picard contrib tests before pushing (they silently skip without an
importable Picard): `/usr/bin/python3` with `PYTHONPATH` to `/usr/lib/picard`
and dist-packages PyQt5 (see CONTRIBUTING.md / project memory).

## Documentation

- Check `contrib/picard/README.md` first: if it documents the fixed field list,
  replace that with the dynamic full-tag-set mapping (the `~`-skip, performer
  role-fold, comment/lyrics collapse, artist/artists both emitted, and the
  options-page `fields:` override). If it doesn't enumerate fields, the doc edit
  is smaller — just note the broadened coverage.
- Check `contrib/beets/README.md` for any documented store-key spellings
  (`comp`, `artist_sort`) and update them to the realigned standard names.

## Out of scope / follow-up

- Picard format-peculiar value rewrites that are *not* universal (e.g.
  `musicip_fingerprint → fingerprint` with a value prefix, Vorbis
  `sanitize_date`) are intentionally **not** replicated: the store is
  format-agnostic and the Rust layer handles per-format rendering. Such fields
  pass through verbatim.
