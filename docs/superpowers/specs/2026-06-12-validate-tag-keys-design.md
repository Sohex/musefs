# Validate user-defined tag keys before Vorbis synthesis

Design for [issue #300](https://github.com/Sohex/musefs/issues/300).

## Problem

FLAC and Ogg synthesis write user-defined `tags.key` values directly into
Vorbis comment field-name position with no validation. A key containing `=`
shifts the key/value boundary on a synthesize → scan round-trip: a stored key
`a=b` with value `c` synthesizes the comment `A=B=c`, which a later musefs scan
parses (via `split_once('=')`) as key `A`, value `B=c`. Empty keys and
control-character keys are likewise accepted into field-name position, producing
non-portable Vorbis comments. This contradicts the documented FLAC/Ogg promise
that user-defined fields round-trip by name.

The `=`/grammar problem is **specific to Vorbis**: musefs serves each track in
its backing format (no cross-format conversion), and MP3 (`TXXX`) / M4A
(freeform) custom keys legitimately contain `=`, `:`, and spaces. The scanner
already writes such keys for those formats. So the strict rule must apply only
on the FLAC/Ogg synthesis path, while a weaker universal-hygiene floor (no
empty, no control-char keys) is safe for every format.

## Grammar

`is_valid_key` enforces the **strict Vorbis spec** for comment field names: one
or more characters in ASCII `0x20`–`0x7D`, excluding `0x3D` (`=`). This rejects
empty keys, control characters (`< 0x20`, `0x7F`), `=`, and all non-ASCII /
high bytes.

Strict (rather than lenient UTF-8-tolerant) matches the libraries musefs
interoperates with: **mutagen** (which Picard is built on — directly in the
contrib path) validates keys to exactly this range and raises on violations,
and **TagLib** applies a legal-character check on Xiph field names when writing.
A key those libraries would reject does not portably round-trip "by name"; it
only survives a musefs→musefs loop. Dropped keys are logged, so the loss is
never silent.

## Architecture: three coordinated layers

| Layer | Responsibility | Mechanism |
| --- | --- | --- |
| `musefs-db` | Universal hygiene floor for **all** formats | `CHECK` on `tags.key`: non-empty and no embedded control char |
| `musefs-format` | Owns the Vorbis grammar and self-guarantees well-formed output; stays pure (no logging) | `vorbiscomment::is_valid_key(&str) -> bool`; `build()` **defensively skips** keys failing `is_valid_key` (emitting only valid comments, count reflecting them) |
| `musefs-core` | Integration: observability | At the FLAC/Ogg dispatch, `log::warn!` each `inputs` key that fails `is_valid_key`, with its `track_id` |

The grammar *check* lives in `format` (the layer that knows Vorbis rules) and
`build()` is the single enforcement point — it is **total** for any caller,
including the fuzz harnesses (`fuzz_targets/{flac,ogg}.rs` feed it arbitrary
keys via `arb_tags`), so it must skip rather than assert. The *logging* lives in
`core` (which already depends on `log`); core observes the same predicate purely
to report which keys were dropped. The two are deliberately separate so the
format layer remains a pure leaf.

## Data flow

### Write path (into the DB)

- **External writers** (Picard, beets, …) hit the DB `CHECK` directly. An empty
  or control-char key is rejected with a constraint error at write time — the
  contract enforcement #300 asks for.
- **Scanner** stays robust against malformed backing files:
  - `vorbiscomment::parse` skips comments with an **empty field name** (a
    `=value` comment has no name and is already malformed), so the scanner never
    ingests an empty key from a real FLAC/Ogg. The first-`=` split is otherwise
    unchanged: `A=B=c` still parses as key `A`, value `B=c`.
  - The scanner's **two** tag-collection loops drop any remaining
    empty/control-char keys before `replace_tags`: `scan.rs:575` (iterates
    `probed.tags` by value) and `scan.rs:658` (iterates `&probed.tags` by
    reference). Both call one shared predicate — `key_passes_floor(&str) ->
    bool` (non-empty, no byte below 0x20) — which mirrors the DB `CHECK` exactly
    (DEL 0x7F and high/non-ASCII bytes pass both), so a scan never trips it. This
    is the **universal** floor, distinct from the strict
    `vorbiscomment::is_valid_key`: applying the Vorbis predicate here would
    wrongly drop legal MP3/M4A keys containing `=`/`:`/space.

The scanner *gracefully skips* malformed keys; external writers get a *hard
rejection*. Both honor the same floor, each in the appropriate register.

### Read / synthesis path (DB → served bytes)

- `inputs = tags_to_inputs(get_tags(...))` is built once during layout
  resolution (`reader.rs:172`), inside `HeaderCache::resolve` (`reader.rs:113`),
  which caches the resulting `Arc<ResolvedFile>` per track. It is rebuilt only on
  a cache miss/invalidation.
- In the FLAC branch (`reader.rs:209`) and the Ogg branch (`reader.rs:285`),
  core `log::warn!`s each `inputs` key that fails `is_valid_key`, with its
  `track_id`. Because this runs during resolution (not per `read_at`), the
  HeaderCache de-dupes the warning: a malformed key warns once per resolution,
  not on every served byte range.
- `inputs` is passed to `flac::synthesize_layout` / `ogg::synthesize_layout`
  unchanged; `vorbiscomment::build` performs the actual drop. Core does not
  pre-filter — `build` is the single enforcement point, so there is no second
  predicate to keep in sync with core's logging beyond `is_valid_key` itself.
- MP3 / M4A / WAV branches are untouched.

**Worked example** — stored key `a=b`, value `c`:

- FLAC/Ogg track → key dropped at synthesis and logged → no `A=B=c` boundary
  shift.
- MP3 track → preserved verbatim in `TXXX` (where `=` is legal).
- Empty / control-char keys → rejected at the DB for external writers, skipped
  by the scanner, and dropped at synthesis as defense in depth.

## The DB CHECK (no new migration)

There are no deployed databases, so a versioned migration is unnecessary. The
`tags` table's canonical definition lives in `MIGRATION_V4`
(`musefs-db/src/schema.rs:177`); `MIGRATION_V5` does not touch `tags`. The
`CHECK` is added **in place** to V4's `CREATE TABLE tags`:

```sql
CHECK (length(key) >= 1
       AND key NOT GLOB '*[' || char(1) || '-' || char(31) || ']*')
```

Ripples:

- The open-time schema verifier (`verify_schema`, `schema.rs:~401`) derives its
  expectation by replaying the migration chain on a reference DB, so editing V4
  auto-updates it — no snapshot to hand-maintain.
- `user_version` stays at 5 (no migration added), so the Picard
  `test_conftest_sanity.py` hardcoded version assertion is unaffected.
- **Python mirror — two files, two steps.** There are two mirrors: the canonical
  `contrib/python-musefs/src/musefs_common/schema.py` and the vendored Picard
  copy `contrib/picard/musefs/_common/schema.py`. The `schema_py_fixture_is_fresh`
  test (`schema.rs:~779`) fails the full-workspace pre-commit gate until the
  canonical mirror is regenerated. Both steps are required:
  1. `MUSEFS_REGEN_SCHEMA_PY=1 cargo test -p musefs-db schema_py` (rewrites the
     canonical mirror);
  2. `python contrib/python-musefs/vendor_to_picard.py` (copies it into Picard).
  Verify the Picard copy actually changed. Each mirror renders the **full
  migration chain**, so the `tags` CREATE appears **twice** (V1's original, then
  V4's recreated table); confirm the new `CHECK` lands in the **V4 rendering**
  (the second occurrence).
- **NUL-inside-key limitation (accepted).** `length()`/`GLOB` stop at an embedded
  NUL (`char(0)`), so a NUL *inside* a key evades the DB `CHECK`. On the
  FLAC/Ogg path `build`'s `is_valid_key` drops it (NUL `< 0x20`), but for
  **MP3/M4A external writers** there is no Rust backstop — such a key reaches
  `TXXX`/freeform synthesis. A Rust guard in `replace_tags` would **not** close
  this, because external writers write raw SQL and bypass `replace_tags`
  entirely; only the `CHECK` governs them. An embedded-NUL custom key from an
  external writer is pathological, so this gap is documented and tolerated rather
  than chased. A one-line note goes in the schema comment.
- Any migration-chain test that asserts V4's `tags` shape is updated.
- The DB floor also applies to **WAV-backed** keys (the `tags` table is
  format-agnostic): a WAV custom key with a control char is rejected at the DB.
  This is the intended floor behavior even though WAV *grammar* is out of scope.

## Testing

- **`vorbiscomment` regression** (FLAC *and* Ogg): keys `a=b`, `""`,
  `"\n"`/control char, and a normal custom key (`custom_thing`). Assert valid
  keys survive in field-name position, invalid keys are dropped, and a
  synthesize → parse round-trip preserves the key/value boundary (no `A=B=c`).
- **Order preservation:** inputs `[valid1, a=b, valid2]` synthesize to
  `valid1, valid2` in order, with values intact and the count reflecting only the
  surviving tags (FLAC and Ogg). `get_tags` returns `ORDER BY key, ordinal`, so
  dropping a key must not reorder or renumber the survivors.
- **`is_valid_key` unit table:** boundary bytes `0x1F`/`0x20`/`0x3D`/`0x7D`/
  `0x7E`, empty string, a non-ASCII key.
- **`build` totality:** `build` over arbitrary keys (including `a=b`, empty,
  control char) never panics and emits only valid comments — the totality the
  fuzz harness relies on (`build` has no assert guarding key validity).
- **DB:** an external `replace_tags` with a control-char key (and with an empty
  key) returns a constraint error and rolls back the whole transaction (inserts
  are row-by-row, `tags.rs:177`) — documenting *why* the scanner floor exists; a
  valid custom key inserts successfully.
- **`parse`:** a `=value` comment yields no tag; a `A=B=c` comment still yields
  key `A`, value `B=c` (first-`=` split preserved).
- **Scanner:** a probed key that violates the floor is skipped, not aborted, and
  the track's remaining tags persist — for **both** collection loops
  (`scan.rs:575` and `:658`).
- **Fuzz:** `cargo +nightly fuzz build` (the `fuzz/` crate is outside the
  workspace; the touched `vorbiscomment`/`flac`/`ogg` targets feed `arb_tags`
  and would break silently otherwise).

## Documentation

- `docs/FLAC.md` and `docs/OGG.md`: note that out-of-grammar user-defined keys
  are dropped (with the strict Vorbis range) on synthesis.
- The external-writer contract section in `ARCHITECTURE.md`: document the
  `tags.key` floor (non-empty, no control chars) that writers must satisfy.

## Out of scope

- WAV `INFO` 4-byte key grammar (separate format, not implicated by #300).
- Validating ID3 / M4A key grammar (their custom-key rules differ and are not
  the boundary-shift vector).
