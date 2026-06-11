# Field-size caps + schema identity gate

**Issues:** #270, #267, #269, #278
**Date:** 2026-06-11
**Status:** Design approved, pending implementation plan

## Problem

A crafted or stale `--db` can smuggle under-constrained or oversized payloads
past the **read-only** serve path, which trusts the store as input. Two
independent holes:

1. **"Wrong shape."** `schema::migrate` fast-paths when `PRAGMA user_version >=
   latest` and `Db::open_readonly` validates nothing at all. A crafted DB can
   claim the latest version while omitting `CHECK` constraints or triggers, or
   hand-build tables with the expected names but weaker contracts (#270).
2. **"Right shape, poisoned row."** A `CHECK` constraint is enforced only at
   *write* time. A hostile author controls their own writing connection, so they
   can `PRAGMA ignore_check_constraints=ON`, insert an oversized row, and ship a
   file whose schema is byte-identical to the canonical latest. Reading it back
   never re-validates `CHECK`s. Several text/blob fields are then materialized in
   hot paths (template rendering, MP3/MP4/Vorbis/FLAC synthesis) before any
   format-level size check can reject them (#267, #269, #278).

These are the same "uncapped DB payload materialized before a size check" class.
The fields with no caps today: `tags.key`, `tags.value`,
`structural_blocks.body` (and its `kind`/`ordinal`), `art.mime`,
`track_art.description`, and — as defense-in-depth, since they stream rather than
fully materialize — `tags.value_blob` and `art.data`/`byte_len`.

## Threat model and scoping

- **No databases exist in the wild yet** (pre-release). This removes the need to
  support older-version read-only mounts, which lets #270 use the simplest strong
  invariant and lets the new caps fold into the existing V4 migration rather than
  appending a redundant V5.
- In scope: #270, #267, #269, #278.
- Out of scope: #265 (audio front/header reads) and #266 (Ogg art streaming) —
  same epic, different fields, handled separately.

## Architecture: two complementary layers

Neither defense is redundant; both ship.

- **#270 closes "wrong shape"** — a schema-identity gate at open rejects any DB
  whose schema is not byte-identical to the canonical latest.
- **The caps close "right shape, poisoned row"** — a `CHECK` (write-time, for
  honest writers) **plus** a fail-closed reader guard at materialization
  (read-time, for smuggled rows). The schema gate proves structure, not row
  contents, so the reader guard is load-bearing regardless.

## Section 1 — #270 schema identity gate (`musefs-db`)

New `schema::validate_identity(conn) -> Result<()>`:

- Build a **reference object set** once, cached in a `OnceLock`: open an
  in-memory `Connection`, run `migrate()`, read
  `SELECT type, name, sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'`
  into a `BTreeMap<(type, name), sql>`. This is self-maintaining — no constant to
  regenerate when the schema changes — and reuses the migration as the single
  definition of "correct."
- Read the same set from the opened `conn` and compare. On any missing, extra, or
  altered object, return `Error::SchemaMismatch` identifying the first offending
  object.
- Run `PRAGMA foreign_key_check`; any returned row → reject.

**Determinism of "first offending object."** Iterate the union of the reference
and `conn` keys in `(type, name)` `BTreeMap` order and report the first key that
differs, with a fixed precedence among the three difference classes at a tied key
(missing → extra → altered). Without a pinned order, two implementations (or a
refactor) report different "first" objects and the Section 6 assertion on the
named object goes flaky.

**`foreign_key_check` runs without `PRAGMA foreign_keys = ON`.** `open_readonly`
deliberately never enables FK *enforcement* (no writes are possible), but
`PRAGMA foreign_key_check` is a standalone integrity scan that returns violating
rows regardless of the enforcement pragma — this is a deliberate, non-obvious
reliance, not an oversight. Note its cost: with no table argument it scans every
FK in the DB at each open (a full pass per mount); acceptable for the gate, but
stated so it is a conscious choice.

**Byte-exact `sql` comparison is a migration-text-stability constraint.** SQLite
stores `CREATE` statements verbatim, so the in-memory reference (replayed
`migrate()`) and an honest same-version `conn` produce identical `sql` text and
match. The consequence: a *cosmetic* edit to a shipped migration string
(whitespace, comment, quoting) changes the stored `sql` and would reject DBs
written by a prior build. Under "reject-older" with no DBs in the wild this is
acceptable, but it makes migration text a compatibility surface — flag it so it
is a conscious constraint after release, not a surprise.

**Reference set is process-global and infallible-by-construction.** The
`OnceLock` assumes one canonical schema with no per-mount/feature-gated variation
(true today); the in-memory `migrate()` that builds it cannot fail on a fresh
connection, so the initializer surfaces any error rather than caching a partial
set. Both assumptions are stated so the caching is provably safe.

**Call sites:**

- `Db::open_readonly` (`musefs-db/src/lib.rs`) — the load-bearing case; today it
  opens the connection and validates nothing. Add the gate immediately after
  open.
- `Db::<ReadWrite>::configure` (`musefs-db/src/lib.rs`) — after `migrate()`, as a
  cheap post-condition assertion (catches corruption or an in-place-modified
  schema).

**Policy: reject older/mismatched.** The read-only mount accepts exactly one
schema — the canonical latest — and rejects everything else. A developer's
pre-cap V4 DB will fail the gate and must be regenerated by a scan; this is the
intended dogfood, acceptable because no real DBs exist.

**Error messaging (required).** `Error::SchemaMismatch` must state both the
problem and the remedy in plain language: that the store's schema does not match
the version musefs expects (a schema version / identity mismatch), and that the
fix is to regenerate it by running `musefs scan` against the backing library.
The first differing object name is included for diagnosis but the user-facing
sentence leads with problem + fix, not the raw object diff.

## Section 2 — caps folded into V4 (`musefs-db/src/schema.rs`)

Extend `MIGRATION_V4` (the existing constraint migration) with:

| Table.field | New `CHECK` | Value |
| --- | --- | --- |
| `tags.key` | `length(key) <= 256` | 256 chars |
| `tags.value` | `length(value) <= 262144` | 256 KiB (262144 bytes) |
| `tags.value_blob` | `value_blob IS NULL OR length(value_blob) <= 16711680` | `MAX_BINARY_TAG_BYTES`, 16 MiB − 64 KiB |
| `art.mime` | `length(mime) <= 255` | 255 chars |
| `art.byte_len` | `byte_len <= 16711680` | `MAX_ART_BYTES`, 16 MiB − 64 KiB |
| `track_art.description` | `length(description) <= 1024` | 1 KiB (1024 chars) |
| `structural_blocks.kind` | `kind IN ('STREAMINFO','SEEKTABLE')` | whitelist |
| `structural_blocks.ordinal` | `ordinal >= 0` | — |
| `structural_blocks.body` | `length(body) <= 16777215` | 16 MiB − 1 byte (FLAC 24-bit block max) |

The `tags.value_blob` and `art.byte_len` caps are **defense-in-depth**: both blobs
stream at serve time (chunked `blob_open`/`read_at_exact`, never fully
materialized) and the binary-tag path is already rejected past `MAX_BLOCK_BODY`
at the format layer, so neither is a reachable allocation DoS. The `CHECK`s exist
only to bound on-disk blob size and keep the size contract symmetric with the
text/structural caps; they need **no** reader-side guard. Their values match the
scanner's own ingestion caps (`MAX_ART_BYTES` / `MAX_BINARY_TAG_BYTES`,
`musefs-core/src/scan.rs`) so every scanner-written row passes and only crafted
excess is rejected. `art`'s existing `CHECK (byte_len = length(data))` ties
`byte_len` to the blob, so capping `byte_len` caps `data`.

`structural_blocks` is **added to V4's rebuild set**: stash its rows → drop it
*before* `tracks` → recreate it *after* `tracks` is recreated → refill from the
stash. It has no triggers, so nothing is recreated for it. The V4 doc comment's
"`structural_blocks` … NOT rebuilt" note is corrected. As with the other V4
tables, a pre-existing violating row aborts the migration at the `INSERT…SELECT`
refill.

**Ordering is load-bearing and fixes a latent data-loss bug.**
`structural_blocks.track_id` references `tracks(id) ON DELETE CASCADE`, and the
V4 migration runs with `foreign_keys` ON (set by `configure`; a transaction
cannot toggle the pragma). `DROP TABLE tracks` therefore performs an implicit
`DELETE` of every `tracks` row, which fires the cascade on any still-present
child. The *current* V4 drops only `tags`/`art`/`track_art` before `tracks` and
leaves `structural_blocks` in place — so dropping `tracks` cascade-empties
`structural_blocks` (verified empirically: populated rows go to zero on
SQLite 3.46). For a fresh migration this is invisible (the table is empty), but
any previously-scanned V2/V3 DB upgraded to V4 silently loses its FLAC
`STREAMINFO`/`SEEKTABLE` blocks and falls back to front-reads until re-scanned.
Folding `structural_blocks` into the rebuild fixes this **only because** its
stash (`CREATE TEMP TABLE … AS SELECT * FROM structural_blocks`) runs at the top
*before* any `DROP`, and its `DROP` is sequenced before `DROP TABLE tracks`. The
plan must pin both: stash with the other three at the top, and drop it in the
children-first group ahead of `tracks`. A regression test asserting structural
rows survive a V2/V3→V4 migration is required (see Section 6).

This is a *move* of the effective `structural_blocks` definition from V2 to V4,
not a pure add. `MIGRATION_V2`'s `CREATE TABLE structural_blocks` is left
**unchanged** (unconstrained) — V2 must stay replayable for the V2→V3→V4 upgrade
path. The canonical post-migrate shape is the V4 (constrained) one;
`schema_sql_matches_migrate` already compares the rendered concatenation's
`dump_master` against a live `migrate()`, so both reflect the V4 recreate — name
that test as load-bearing for the ordering.

The `structural_blocks.body` ceiling equals FLAC's 24-bit block-length limit
(`MAX_BLOCK_BODY` in `musefs-format/src/flac.rs`); the value is duplicated as a
`musefs-db` constant (see Section 3) because the db layer cannot depend on the
format layer.

## Section 3 — reader-side fail-closed guards (`musefs-db` readers)

Named constants in `musefs-db`:

- `MAX_TAG_KEY_LEN = 256`
- `MAX_TAG_VALUE_LEN = 262144`
- `MAX_ART_MIME_LEN = 255`
- `MAX_ART_DESCRIPTION_LEN = 1024`
- `MAX_STRUCTURAL_BODY_LEN = 0x00FF_FFFF` (mirrors format's `MAX_BLOCK_BODY`; db
  cannot import the format layer — cross-checked in a core/integration test)
- `MAX_TAGS_PER_TRACK = 4096`
- `STRUCTURAL_KINDS = ["STREAMINFO", "SEEKTABLE"]` (single source for the V4
  `kind` `CHECK` and the `get_structural_blocks` guard)
- `MAX_BINARY_TAG_BYTES = 16711680` and `MAX_ART_BYTES = 16711680` — used only by
  the V4 blob `CHECK`s (Section 2), no reader guard; mirror the scanner caps in
  `musefs-core/src/scan.rs` (cross-checked in a core/integration test)

These are the constants for the read guards and CHECK rendering; the two blob
caps are CHECK-only.

**Check length in SQL, before materializing.** A Rust-side check that runs
*after* `r.get::<String>(col)` has already allocated the (potentially
multi-GiB) value does not prevent the allocation DoS — it only rejects after the
damage. Every guarded query therefore selects `length(col)` as an extra column
(SQLite computes it without transferring the payload), and the row mapper
rejects an over-cap row **before** calling `r.get::<String>`/`get::<Vec<u8>>`.
The guard is thus genuinely allocation-free.

**Guard every reader that materializes a capped field — not a generic "reader."**
The serve surface has multiple readers per field, and the hot paths do not go
through the single-item ones:

- `tags.value` / `tags.key` — guarded in **all four** text-tag readers
  (`get_tags`, `tags_for_tracks`, `tags_grouped`, `tags_grouped_for_keys`),
  routed through one shared validated row-mapper so a future tag reader cannot
  silently bypass the cap. The live serve paths are `tags_grouped_for_keys`
  (`facade.rs:317`, path rendering), `tags_for_tracks` (`facade.rs:497`, bulk
  rendering), and `get_tags` (`reader.rs:165`, synthesis); `tags_grouped` is the
  unfiltered variant.
- `tags.key` on the **binary** path — `get_binary_tags` (`tags.rs:123`,
  used at `mapping.rs:79`) is a *fifth* reader of `tags`: it filters to
  `value_blob IS NOT NULL` and materializes each row's `key` as a `String`. It
  must apply the same `MAX_TAG_KEY_LEN` guard, or a crafted DB smuggles an
  oversized `key` on a binary row past the text-reader guards. (The blob bytes
  themselves stream via `read_binary_tag_chunk_into` and are bounded — only `key`
  needs a read guard here.) While touching this method, fix its self-referential
  doc comment (`tags.rs:124` says "to match `get_binary_tags`" — it means the
  layout builder / `read_binary_tag_chunk` order).
- `art.mime` — guarded in `get_art_meta` (`art.rs`, the method `mapping.rs:44`
  actually calls). The art *data* blob is **not** guarded here, and the earlier
  "bounded by `MAX_ART_BYTES`" reasoning was wrong for the smuggled-DB case
  (`MAX_ART_BYTES` is a scan-time filter): the real reason no data guard is
  needed is that `get_art` (full-blob materialization) is **not on any serve
  path** — the serve path streams art via `read_art_chunk_into` — and the
  on-disk size is now bounded by the `art.byte_len` `CHECK` (Section 2).
- `track_art.description` — guarded in `get_track_art` (`art.rs`, used at
  `mapping.rs:39`).
- `structural_blocks.body`/`kind`/`ordinal` — guarded in `get_structural_blocks`
  (`structural.rs`, used at `reader.rs:174`): reject a `body` over
  `MAX_STRUCTURAL_BODY_LEN` (via `length(body)` pre-check), an unknown `kind`, or
  a negative `ordinal`. The valid-`kind` set is defined **once** as a `musefs-db`
  constant array used by both the reader guard and (rendered into) the V4 `CHECK`,
  with a drift assertion (Section 4) — it must not be duplicated as two
  independent literals.

**`MAX_TAGS_PER_TRACK` semantics (per-track row count).** The multi-value
explosion vector is a single track accumulating thousands of tag rows. The cap
is therefore a **per-track** count, enforced as rows are streamed:

- `get_tags` (single track, full load — the synthesis path that materializes
  *all* of a track's tags) is the primary site: error if the track yields more
  than `MAX_TAGS_PER_TRACK` rows.
- `tags_for_tracks` and `tags_grouped` (bulk, full tag set keyed by track):
  maintain a per-`track_id` running count while streaming and error if any track
  exceeds the cap — not a global `LIMIT`, which would silently truncate.
- `get_binary_tags` — binary rows live in the **same** `tags` table and are
  invisible to the text readers, so a track with 4096 text rows plus 100k binary
  rows would pass every text-side count. `get_binary_tags` returns a
  `Vec<BinaryTagRow>` (each holding a `String` key) for one track, so it applies
  the **same** per-track count cap to the binary set. The cap is a per-track count
  on *each* of the text and binary sets (a track may hold up to
  `MAX_TAGS_PER_TRACK` of each); a single combined ceiling would require a joined
  count the per-reader streaming model does not express, and the two separate
  bounds already cap total materialization at `2 × MAX_TAGS_PER_TRACK`.
- `tags_grouped_for_keys` (key-filtered) only loads the template's keys, so it
  cannot see a track's *total* tag count; there the per-track count bounds the
  returned subset. The full-load guard on `get_tags` plus the per-key value
  ordering already bound this path, so the subset count is belt-and-suspenders,
  not the primary defense.

Each guard error propagates to the serve path and maps to `EIO`, exactly like
the existing orphan-`art_id` case: a controlled failure, never an OOM or
undefined behavior. `SchemaMismatch` is the exception (surfaces at open, not as
an errno — Section 1).

Because `tags.value` is capped at the read boundary, template rendering never
sees an oversized string, so #267's "make rendering budget-aware" half is
**resolved by construction**: the existing post-render `truncate_component`
(`musefs-core/src/tree.rs`) stays as-is and no rendering rewrite is needed.

## Section 4 — single source of truth & mirrors

- The `CHECK` literals live in the V4 SQL; the reader guards use the `musefs-db`
  constants. The drift test must be **value-tied**, not a bare substring scan:
  asserting the SQL merely *contains* `"256"` is trivially satisfied (it is a
  substring of `262144`-adjacent text and several caps share digits). Instead
  assert the exact predicate substrings — `length(key) <= 256`,
  `length(value) <= 262144`, `value_blob IS NULL OR length(value_blob) <= 16711680`,
  `length(mime) <= 255`, `byte_len <= 16711680`, `length(description) <= 1024`,
  `length(body) <= 16777215`, and the `kind IN ('STREAMINFO','SEEKTABLE')` /
  `ordinal >= 0` predicates — and assert each `MAX_*` constant **equals** the
  value in its predicate (the value-level cross-check that actually prevents
  CHECK↔guard divergence).
- The cross-layer equality `MAX_STRUCTURAL_BODY_LEN == musefs-format::MAX_BLOCK_BODY`
  cannot be asserted in `musefs-db` (no format dependency). It is checked in a
  crate that sees both layers — a `musefs-core` or integration test — and the
  spec names that test as the owner of the cross-layer invariant. The
  `MAX_BINARY_TAG_BYTES`/`MAX_ART_BYTES` mirrors of `musefs-core/src/scan.rs` are
  cross-checked the same way.
- Regenerate the Python schema mirror:
  `MUSEFS_REGEN_SCHEMA_PY=1 cargo test -p musefs-db schema_py`, then re-vendor
  `musefs_common/schema.py` per CONTRIBUTING. Because the mirror renders each
  migration verbatim, the regenerated `SCHEMA_SQL` diff includes **both** the
  unchanged V2 `structural_blocks` create and the V4 drop+recreate — larger than
  "a few added CHECKs," and the re-vendor step is mandatory.
- `ARCHITECTURE.md`'s external-writer contract gains the length caps in its list
  of rejected shapes, a note that `structural_blocks` is now constrained, and the
  #270 identity-gate + reject-older policy.

## Section 5 — error handling

New `musefs-db` error variants:

- `SchemaMismatch` — carries the offending object identity for logs; its
  `Display` leads with problem + remedy (Section 1).
- `FieldTooLarge { table, field, len, max }`.
- `TooManyValues { track_id, count, max }`.

`FieldTooLarge`/`TooManyValues` propagate as `CoreError` and collapse to `EIO`
at the single translation site `musefs-fuse::errno` (`musefs-fuse/src/lib.rs:94`),
the same arm that already maps `CoreError::Format(_)` and the orphan-`art_id`
case. The plan must confirm the `DbError → CoreError` conversion carries the new
variants and that `errno`'s structural-collapse arm covers them (a new variant
that falls through to no arm would fail to compile or mis-map). The guarded
readers are also called from **non-fuse** paths (CLI, `scan` freshness checks,
and `tags_grouped` as the unfiltered variant); those must propagate the new
`Err` cleanly without an `EIO` framing or a panic.

`SchemaMismatch` surfaces at mount/open time (from `open_readonly`/`configure`),
so its message reaches the user directly rather than as an errno.

## Section 6 — testing

- **#270:** a forged DB (`user_version = 4` but with a hand-built table missing a
  `CHECK`/trigger, or an extra/altered object) → `open_readonly` rejects with
  `SchemaMismatch`; an honest freshly migrated DB opens. For a DB with **several**
  differences, assert the error names the *specific* first object (per the pinned
  `(type, name)` order), so the determinism is actually exercised, not just "some
  mismatch." Assert the message names the remedy (run a scan). Assert an orphaned
  child row is rejected by `foreign_key_check` **on a read-only connection**
  (not only via RW `configure`), since `open_readonly` never sets the
  `foreign_keys` pragma.
