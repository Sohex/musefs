# GENERATED from python-musefs/src/musefs_common/schema.py — do not edit.
# Run contrib/python-musefs/vendor_to_picard.py after changing the library.
#
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
-- TEXT counts characters, so the V1 `CHECK (length(value) <= 262144)` was up to
-- ~4x looser than the documented 256 KiB byte bound; length(CAST(value AS BLOB))
-- counts bytes. SQLite cannot alter a CHECK in place, so recreate the table
-- (V2 is unreleased — this is folded in rather than added as a new migration).
-- Pre-existing over-cap rows (only reachable on an upgraded store) are dropped:
-- the read-time guard already counts bytes, so they were unreadable anyway, and
-- carrying them would abort the rebuild on the new CHECK.
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
    CHECK (length(CAST(value AS BLOB)) <= 262144),
    CHECK (value_blob IS NULL OR length(value_blob) <= 16711680)
);
INSERT INTO tags_new (track_id, key, value, ordinal, value_blob)
    SELECT track_id, key, value, ordinal, value_blob FROM tags
    WHERE length(CAST(value AS BLOB)) <= 262144;
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
-- reinterpreted. The next `scan` or `revalidate` recomputes them: revalidate
-- already re-probes a row missing the checksum its tier asks for, so no new
-- backfill machinery is needed. `content_hash` is untouched: it is a full-file
-- SHA-256 and its meaning has not changed.
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
    -- and each scan arms the guard for the rows it touches.
    backing_ino      INTEGER NOT NULL DEFAULT 0,
    CHECK (typeof(backing_path) = 'blob'
           AND length(backing_path) > 0
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
    CHECK (typeof(backing_ino) = 'integer' AND backing_ino >= 0),
    CHECK (typeof(content_version) = 'integer' AND content_version >= 0),
    CHECK (typeof(updated_at) = 'integer' AND updated_at >= 0),
    CHECK (audio_offset + audio_length <= backing_size),
    -- instr(..., char(0)) = 0 alongside the character cap (#693): SQLite's
    -- length() on TEXT stops at the first NUL, so `<64 hex chars>` + NUL +
    -- anything satisfied a bare length() = 64 while storing something else
    -- entirely. Banning NUL keeps the documented `64 characters` meaning rather
    -- than quietly converting the field to a byte cap.
    CHECK (fingerprint IS NULL
           OR (typeof(fingerprint) = 'text'
               AND length(fingerprint) = 64
               AND instr(fingerprint, char(0)) = 0)),
    CHECK (content_hash IS NULL
           OR (typeof(content_hash) = 'text'
               AND length(content_hash) = 64
               AND instr(content_hash, char(0)) = 0))
);

-- 4. Refill. CAST(backing_path AS BLOB) is what preserves identity across the
-- type change: the bytes are unchanged, and without the cast every existing row
-- would be unreachable to a byte-binding reader while the unique index failed
-- to fire, silently giving each track a second row.
--
-- `fingerprint` is dropped on the floor here -- that is #691's reset, folded in.
--
-- `content_hash` is sanitized rather than carried blindly. It is a
-- scanner-owned derived column that the next scan recomputes, which is exactly
-- the case the sanitize-only-under-a-flag policy carves out: nulling one costs
-- a rescan, while carrying a value the new CHECK rejects would abort the whole
-- upgrade over a column that rebuilds itself. The other tightened columns are
-- NOT sanitized here -- they are either structural or NOT NULL, so a row that
-- violates them fails the migration, which is what the row-rejection pre-flight
-- and its repair flag exist to report ahead of time.
INSERT INTO tracks (id, backing_path, format, audio_offset, audio_length,
                    backing_size, backing_mtime_ns, content_version, updated_at,
                    backing_ctime_ns, fingerprint, content_hash, backing_ino)
    SELECT id, CAST(backing_path AS BLOB), format, audio_offset, audio_length,
           backing_size, backing_mtime_ns, content_version, updated_at,
           backing_ctime_ns,
           NULL,
           CASE WHEN typeof(content_hash) = 'text'
                     AND length(content_hash) = 64
                     AND instr(content_hash, char(0)) = 0
                THEN content_hash END,
           0
    FROM tracks_hold_v4;

INSERT INTO tags (track_id, key, value, ordinal, value_blob)
    SELECT track_id, key, value, ordinal, value_blob FROM tags_hold_v4;
INSERT INTO track_art (track_id, art_id, picture_type, description, ordinal)
    SELECT track_id, art_id, picture_type, description, ordinal
    FROM track_art_hold_v4;
INSERT INTO structural_blocks (track_id, kind, ordinal, body)
    SELECT track_id, kind, ordinal, body FROM structural_blocks_hold_v4;

DROP TABLE tracks_hold_v4;
DROP TABLE tags_hold_v4;
DROP TABLE track_art_hold_v4;
DROP TABLE structural_blocks_hold_v4;

-- 5. DROP TABLE tracks took its index and its four triggers with it. Recreate
-- the index, and all thirteen triggers verbatim -- with one deliberate
-- exception, noted on tracks_geometry_au below.
CREATE INDEX tracks_fingerprint_idx ON tracks(fingerprint);

CREATE TRIGGER tracks_changelog_ai AFTER INSERT ON tracks BEGIN
    INSERT INTO track_changes (track_id) VALUES (NEW.id);
END;
CREATE TRIGGER tracks_changelog_au AFTER UPDATE ON tracks BEGIN
    INSERT INTO track_changes (track_id) VALUES (NEW.id);
END;
CREATE TRIGGER tracks_changelog_ad AFTER DELETE ON tracks BEGIN
    INSERT INTO track_changes (track_id) VALUES (OLD.id);
END;

-- The one non-verbatim recreation: `backing_ino` joins the geometry set. A
-- changed inode means the backing file was replaced, which is the whole reason
-- the column exists, so it belongs in the WHEN guard that keeps
-- content_version a true superset of served-byte inputs. Inert until the Rust
-- half starts writing the column, since every row is the 0 sentinel until then.
CREATE TRIGGER tracks_geometry_au
AFTER UPDATE ON tracks
WHEN NEW.format        <> OLD.format
  OR NEW.audio_offset  <> OLD.audio_offset
  OR NEW.audio_length  <> OLD.audio_length
  OR NEW.backing_size  <> OLD.backing_size
  OR NEW.backing_mtime_ns <> OLD.backing_mtime_ns
  OR NEW.backing_ino   <> OLD.backing_ino
BEGIN
    UPDATE tracks SET content_version = content_version + 1 WHERE id = NEW.id;
END;

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

CREATE TRIGGER structural_blocks_ai AFTER INSERT ON structural_blocks BEGIN
    UPDATE tracks SET content_version = content_version + 1 WHERE id = NEW.track_id;
END;
CREATE TRIGGER structural_blocks_ad AFTER DELETE ON structural_blocks BEGIN
    UPDATE tracks SET content_version = content_version + 1 WHERE id = OLD.track_id;
END;

CREATE TRIGGER art_ad AFTER DELETE ON art BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id IN (SELECT track_id FROM track_art WHERE art_id = OLD.id);
END;
PRAGMA user_version = 4;
"""

USER_VERSION = 4

# Byte cap on `tags.value`, mirrored so an external writer can check a
# value before the `CHECK` does. Generated from the Rust constant: it
# moved once already (#644) and a hand-kept copy would silently rot.
MAX_TAG_VALUE_LEN = 16777215
