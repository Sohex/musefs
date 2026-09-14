# GENERATED from musefs-db/src/schema.rs — do not edit.
# Regenerate: MUSEFS_REGEN_SCHEMA_PY=1 cargo test -p musefs-db schema_py
# Re-vendor:  python contrib/python-musefs/vendor_to_picard.py

SCHEMA_SQL = """\
-- ── MIGRATION_V1 ──
CREATE TABLE tracks (
    id               INTEGER PRIMARY KEY,
    backing_path     TEXT NOT NULL UNIQUE,
    format           TEXT NOT NULL,
    audio_offset     INTEGER NOT NULL,
    audio_length     INTEGER NOT NULL,
    backing_size     INTEGER NOT NULL,
    backing_mtime_ns INTEGER NOT NULL,
    content_version  INTEGER NOT NULL DEFAULT 0,
    updated_at       INTEGER NOT NULL,
    backing_ctime_ns INTEGER NOT NULL DEFAULT 0 CHECK (backing_ctime_ns >= 0),
    CHECK (format IN ('flac','mp3','m4a','opus','vorbis','oggflac','wav')),
    CHECK (audio_offset >= 0),
    CHECK (audio_length >= 0),
    CHECK (backing_size >= 0),
    CHECK (backing_mtime_ns >= 0),
    CHECK (content_version >= 0),
    CHECK (updated_at >= 0),
    CHECK (audio_offset + audio_length <= backing_size)
);

CREATE TABLE tags (
    track_id   INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,
    ordinal    INTEGER NOT NULL DEFAULT 0,
    value_blob BLOB,
    PRIMARY KEY (track_id, key, ordinal),
    CHECK (ordinal >= 0),
    CHECK (value_blob IS NULL OR value = ''),
    CHECK (length(key) <= 256),
    CHECK (length(key) >= 1
           AND key NOT GLOB '*[' || char(1) || '-' || char(31) || ']*'),
    CHECK (length(value) <= 262144),
    CHECK (value_blob IS NULL OR length(value_blob) <= 16711680)
);

CREATE TABLE art (
    id       INTEGER PRIMARY KEY,
    sha256   TEXT NOT NULL UNIQUE,
    mime     TEXT NOT NULL,
    width    INTEGER,
    height   INTEGER,
    byte_len INTEGER NOT NULL,
    data     BLOB NOT NULL,
    CHECK (byte_len = length(data)),
    CHECK (length(sha256) = 64),
    CHECK (width IS NULL OR width >= 0),
    CHECK (height IS NULL OR height >= 0),
    CHECK (length(mime) <= 255),
    CHECK (byte_len <= 16711680)
);

CREATE TABLE track_art (
    track_id     INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    art_id       INTEGER NOT NULL REFERENCES art(id),
    picture_type INTEGER NOT NULL DEFAULT 3,
    description  TEXT NOT NULL DEFAULT '',
    ordinal      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (track_id, ordinal),
    CHECK (picture_type BETWEEN 0 AND 20),
    CHECK (ordinal >= 0),
    CHECK (length(description) <= 1024)
);

-- Read-only, derived-from-file structural metadata (FLAC STREAMINFO/SEEKTABLE).
-- NOT part of the editable `tags` contract: external tools never touch it.
CREATE TABLE structural_blocks (
    track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    kind     TEXT NOT NULL,
    ordinal  INTEGER NOT NULL DEFAULT 0,
    body     BLOB NOT NULL,
    PRIMARY KEY (track_id, kind, ordinal),
    CHECK (kind IN ('STREAMINFO','SEEKTABLE')),
    CHECK (ordinal >= 0),
    CHECK (length(body) <= 16777215)
);

-- Bounded changelog ring for O(changed) refresh. Every metadata edit funnels
-- through an UPDATE on the tracks row (the tags/track_art triggers), so
-- triggers on tracks alone capture all writers. Relies on SQLite nested
-- trigger activation (on by default; distinct from PRAGMA recursive_triggers).
CREATE TABLE track_changes (
    seq      INTEGER PRIMARY KEY AUTOINCREMENT,
    track_id INTEGER NOT NULL
);

-- Index the reverse art -> track_art edge so bulk orphan-GC and the art delete
-- trigger below do not scan the whole join table per deleted row.
CREATE INDEX track_art_art_id_idx ON track_art(art_id);

CREATE TRIGGER tags_ai AFTER INSERT ON tags BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = NEW.track_id;
END;
CREATE TRIGGER tags_au AFTER UPDATE ON tags BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = NEW.track_id;
END;
CREATE TRIGGER tags_ad AFTER DELETE ON tags BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = OLD.track_id;
END;

CREATE TRIGGER track_art_ai AFTER INSERT ON track_art BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = NEW.track_id;
END;
CREATE TRIGGER track_art_au AFTER UPDATE ON track_art BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = NEW.track_id;
END;
CREATE TRIGGER track_art_ad AFTER DELETE ON track_art BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = OLD.track_id;
END;

CREATE TRIGGER tracks_changelog_ai AFTER INSERT ON tracks BEGIN
    INSERT INTO track_changes (track_id) VALUES (NEW.id);
END;
CREATE TRIGGER tracks_changelog_au AFTER UPDATE ON tracks BEGIN
    INSERT INTO track_changes (track_id) VALUES (NEW.id);
END;
CREATE TRIGGER tracks_changelog_ad AFTER DELETE ON tracks BEGIN
    INSERT INTO track_changes (track_id) VALUES (OLD.id);
END;

-- Self-pruning ring: writers maintain it; the mount's read-only connections
-- never need to. Deletes only from the old end, so retained seqs stay contiguous.
CREATE TRIGGER track_changes_prune AFTER INSERT ON track_changes BEGIN
    DELETE FROM track_changes WHERE seq <= NEW.seq - 8192;
END;

-- art rows are content-addressed by sha256: once written, their content
-- columns are immutable. A writer needing different bytes/metadata inserts a
-- NEW row and relinks via track_art (which bumps content_version through the
-- track_art triggers). width/height use IS NOT (NULL-safe) because they are
-- nullable; the NOT NULL columns use <>.
CREATE TRIGGER art_reject_content_update
BEFORE UPDATE ON art
WHEN NEW.data   <> OLD.data
  OR NEW.sha256 <> OLD.sha256
  OR NEW.mime   <> OLD.mime
  OR NEW.byte_len <> OLD.byte_len
  OR NEW.width  IS NOT OLD.width
  OR NEW.height IS NOT OLD.height
BEGIN
    SELECT RAISE(ABORT,
        'art rows are immutable; insert a new content-addressed row and relink via track_art');
END;

-- Deleting an art row that still has track_art references (an orphan an
-- external writer can produce with foreign_keys OFF) bumps every referencing
-- track, so the mount rebuilds and serves a clean EIO on the orphan rather
-- than streaming stale bytes from an old cached layout. Inert on the normal
-- gc_orphan_art path, where the deleted row has no references.
CREATE TRIGGER art_ad AFTER DELETE ON art BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id IN (SELECT track_id FROM track_art WHERE art_id = OLD.id);
END;

-- Scanner-owned geometry feeds the synthesized layout, but upsert_track does
-- not touch content_version. Bump it whenever a geometry column actually
-- changes, making content_version a true superset of served-byte inputs. The
-- WHEN guard is false on this trigger's own nested UPDATE (only content_version
-- changes), so the recursion terminates after exactly one bump.
CREATE TRIGGER tracks_geometry_au
AFTER UPDATE ON tracks
WHEN NEW.format        <> OLD.format
  OR NEW.audio_offset  <> OLD.audio_offset
  OR NEW.audio_length  <> OLD.audio_length
  OR NEW.backing_size  <> OLD.backing_size
  OR NEW.backing_mtime_ns <> OLD.backing_mtime_ns
BEGIN
    UPDATE tracks SET content_version = content_version + 1 WHERE id = NEW.id;
END;

-- FLAC structural blocks feed synthesized headers and flip the synthesis path
-- (legacy front-read fallback vs streamed fast path), so a change must bump.
-- set_structural_blocks is DELETE-then-INSERT (no UPDATE path exists), so these
-- fire on every rewrite; the resulting over-bump on a byte-identical re-probe
-- is harmless monotone churn (content_version is compared only for equality).
CREATE TRIGGER structural_blocks_ai AFTER INSERT ON structural_blocks BEGIN
    UPDATE tracks SET content_version = content_version + 1 WHERE id = NEW.track_id;
END;
CREATE TRIGGER structural_blocks_ad AFTER DELETE ON structural_blocks BEGIN
    UPDATE tracks SET content_version = content_version + 1 WHERE id = OLD.track_id;
END;
PRAGMA user_version = 1;

-- ── MIGRATION_V2 ──
-- fingerprint/content_hash are scanner-owned content identities. Neither is
-- UNIQUE and the index is NON-unique BY DESIGN: duplicate-content tracks (same
-- album in two places, genuine dupes) legitimately share both values, and a
-- UNIQUE constraint would abort the scan batch on the second copy. Correctness
-- comes from the refind logic (unique-missing candidate + confirmation), not
-- from DB uniqueness. Both columns carry a length(x) = 64 CHECK locking them
-- to SHA-256 hex (Task E2 benchmark locked the hash to SHA-256: under a
-- realistic SSD/HDD I/O profile the fingerprint adds ~8.6%; the RAM
-- microbench's higher ratio is an I/O-elimination artifact — see
-- the benchmarks docs). Hash function is now fixed, so the CHECK is added here.
ALTER TABLE tracks ADD COLUMN fingerprint  TEXT
    CHECK (fingerprint IS NULL OR length(fingerprint) = 64);
ALTER TABLE tracks ADD COLUMN content_hash TEXT
    CHECK (content_hash IS NULL OR length(content_hash) = 64);
CREATE INDEX tracks_fingerprint_idx ON tracks(fingerprint);

-- Rebuild `tags` with a byte-accurate value cap (#505). SQLite's length() on
-- TEXT counts characters, so the V1 `CHECK (length(value) <= 262144)` bounded a
-- value's bytes only to about four times that; length(CAST(value AS BLOB))
-- counts bytes. SQLite cannot alter a CHECK in place, so recreate the table.
--
-- The cap is 16 MiB - 1, FLAC's metadata-block ceiling and where V3 puts it
-- too (#644), and the refill keeps every row. This step first shipped at 256
-- KiB and dropped each row past it: a multibyte lyrics tag V1's character cap
-- admitted, which 1.0.0 served and V3 and V4 would have kept, went without a
-- word, and the 2.0.0 upgrade's pre-flight, which checks rows against V4, never
-- saw it go. V1's character cap bounds a value to about 1 MiB in bytes, so no
-- row V1 holds fails this CHECK.
--
-- A released step's text is safe to change here, and only because of where the
-- step now runs. A store already past V1 ran the old text, and V3 and V4 both
-- rebuild `tags` after it, so nothing of that text survives in any schema. A
-- store still at V1 reaches this step only through `musefs migrate`, which runs
-- V3 and V4 with it (#749). V3's note about V2's narrowing describes the text
-- this replaced.
CREATE TABLE tags_new (
    track_id   INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,
    ordinal    INTEGER NOT NULL DEFAULT 0,
    value_blob BLOB,
    PRIMARY KEY (track_id, key, ordinal),
    CHECK (ordinal >= 0),
    CHECK (value_blob IS NULL OR value = ''),
    CHECK (length(key) <= 256),
    CHECK (length(key) >= 1
           AND key NOT GLOB '*[' || char(1) || '-' || char(31) || ']*'),
    CHECK (length(CAST(value AS BLOB)) <= 16777215),
    CHECK (value_blob IS NULL OR length(value_blob) <= 16711680)
);
INSERT INTO tags_new (track_id, key, value, ordinal, value_blob)
    SELECT track_id, key, value, ordinal, value_blob FROM tags;
DROP TABLE tags;
ALTER TABLE tags_new RENAME TO tags;

-- DROP TABLE tags dropped its INSERT/UPDATE/DELETE triggers; recreate them
-- verbatim so the content_version/updated_at bump contract is unchanged.
CREATE TRIGGER tags_ai AFTER INSERT ON tags BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = NEW.track_id;
END;
CREATE TRIGGER tags_au AFTER UPDATE ON tags BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = NEW.track_id;
END;
CREATE TRIGGER tags_ad AFTER DELETE ON tags BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = OLD.track_id;
END;
PRAGMA user_version = 2;

-- ── MIGRATION_V3 ──
-- Widen the two caps musefs invented rather than inherited (#644).
--
-- `tags.value` moves 256 KiB -> 16 MiB - 1 (FLAC's 24-bit metadata-block
-- ceiling, the largest tag synthesis could ever serve) and
-- `track_art.description` moves 1 KiB -> 8 KiB. Both are *widenings*, so the
-- refills need no WHERE filter and drop no rows -- unlike V2's narrowing, which
-- had to shed over-cap rows to avoid aborting on its own new CHECK.
--
-- SQLite cannot alter a CHECK in place, so both tables are recreated. V1/V2
-- text is left untouched: they must stay replayable for a V1 -> V2 -> V3
-- upgrade, and their literals are frozen history, not the current caps.

-- `art_ad`'s body reads `track_art`. ALTER TABLE ... RENAME reparses the whole
-- schema and would fail with 'error in trigger art_ad: no such table' while
-- track_art is momentarily absent, so drop it up front and recreate it verbatim
-- below. (V2's `tags` rebuild needed no such dance: nothing referenced `tags`.)
DROP TRIGGER art_ad;

CREATE TABLE tags_v3 (
    track_id   INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,
    ordinal    INTEGER NOT NULL DEFAULT 0,
    value_blob BLOB,
    PRIMARY KEY (track_id, key, ordinal),
    CHECK (ordinal >= 0),
    CHECK (value_blob IS NULL OR value = ''),
    CHECK (length(key) <= 256),
    CHECK (length(key) >= 1
           AND key NOT GLOB '*[' || char(1) || '-' || char(31) || ']*'),
    CHECK (length(CAST(value AS BLOB)) <= 16777215),
    CHECK (value_blob IS NULL OR length(value_blob) <= 16711680)
);
INSERT INTO tags_v3 (track_id, key, value, ordinal, value_blob)
    SELECT track_id, key, value, ordinal, value_blob FROM tags;
DROP TABLE tags;
ALTER TABLE tags_v3 RENAME TO tags;

CREATE TABLE track_art_v3 (
    track_id     INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    art_id       INTEGER NOT NULL REFERENCES art(id),
    picture_type INTEGER NOT NULL DEFAULT 3,
    description  TEXT NOT NULL DEFAULT '',
    ordinal      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (track_id, ordinal),
    CHECK (picture_type BETWEEN 0 AND 20),
    CHECK (ordinal >= 0),
    CHECK (length(description) <= 8192)
);
INSERT INTO track_art_v3 (track_id, art_id, picture_type, description, ordinal)
    SELECT track_id, art_id, picture_type, description, ordinal FROM track_art;
DROP TABLE track_art;
ALTER TABLE track_art_v3 RENAME TO track_art;

-- DROP TABLE took each table's triggers (and track_art's index) with it;
-- recreate them verbatim so the content_version/updated_at bump contract and
-- the reverse art -> track_art edge are unchanged.
CREATE INDEX track_art_art_id_idx ON track_art(art_id);

CREATE TRIGGER tags_ai AFTER INSERT ON tags BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = NEW.track_id;
END;
CREATE TRIGGER tags_au AFTER UPDATE ON tags BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = NEW.track_id;
END;
CREATE TRIGGER tags_ad AFTER DELETE ON tags BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = OLD.track_id;
END;

CREATE TRIGGER track_art_ai AFTER INSERT ON track_art BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = NEW.track_id;
END;
CREATE TRIGGER track_art_au AFTER UPDATE ON track_art BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = NEW.track_id;
END;
CREATE TRIGGER track_art_ad AFTER DELETE ON track_art BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = OLD.track_id;
END;

CREATE TRIGGER art_ad AFTER DELETE ON art BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id IN (SELECT track_id FROM track_art WHERE art_id = OLD.id);
END;
PRAGMA user_version = 3;

-- ── MIGRATION_V4 ──
-- Retire every V1-era `fingerprint` (#691).
--
-- The cheap fingerprint used to hash only the probe's *parsed* output. Outside
-- FLAC -- whose STREAMINFO carries an MD5 of the unencoded audio, and is the
-- one structural block the probe preserves -- that input domain holds no audio
-- bytes at all, so two different MP3/M4A/Ogg/WAV files with the same tags, the
-- same art and an equal audio-region length shared one fingerprint. The
-- default strictness accepts a fingerprint-only candidate, so such a collision
-- could retarget a curated row onto audio it was never written for.
--
-- The fingerprint now folds in sampled audio bytes, which changes the value for
-- every file. Rows carrying the old value would claim a fingerprint under an
-- algorithm that no longer produces it -- a stale content identity of exactly
-- the kind #689 is about -- so they are nulled here rather than silently
-- reinterpreted. The next `revalidate` recomputes them: it already re-probes a
-- row missing the checksum its tier asks for, so no new backfill machinery is
-- needed. `content_hash` is nulled too, although its meaning did not change:
-- before #689 a fingerprint-tier rescan of a rewritten file kept the old bytes'
-- hash, so no stored value can be trusted to describe the file beside it.
-- `revalidate --checksum=full` recomputes those.
--
-- The cost of nulling is bounded and one-way: a file that moves between this
-- upgrade and the next scan is not move-recovered (it inserts fresh, as an
-- unfingerprinted row always has). Leaving the old values in place would not
-- recover it either -- they cannot match a new-algorithm fingerprint -- so this
-- trades nothing away for an honest column. It is folded into the rebuild's
-- refill below rather than run as a statement of its own: the refill rewrites
-- every row anyway, so a separate UPDATE would rewrite them all twice.

-- ─────────────────────────────────────────────────────────────────────────────
-- Rebuild `tracks` (#686). This is the expensive, once-only event of the 2.0.0
-- migration: `tracks` is the only table with children, and SQLite cannot add
-- AUTOINCREMENT, change a column's type, or alter a CHECK in place. Everything
-- wanting a `tracks` schema change therefore rides this one rebuild:
--
--   #678  id -> INTEGER PRIMARY KEY AUTOINCREMENT   (what forces the rebuild)
--   #674  add backing_ino
--   #680  backing_path TEXT -> BLOB
--   #696  drop the lower bounds on backing_mtime_ns and backing_ctime_ns
--   #693  NUL-proof the two checksum columns
--   #718  storage-class constraints, backing_path above all
--   #691  the fingerprint reset above, folded into the refill
--
-- Foreign keys stay enforced throughout (the pragma is a no-op inside the
-- migration's transaction in any case), which is what dictates the shape below:
-- with enforcement on, DROP TABLE performs an implicit DELETE, and that DELETE
-- cascades into the three child tables. So the children are copied out first
-- and restored afterwards, rather than the pragma being weakened for the run.

-- 1. Every trigger that is on `tracks` or names it in its body: thirteen, not
-- the twelve the V4 plan counted -- `art_ad` names `tracks` too, not only
-- `track_art`. Dropping the eight on the child tables is not tidiness: the
-- cascade from DROP TABLE below fires the children's AFTER DELETE triggers,
-- and those UPDATE a `tracks` that is in the middle of being dropped. The
-- refill would likewise fire the AFTER INSERT triggers and bump
-- `content_version` on every row in the store -- which the served virtual mtime
-- now derives from (#725), so an accidental bump is visible outside musefs.
DROP TRIGGER tracks_changelog_ai;
DROP TRIGGER tracks_changelog_au;
DROP TRIGGER tracks_changelog_ad;
DROP TRIGGER tracks_geometry_au;
DROP TRIGGER tags_ai;
DROP TRIGGER tags_au;
DROP TRIGGER tags_ad;
DROP TRIGGER track_art_ai;
DROP TRIGGER track_art_au;
DROP TRIGGER track_art_ad;
DROP TRIGGER structural_blocks_ai;
DROP TRIGGER structural_blocks_ad;
DROP TRIGGER art_ad;

-- 2. Hold the parent and the three children. The holding tables carry no
-- constraints and no foreign keys, so nothing in them can abort on the shape
-- being replaced, and the values round-trip by storage class rather than by
-- affinity.
CREATE TABLE tracks_hold_v4 (
    id               INTEGER,
    backing_path     TEXT,
    format           TEXT,
    audio_offset     INTEGER,
    audio_length     INTEGER,
    backing_size     INTEGER,
    backing_mtime_ns INTEGER,
    content_version  INTEGER,
    updated_at       INTEGER,
    backing_ctime_ns INTEGER,
    content_hash     TEXT
);
INSERT INTO tracks_hold_v4
    SELECT id, backing_path, format, audio_offset, audio_length, backing_size,
           backing_mtime_ns, content_version, updated_at, backing_ctime_ns,
           content_hash
    FROM tracks;

CREATE TABLE tags_hold_v4 (
    track_id INTEGER, key TEXT, value TEXT, ordinal INTEGER, value_blob BLOB
);
INSERT INTO tags_hold_v4
    SELECT track_id, key, value, ordinal, value_blob FROM tags;

CREATE TABLE track_art_hold_v4 (
    track_id INTEGER, art_id INTEGER, picture_type INTEGER,
    description TEXT, ordinal INTEGER
);
INSERT INTO track_art_hold_v4
    SELECT track_id, art_id, picture_type, description, ordinal FROM track_art;

CREATE TABLE structural_blocks_hold_v4 (
    track_id INTEGER, kind TEXT, ordinal INTEGER, body BLOB
);
INSERT INTO structural_blocks_hold_v4
    SELECT track_id, kind, ordinal, body FROM structural_blocks;

-- 3. Drop and recreate. The new table is created under its final name rather
-- than built beside the old one and renamed: ALTER TABLE ... RENAME reparses
-- the whole schema, and the thirteen triggers above would have to be absent for
-- that to succeed anyway (the `art_ad` problem V3 hit, at `tracks` scale).
DROP TABLE tracks;

CREATE TABLE tracks (
    -- AUTOINCREMENT so a deleted id is never handed back out (#678). The
    -- incremental refresh treats the id as a persistent identity, and the
    -- default allocator's max(rowid)+1 let a pruned track and its replacement
    -- collide on id, format and content_version -- a substitution the refresh
    -- then blessed as a no-op.
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    -- BLOB, not TEXT (#680): a filesystem path is bytes, and the lossy
    -- String round-trip collapsed two distinct files onto one row. The
    -- typeof CHECK is what makes the column's type a guarantee rather than an
    -- affinity (#718): SQLite would otherwise accept TEXT here, and UNIQUE
    -- does not compare a TEXT path equal to the same bytes as a BLOB, so the
    -- one path could occupy two rows through a second door.
    backing_path     BLOB NOT NULL UNIQUE,
    format           TEXT NOT NULL,
    audio_offset     INTEGER NOT NULL,
    audio_length     INTEGER NOT NULL,
    backing_size     INTEGER NOT NULL,
    backing_mtime_ns INTEGER NOT NULL,
    content_version  INTEGER NOT NULL DEFAULT 0,
    updated_at       INTEGER NOT NULL,
    backing_ctime_ns INTEGER NOT NULL DEFAULT 0,
    fingerprint      TEXT,
    content_hash     TEXT,
    -- The inode, for backing filesystems that do not store sub-second
    -- timestamps (#674). Zero is the sentinel for `not yet known`, matching the
    -- backing_ctime_ns precedent: an upgraded store starts every row unknown,
    -- and each scan arms the guard for the rows it touches. Linux never hands
    -- out inode 0 for a file, so the sentinel cannot collide with a real value;
    -- the Rust model still says `Option<u64>` rather than making every reader
    -- remember that. `musefs revalidate` re-probes exactly the rows
    -- still holding the sentinel, which is what makes it the repopulation path
    -- for an upgraded store.
    --
    -- Stored as the inode's two's-complement bit pattern, so a value above
    -- i64::MAX reads back negative: `st_ino` is a u64 and SQLite has no
    -- unsigned 64-bit integer. Compared only for equality (the `<>` in
    -- `tracks_geometry_au`, and the Rust freshness stamp), never ordered or
    -- summed, so the encoding costs nothing it is used for.
    backing_ino      INTEGER NOT NULL DEFAULT 0,
    -- The upper bound is 64 KiB (#758), a portable ceiling rather than any
    -- platform's PATH_MAX: no path past the OS limit can be opened to serve, and
    -- without a bound a crafted row chose the size of the allocation every reader
    -- makes, getattr included. The readers re-check it from length() before
    -- loading the path, for a store written with its constraints off.
    CHECK (typeof(backing_path) = 'blob'
           AND length(backing_path) > 0
           AND length(backing_path) <= 65536
           AND instr(backing_path, x'00') = 0),
    -- The IN list is strictly stronger than a typeof CHECK would be: no
    -- non-TEXT value compares equal to any of these, so the storage class is
    -- already pinned.
    CHECK (format IN ('flac','mp3','m4a','opus','vorbis','oggflac','wav')),
    CHECK (typeof(audio_offset) = 'integer' AND audio_offset >= 0),
    CHECK (typeof(audio_length) = 'integer' AND audio_length >= 0),
    CHECK (typeof(backing_size) = 'integer' AND backing_size >= 0),
    -- No lower bound on either stamp (#696): a backing file dated before 1970
    -- carries a negative nanosecond offset, and rejecting it here kept the file
    -- out of the mount entirely. The storage class is still pinned, because the
    -- Rust side reads both as i64.
    CHECK (typeof(backing_mtime_ns) = 'integer'),
    CHECK (typeof(backing_ctime_ns) = 'integer'),
    -- No lower bound: the column holds the inode's two's-complement bit
    -- pattern, so an inode above i64::MAX is stored negative. SQLite INTEGER is
    -- signed 64-bit and `st_ino` is a full u64, so the encoding is forced --
    -- see `models::ino_to_col`. The storage class is still pinned, which is the
    -- half that stops a Rust-side conversion failure.
    CHECK (typeof(backing_ino) = 'integer'),
    CHECK (typeof(content_version) = 'integer' AND content_version >= 0),
    CHECK (typeof(updated_at) = 'integer' AND updated_at >= 0),
    CHECK (audio_offset + audio_length <= backing_size),
    -- instr(..., char(0)) = 0 alongside the character cap (#693): SQLite's
    -- length() on TEXT stops at the first NUL, so `<64 hex chars>` + NUL +
    -- anything satisfied a bare length() = 64 while storing something else
    -- entirely. Banning NUL keeps the documented `64 characters` meaning rather
    -- than quietly converting the field to a byte cap.
    --
    -- And the characters are lowercase hex (#761), the one spelling every
    -- writer produces and the only one an equality match finds. GLOB is
    -- case-sensitive, so the one clause refuses uppercase and non-hex alike; the
    -- NUL test stays beside it because GLOB, like length(), stops at a NUL.
    CHECK (fingerprint IS NULL
           OR (typeof(fingerprint) = 'text'
               AND length(fingerprint) = 64
               AND instr(fingerprint, char(0)) = 0
               AND fingerprint NOT GLOB '*[^0-9a-f]*')),
    CHECK (content_hash IS NULL
           OR (typeof(content_hash) = 'text'
               AND length(content_hash) = 64
               AND instr(content_hash, char(0)) = 0
               AND content_hash NOT GLOB '*[^0-9a-f]*'))
);

-- 4. Refill. CAST(backing_path AS BLOB) is what preserves identity across the
-- type change: the bytes are unchanged, and without the cast every existing row
-- would be unreachable to a byte-binding reader while the unique index failed
-- to fire, silently giving each track a second row.
--
-- `fingerprint` is dropped on the floor here -- that is #691's reset, folded in.
--
-- `content_hash` is dropped on the floor as well (see the #691 note above for
-- why none is trusted). It is a scanner-owned derived column a revalidate
-- recomputes, which is the case the sanitize-only-under-a-flag policy carves
-- out, and it also means no stored hash can abort the upgrade by failing the
-- tightened CHECK. The other tightened columns are
-- NOT sanitized here -- they are either structural or NOT NULL, so a row that
-- violates them fails the migration. That failure is atomic: every step runs in
-- one transaction, so nothing is half-applied and the store is exactly as it
-- was. Repairing such a row, or reporting it before the run starts, is the
-- command's business rather than this SQL's -- rewriting a value an external
-- writer chose is precisely what must not happen without being asked.
INSERT INTO tracks (id, backing_path, format, audio_offset, audio_length,
                    backing_size, backing_mtime_ns, content_version, updated_at,
                    backing_ctime_ns, fingerprint, content_hash, backing_ino)
    SELECT id, CAST(backing_path AS BLOB), format, audio_offset, audio_length,
           backing_size, backing_mtime_ns, content_version, updated_at,
           backing_ctime_ns,
           NULL,
           NULL,
           0
    FROM tracks_hold_v4;

-- The refill leaves `sqlite_sequence` at the highest id still standing. The old
-- table allocated max(id) + 1, so a track deleted from the top of the range
-- left its id for the next insert to take (#678), and the changelog ring may
-- still name that id -- the ring goes below -- as may state an external tool
-- kept. So the sequence starts past the highest id the ring holds too. A child
-- row cannot name a higher one: one whose track is gone fails the refill below,
-- or `migrate --repair` has removed it. A ring row whose track_id is not an
-- integer names no track.
UPDATE sqlite_sequence
   SET seq = (SELECT max(track_id) FROM track_changes WHERE typeof(track_id) = 'integer')
 WHERE name = 'tracks'
   AND seq < (SELECT max(track_id) FROM track_changes WHERE typeof(track_id) = 'integer');
INSERT INTO sqlite_sequence (name, seq)
    SELECT 'tracks', ring.top
    FROM (SELECT max(track_id) AS top FROM track_changes
          WHERE typeof(track_id) = 'integer') AS ring
    WHERE ring.top > 0
      AND NOT EXISTS (SELECT 1 FROM sqlite_sequence WHERE name = 'tracks');

-- 5. Rebuild the three child tables. All are empty right now -- the cascade
-- above took them -- so each is a drop and a create, with the holding tables as
-- the source. `tags` and `track_art` change shape; `structural_blocks` keeps
-- its columns and gains only the storage classes every other table now pins.

-- `tags` loses its primary key in favour of one unique index that folds the row
-- class, `value_blob IS NULL`, in as a fourth column (#663). The PK numbered a
-- track's text rows and its binary rows in one ordinal space per key, and an
-- external writer that
-- rewrites text rows alone -- which both `contrib` helpers do, scoping their
-- DELETE to `value_blob IS NULL` so scanner-written binary payloads survive --
-- could write a text row onto an ordinal a binary row already held. The two
-- classes now get independent ordinal spaces. The rowids are not carried
-- across: the refill below lets SQLite assign fresh ones. Binary tag payloads
-- are addressed by rowid only from a served layout, and every layout is built
-- from the store after it opens, which a store behind this gated step cannot do
-- until the migration has finished.
DROP TABLE tags;
CREATE TABLE tags (
    track_id   INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,
    ordinal    INTEGER NOT NULL DEFAULT 0,
    value_blob BLOB,
    CHECK (typeof(track_id) = 'integer'),
    CHECK (typeof(ordinal) = 'integer' AND ordinal >= 0),
    CHECK (value_blob IS NULL OR value = ''),
    -- instr(key, char(0)) = 0 alongside the character cap (#693): SQLite's
    -- length() on TEXT stops at the first NUL, so a short prefix plus NUL plus
    -- a megabyte of anything measured 1 and stored the lot.
    CHECK (typeof(key) = 'text'
           AND length(key) <= 256
           AND length(key) >= 1
           AND instr(key, char(0)) = 0
           AND key NOT GLOB '*[' || char(1) || '-' || char(31) || ']*'),
    CHECK (typeof(value) = 'text' AND length(CAST(value AS BLOB)) <= 16777215),
    CHECK (value_blob IS NULL
           OR (typeof(value_blob) = 'blob' AND length(value_blob) <= 16711680))
);

-- `track_art` gains the per-embedding columns (#716). `mime`, `width` and
-- `height` describe one file's picture block, not the image bytes every file
-- shares, so owning them on the deduplicated `art` row was the wrong functional
-- dependency: whichever occurrence was ingested first chose them for every
-- track referencing the blob. `depth` and `colors` are new storage for values
-- FLAC's parser already reads and throws away.
--
-- The backfill can only copy the shared values to every link -- the true
-- per-embedding ones were destroyed at ingest and come back on a revalidate,
-- which is what `musefs migrate`'s revalidate offer is for (#746). `depth` and
-- `colors` have no shared value to copy and start at 0, which is what both the
-- format and
-- synthesis already take to mean unknown.
DROP TABLE track_art;
CREATE TABLE track_art (
    track_id     INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    art_id       INTEGER NOT NULL REFERENCES art(id),
    picture_type INTEGER NOT NULL DEFAULT 3,
    description  TEXT NOT NULL DEFAULT '',
    mime         TEXT NOT NULL DEFAULT '',
    width        INTEGER,
    height       INTEGER,
    depth        INTEGER NOT NULL DEFAULT 0,
    colors       INTEGER NOT NULL DEFAULT 0,
    ordinal      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (track_id, ordinal),
    CHECK (typeof(track_id) = 'integer'),
    CHECK (typeof(art_id) = 'integer'),
    CHECK (typeof(picture_type) = 'integer' AND picture_type BETWEEN 0 AND 20),
    CHECK (typeof(ordinal) = 'integer' AND ordinal >= 0),
    CHECK (typeof(description) = 'text'
           AND length(description) <= 8192
           AND instr(description, char(0)) = 0),
    -- #693's ban follows the column rather than staying on `art.mime`, which
    -- the next step removes.
    CHECK (typeof(mime) = 'text'
           AND length(mime) <= 255
           AND instr(mime, char(0)) = 0),
    -- Upper bounds tie these to the Rust model's width: the geometry is
    -- Option<u32>, and a schema-valid 2^40 was a conversion failure (#718).
    CHECK (width IS NULL
           OR (typeof(width) = 'integer' AND width BETWEEN 0 AND 4294967295)),
    CHECK (height IS NULL
           OR (typeof(height) = 'integer' AND height BETWEEN 0 AND 4294967295)),
    CHECK (typeof(depth) = 'integer' AND depth BETWEEN 0 AND 4294967295),
    CHECK (typeof(colors) = 'integer' AND colors BETWEEN 0 AND 4294967295)
);

-- 6. Rebuild `art`. This has to happen *here*, between `track_art` being
-- recreated and the children being refilled, and the window is narrow for a
-- reason: `track_art.art_id` references `art(id)` with no ON DELETE CASCADE, so
-- with foreign keys enforced `DROP TABLE art` fails outright while any link row
-- exists. Right now none does -- the cascade emptied `track_art` and the refill
-- below has not run yet -- so this is the one point in the migration where the
-- table can be replaced at all.
--
-- This is also the expensive step: every image blob is copied twice, and the
-- store transiently holds about its own size again. That cost is what the
-- command's free-space pre-flight exists to check before starting.
--
-- The refill is straight: `art` carries no scanner-owned column a rescan could
-- recompute, so there is nothing here that the sanitize-only-under-a-flag
-- policy would let this step null on its own. A row the tightened constraints
-- reject fails the migration, atomically, the way it does for the rebuilds above.
-- The holding table keeps the three columns the new `art` drops: it is what the
-- `track_art` backfill below reads them from, since by then the real table no
-- longer has them.
CREATE TABLE art_hold_v4 (
    id INTEGER, sha256 TEXT, mime TEXT, width INTEGER, height INTEGER,
    byte_len INTEGER, data BLOB
);
INSERT INTO art_hold_v4
    SELECT id, sha256, mime, width, height, byte_len, data FROM art;
DROP TABLE art;
CREATE TABLE art (
    id       INTEGER PRIMARY KEY,
    sha256   TEXT NOT NULL UNIQUE,
    -- No mime, no width, no height: they describe one file's picture block,
    -- not the bytes every file sharing this row holds, and they live on
    -- `track_art` now (#716). What is left is the content and its identity.
    byte_len INTEGER NOT NULL,
    data     BLOB NOT NULL,
    -- Lowercase hex, as on the two track checksums (#761). Dedup is
    -- `ON CONFLICT(sha256)`, a text match, so the same bytes filed under another
    -- spelling of their digest were stored twice and never reached #724's byte
    -- comparison. A refill row that fails is not lowercased: a lowercase row for
    -- the same bytes may already exist, which is the collision this prevents.
    -- It fails the migration, and `migrate`'s pre-flight reports it first.
    CHECK (typeof(sha256) = 'text'
           AND length(sha256) = 64
           AND instr(sha256, char(0)) = 0
           AND sha256 NOT GLOB '*[^0-9a-f]*'),
    CHECK (typeof(byte_len) = 'integer'
           AND byte_len >= 0
           AND byte_len <= 16711680),
    CHECK (typeof(data) = 'blob'),
    CHECK (byte_len = length(data))
);
INSERT INTO art (id, sha256, byte_len, data)
    SELECT id, sha256, byte_len, data FROM art_hold_v4;
-- `art_hold_v4` is NOT dropped here: the `track_art` backfill below still needs
-- the three columns the new `art` gave up. It goes with the other holding
-- tables once every refill is done.

INSERT INTO tags (track_id, key, value, ordinal, value_blob)
    SELECT track_id, key, value, ordinal, value_blob FROM tags_hold_v4;
-- Read from `art_hold_v4`, not `art`: the rebuild above has already taken these
-- three columns off the real table, and the holding copy is the last place the
-- values exist.
--
-- LEFT JOIN, not JOIN: a link whose `art` row is missing is an orphan an
-- older foreign-keys-off writer could leave behind, and it must fail the
-- migration loudly on `mime`'s NOT NULL rather than be dropped on the floor by
-- an inner join. `depth`/`colors` start at 0, which is already what both the
-- format and synthesis take to mean unknown.
INSERT INTO track_art (track_id, art_id, picture_type, description,
                       mime, width, height, depth, colors, ordinal)
    SELECT h.track_id, h.art_id, h.picture_type, h.description,
           a.mime, a.width, a.height, 0, 0, h.ordinal
    FROM track_art_hold_v4 h LEFT JOIN art_hold_v4 a ON a.id = h.art_id;
-- `structural_blocks` is the one core table whose *shape* this migration would
-- otherwise leave alone, which is what left it the only one with affinity-only
-- columns once the other three gained storage classes (#732). Its rows are
-- already held and the table is already empty, so replacing it here costs a
-- drop and a create and no extra copy of anything -- which is the whole reason
-- it is worth doing in this release rather than buying a gated migration of its
-- own for it later.
DROP TABLE structural_blocks;
CREATE TABLE structural_blocks (
    track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    kind     TEXT NOT NULL,
    ordinal  INTEGER NOT NULL DEFAULT 0,
    body     BLOB NOT NULL,
    PRIMARY KEY (track_id, kind, ordinal),
    CHECK (typeof(track_id) = 'integer'),
    -- No typeof on `kind`: the IN list is strictly stronger, since no non-TEXT
    -- value compares equal to either name. Same call as `tracks.format`, which
    -- is the only other column in the schema whose values are enumerated.
    CHECK (kind IN ('STREAMINFO','SEEKTABLE')),
    CHECK (typeof(ordinal) = 'integer' AND ordinal >= 0),
    CHECK (typeof(body) = 'blob' AND length(body) <= 16777215)
);
INSERT INTO structural_blocks (track_id, kind, ordinal, body)
    SELECT track_id, kind, ordinal, body FROM structural_blocks_hold_v4;

DROP TABLE tracks_hold_v4;
DROP TABLE tags_hold_v4;
DROP TABLE track_art_hold_v4;
DROP TABLE structural_blocks_hold_v4;
DROP TABLE art_hold_v4;

-- The changelog ring is the one internal table whose payload never gained a
-- storage class: V1's `track_id INTEGER NOT NULL` accepts text, a real or a
-- blob, and the refresh reads the column straight into an i64 (#760). It is
-- recreated rather than migrated, because nothing in it is worth keeping: it is
-- derived state, this step is gated so no mount holds a watermark into it, and
-- a mount takes its watermark from whatever is there when it opens. Here, after
-- every refill and before the changelog triggers come back, nothing inserts
-- into it while it is gone. `track_changes_prune` is on the table, so it goes
-- with it and comes back with it; `seq` restarts at 1, which nothing depends on.
DROP TABLE track_changes;
CREATE TABLE track_changes (
    seq      INTEGER PRIMARY KEY AUTOINCREMENT,
    track_id INTEGER NOT NULL,
    CHECK (typeof(track_id) = 'integer')
);
CREATE TRIGGER track_changes_prune AFTER INSERT ON track_changes BEGIN
    DELETE FROM track_changes WHERE seq <= NEW.seq - 8192;
END;

-- 6. Recreate the indexes and the thirteen triggers the drops took with them,
-- plus what the new shapes add. Verbatim except where noted: `tracks_geometry_au`
-- gains `backing_ino` and a guarded ctime clause, `tracks_changelog_au` logs the
-- old id too, the two `_au`
-- bumps widen to both owners, and two reparent-refusal triggers and a rekey
-- refusal are new.
CREATE INDEX tracks_fingerprint_idx ON tracks(fingerprint);

-- The reverse art -> track_art edge, which went with the DROP TABLE above. Bulk
-- orphan-GC and the art delete trigger would otherwise scan the whole join
-- table per deleted row.
CREATE INDEX track_art_art_id_idx ON track_art(art_id);

-- `tags`' primary key, with the class folded in as a fourth column (#663). The
-- expression yields 0 or 1 and never NULL, so uniqueness is per class: two text
-- rows may not share (track_id, key, ordinal) and neither may two binary rows,
-- but one of each may -- which is the collision an external writer rewriting a
-- single class could otherwise provoke.
--
-- One index rather than the two partial ones #663 sketched, because a partial
-- index can only serve a query whose WHERE implies its predicate. Every Rust
-- reader constrains `value_blob`, but `tags_for_track` in the `contrib` helpers
-- deliberately does not -- it reads both classes at once -- and against two
-- partial indexes that query plans as `SCAN tags` plus a temp B-tree for the
-- ORDER BY, where the primary key used to serve it. `track_id` leading here
-- keeps that query on an index.
CREATE UNIQUE INDEX tags_ordinal_idx
    ON tags(track_id, key, ordinal, (value_blob IS NULL));

CREATE TRIGGER tracks_changelog_ai AFTER INSERT ON tracks BEGIN
    INSERT INTO track_changes (track_id) VALUES (NEW.id);
END;
-- The old id first, and the new one only when it differs (#762). A rekey is
-- refused below, but the refresh only removes an id the log names, so logging
-- `NEW.id` alone left a ghost for the old id in the live tree against any
-- writer that got past the refusal. The second insert is conditional so an
-- ordinary update still spends one ring slot rather than two.
CREATE TRIGGER tracks_changelog_au AFTER UPDATE ON tracks BEGIN
    INSERT INTO track_changes (track_id) VALUES (OLD.id);
    INSERT INTO track_changes (track_id) SELECT NEW.id WHERE NEW.id <> OLD.id;
END;
CREATE TRIGGER tracks_changelog_ad AFTER DELETE ON tracks BEGIN
    INSERT INTO track_changes (track_id) VALUES (OLD.id);
END;

-- The one non-verbatim recreation: `backing_ino` joins the geometry set. A
-- changed inode means the backing file was replaced, which is the whole reason
-- the column exists, so it belongs in the WHEN guard that keeps
-- content_version a true superset of served-byte inputs. That includes the
-- sentinel-to-real transition a revalidate performs on a migrated row: nothing
-- about the served bytes changed, but the row's identity now covers a field it
-- did not, so invalidating once is the conservative call.
--
-- A changed `backing_ctime_ns` bumps too, unless a checksum proves the bytes
-- unchanged: a fingerprint or content hash that was stored before and is stored
-- again unchanged by the same statement. A same-size rewrite that puts its old
-- mtime back (`touch -r`) changes ctime and nothing else a stamp records, and
-- without the bump it kept serving its old `content_version`, so the served
-- mtime held still and a kernel page cache kept what it had. ctime alone cannot
-- decide it, because a chmod moves ctime as well, and bumping for every such
-- re-probe is the served-mtime churn #757 removed. A first fingerprint proves
-- nothing about the bytes before it. A statement that leaves both checksums
-- alone is taken at its word that they still hold, which is what
-- `ChecksumWrite::Keep` means, and why the scanner writes a stamp and its
-- checksums in one statement: a trigger sees only the statement that fired it.
CREATE TRIGGER tracks_geometry_au
AFTER UPDATE ON tracks
WHEN NEW.format        <> OLD.format
  OR NEW.audio_offset  <> OLD.audio_offset
  OR NEW.audio_length  <> OLD.audio_length
  OR NEW.backing_size  <> OLD.backing_size
  OR NEW.backing_mtime_ns <> OLD.backing_mtime_ns
  OR NEW.backing_ino   <> OLD.backing_ino
  OR (NEW.backing_ctime_ns <> OLD.backing_ctime_ns
      AND NOT (OLD.fingerprint IS NOT NULL AND NEW.fingerprint IS OLD.fingerprint)
      AND NOT (OLD.content_hash IS NOT NULL AND NEW.content_hash IS OLD.content_hash))
BEGIN
    UPDATE tracks SET content_version = content_version + 1 WHERE id = NEW.id;
END;

CREATE TRIGGER tags_ai AFTER INSERT ON tags BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = NEW.track_id;
END;
-- Both owners, not just the new one (#717). With the refusal below in place the
-- two are always equal and the set collapses to one row, so this costs a single
-- identifier in a statement that already runs. The point is that the
-- invalidation trigger is correct on its own terms rather than correct only
-- because something else forbids the case it mishandles -- which matters
-- against a writer that drops triggers through `writable_schema`, a shape this
-- store's threat model already contemplates.
CREATE TRIGGER tags_au AFTER UPDATE ON tags BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id IN (OLD.track_id, NEW.track_id);
END;
CREATE TRIGGER tags_ad AFTER DELETE ON tags BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = OLD.track_id;
END;

CREATE TRIGGER track_art_ai AFTER INSERT ON track_art BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = NEW.track_id;
END;
CREATE TRIGGER track_art_au AFTER UPDATE ON track_art BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id IN (OLD.track_id, NEW.track_id);
END;
CREATE TRIGGER track_art_ad AFTER DELETE ON track_art BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = OLD.track_id;
END;

CREATE TRIGGER structural_blocks_ai AFTER INSERT ON structural_blocks BEGIN
    UPDATE tracks SET content_version = content_version + 1 WHERE id = NEW.track_id;
END;
CREATE TRIGGER structural_blocks_ad AFTER DELETE ON structural_blocks BEGIN
    UPDATE tracks SET content_version = content_version + 1 WHERE id = OLD.track_id;
END;
-- V1 shipped only the two triggers above, reasoning that the owned writer
-- replaces by DELETE-then-INSERT so no UPDATE path exists, and that the
-- resulting over-bump on a byte-identical re-probe is harmless churn. Neither
-- holds any more. SQL has an UPDATE path whatever musefs does: rewriting `body`
-- changed a served FLAC-header input without bumping `content_version`, and
-- changing `track_id` moved one between tracks without bumping either owner, so
-- a cached layout kept serving the old header (#759). And a bump is no longer
-- invisible churn: the served mtime derives from `content_version` (#725),
-- which is why the owned writer now leaves an identical set alone (#757).
--
-- So an in-place update is refused outright -- no WHEN guard, since there is no
-- legitimate one to let through -- the way art content (#719) and row ownership
-- (#717) already are. The AFTER UPDATE bump covers both owners anyway, so the
-- invalidation stays correct against a writer that drops the refusal through
-- `writable_schema`.
CREATE TRIGGER structural_blocks_au AFTER UPDATE ON structural_blocks BEGIN
    UPDATE tracks SET content_version = content_version + 1
    WHERE id IN (OLD.track_id, NEW.track_id);
END;
CREATE TRIGGER structural_blocks_reject_update
BEFORE UPDATE ON structural_blocks
BEGIN
    SELECT RAISE(ABORT,
        'structural_blocks rows are immutable; delete the row and insert its replacement');
END;

CREATE TRIGGER art_ad AFTER DELETE ON art BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id IN (SELECT track_id FROM track_art WHERE art_id = OLD.id);
END;
-- `art` rows are content-addressed, so their content columns are immutable. The
-- DROP TABLE above took this trigger with it; it comes back with `id` in the
-- guard (#719). Changing the key changes none of the content columns, so the
-- old WHEN clause was false and the trigger never fired -- and under the
-- foreign-keys-off writer this store already defends against, that silently
-- orphaned every link while `art_ad` (AFTER DELETE only) never saw an id
-- change, so nothing bumped `content_version` and a cached layout kept serving.
-- `<>` on id rather than IS NOT: it is the rowid alias and cannot be NULL.
CREATE TRIGGER art_reject_content_update
BEFORE UPDATE ON art
WHEN NEW.id     <> OLD.id
  OR NEW.data   <> OLD.data
  OR NEW.sha256 <> OLD.sha256
  OR NEW.byte_len <> OLD.byte_len
BEGIN
    SELECT RAISE(ABORT,
        'art rows are immutable; insert a new content-addressed row and relink via track_art');
END;

-- Row ownership is immutable (#717), matching what `art_reject_content_update`
-- already says about art content. Reparenting a row is not one edit to one
-- thing -- two tracks change -- and an `AFTER` trigger that has to enumerate
-- everything needing invalidation fails silently by serving stale bytes when it
-- gets that wrong, while a `BEFORE` refusal fails loudly at the write. No
-- writer needs it: both `contrib` helpers already replace by delete-then-insert.
--
-- `<>` rather than `IS NOT` because `track_id` is NOT NULL, matching the
-- convention `art_reject_content_update` states. The WHEN guard is load-bearing:
-- `BEFORE UPDATE OF track_id` fires whenever the column appears in a SET list,
-- so without it a writer rewriting a row wholesale without moving it would be
-- refused.
CREATE TRIGGER tags_reject_reparent
BEFORE UPDATE OF track_id ON tags
WHEN NEW.track_id <> OLD.track_id
BEGIN
    SELECT RAISE(ABORT,
        'tag ownership is immutable; delete the row and insert it under the new track');
END;
CREATE TRIGGER track_art_reject_reparent
BEFORE UPDATE OF track_id ON track_art
WHEN NEW.track_id <> OLD.track_id
BEGIN
    SELECT RAISE(ABORT,
        'art link ownership is immutable; delete the row and insert it under the new track');
END;

-- A track's id is immutable for the same reason (#762). The incremental refresh
-- keys on it -- which is why it is AUTOINCREMENT and never handed back out
-- (#678) -- and foreign keys do not protect it: a childless track has nothing
-- referencing its old id, so it could be rekeyed freely, onto a deleted id
-- included. The WHEN guard is load-bearing, as it is for the reparent refusals.
CREATE TRIGGER tracks_reject_rekey
BEFORE UPDATE OF id ON tracks
WHEN NEW.id <> OLD.id
BEGIN
    SELECT RAISE(ABORT, 'track ids are immutable; delete the row and insert a new one');
END;

PRAGMA user_version = 4;
"""

USER_VERSION = 4

# Byte cap on `tags.value`, mirrored so an external writer can check a
# value before the `CHECK` does. Generated from the Rust constant: it
# moved once already (#644) and a hand-kept copy would silently rot.
MAX_TAG_VALUE_LEN = 16777215