- **CHECKs:** an oversize `INSERT` on a normal connection aborts — both at the V4
  refill and at ordinary write time.
- **Reader guards:** craft an over-cap row via `PRAGMA
  ignore_check_constraints=ON` + oversize `INSERT` (schema fingerprint still
  canonical) → the reader returns `FieldTooLarge`. Cover **each** guarded method
  (`get_tags`, `tags_for_tracks`, `tags_grouped`, `tags_grouped_for_keys`,
  `get_binary_tags` for the binary-row `key`, `get_art_meta`, `get_track_art`,
  `get_structural_blocks`), since the shared mapper is the single point that must
  not be bypassed.
- **Allocation-free guard (observable):** the SELECT-`length()`-first ordering is
  a stated invariant, not just "an error is returned" — a guard that reads the
  value *then* checks its length passes a naive error-return test while silently
  re-introducing the DoS. Pin it observably: assert rejection fires on a query
  path that selects only `length(col)` (no value column), or use a test-build
  instrumentation counter on the value `get()`. "Asserts the error" alone is
  insufficient.
- **Boundary equality:** every cap is inclusive (`length(col) <= N`). For each
  capped field, test the pair — a row at **exactly** `N` is accepted by both the
  `CHECK` (plain `INSERT` succeeds) and the reader guard (read returns it), and a
  row at `N+1` is rejected by both. This catches a `<` vs `<=` off-by-one that
  every "oversize rejected / small accepted" test would miss. The
  `structural_blocks.body` = `MAX_BLOCK_BODY` boundary is highest-value: it must
  equal FLAC's real 24-bit limit, not be one byte short.
- **Per-track count:** more than `MAX_TAGS_PER_TRACK` rows for one track →
  `TooManyValues`, exercised on `get_tags` (single-track text), on a bulk reader
  (`tags_for_tracks`) where one track in the batch exceeds the cap while others
  do not, and on `get_binary_tags` (binary set) so the binary count is not blind.
- **Drift:** the V4 SQL embeds each constant; `schema.py` regeneration matches.
- **Migration:** existing V4 tests extended for the new `CHECK`s and the
  `structural_blocks` rebuild, including that a pre-existing violating row aborts
  the migration. **Regression for the cascade bug:** a V2/V3 DB seeded with
  `structural_blocks` rows migrates to V4 with those rows preserved (guards
  against the current cascade-on-`DROP TABLE tracks` data loss).
- **Fuzz:** format-layer signatures are unchanged, so `fuzz/` is unaffected;
  confirm with `cargo +nightly fuzz build`.

## Consequences

- A pre-cap V4 development DB fails the identity gate and must be regenerated by
  `musefs scan`. Deliberate, given no production DBs exist.
- Folding into V4 (rather than appending V5) keeps a single constraint migration
  and one table rebuild, at the cost of changing an already-written migration —
  permissible only because nothing has shipped.
