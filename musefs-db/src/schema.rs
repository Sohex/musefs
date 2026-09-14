use crate::Result;
use rusqlite::{Connection, TransactionBehavior};

const MIGRATION_V1: &str = r"
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
";

const MIGRATION_V2: &str = r"
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
";

const MIGRATION_V3: &str = r"
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
";

const MIGRATION_V4: &str = r"
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

-- 5. Rebuild the three child tables. All are empty right now -- the cascade
-- above took them -- so each is a drop and a create, with the holding tables as
-- the source. `tags` and `track_art` change shape; `structural_blocks` keeps
-- its columns and gains only the storage classes every other table now pins.

-- `tags` loses its primary key in favour of two partial unique indexes split on
-- `value_blob IS NULL` (#663). The PK numbered a track's text rows and its
-- binary rows in one ordinal space per key, and an external writer that
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
-- per-embedding ones were destroyed at ingest and come back on a rescan, which
-- is what `musefs migrate`'s rescan offer is for. `depth` and `colors` have no
-- shared value to copy and start at 0, which is what both the format and
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
    CHECK (typeof(sha256) = 'text'
           AND length(sha256) = 64
           AND instr(sha256, char(0)) = 0),
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

-- 6. Recreate the indexes and the thirteen triggers the drops took with them,
-- plus what the new shapes add. Verbatim except where noted: `tracks_geometry_au`
-- gains `backing_ino`, `tracks_changelog_au` logs the old id too, the two `_au`
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

";

/// Ring capacity of the `track_changes` changelog. Must match the literal in
/// MIGRATION_V1 (guarded by `changelog_cap_constant_matches_migration_sql`).
#[allow(dead_code)]
pub const CHANGELOG_CAP: i64 = 8192;

/// Whether an ordinary open of the store may apply a migration.
///
/// Every migration up to and including 1.3.0 was transparent: opening the store
/// applied it, and nobody running `mount` or `scan` learned it had happened.
/// That is the right behaviour for a step whose cost and consequences the user
/// would not notice, and the wrong one for a step that rewrites every row,
/// transiently needs the store's size again in free disk, or ends compatibility
/// with the binary they were running yesterday. A gated step is refused on open
/// and applied only by `musefs migrate`, which says what it is about to do
/// before it does it (#706).
///
/// The binary owns this classification rather than the store, so a user jumping
/// from 1.2 straight to 2.1 is still gated on the step that needs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gate {
    /// Applied as a side effect of any open, exactly as before.
    Transparent,
    /// Applied only by `musefs migrate`.
    Gated,
}

/// One numbered schema step: the SQL, whether an ordinary open may run it, the
/// musefs release that introduced it, and a line saying what it does.
///
/// Neither `since` nor `summary` is decoration. `musefs migrate` has to tell
/// the user what it is about to do to their store and which upgrade brought it,
/// and the only place either can be kept honest is next to the SQL (#705).
/// `since` is also what [`Migration::new`] checks the gated-migration contract
/// against.
struct Migration {
    sql: &'static str,
    gate: Gate,
    since: &'static str,
    summary: &'static str,
}

impl Migration {
    /// Declare a migration, enforcing the contract below.
    ///
    /// **A gated step may only be introduced by a major release.** The
    /// classification decides whether someone's upgrade is a restart or an
    /// errand, so what they need to know is not which `user_version` they are
    /// on but whether the release they are moving to crossed a major boundary.
    /// Under semver a major is where an incompatible change is allowed to live,
    /// and a gated migration — data rewritten, disk needed, older binaries
    /// locked out — is exactly that. Tying the two together gives one rule that
    /// holds for every upgrade anyone ever does: crossing a major version may
    /// ask for `musefs migrate`; a minor or a patch never will.
    ///
    /// The converse is deliberately *not* checked. A major release is free to
    /// carry only transparent steps, or none — `MIGRATION_V3` rides 2.0.0 and
    /// is transparent — and a release with nothing to gate should not have to
    /// invent something.
    ///
    /// Every entry in `MIGRATIONS` is built here, in a `const` context, so this
    /// is checked by the compiler rather than by review. That matters because
    /// the failure mode is silent: a gated step slipped into a point release is
    /// a schema change nobody was warned about, and it would be discovered by
    /// the mounts that stopped coming back after an unattended upgrade.
    const fn new(
        sql: &'static str,
        gate: Gate,
        since: &'static str,
        summary: &'static str,
    ) -> Migration {
        assert!(
            !gate.is_gated() || is_major_release(since),
            "a gated migration may only be introduced by a major release (x.0.0): \
             move it to the next major, or make it transparent"
        );
        Migration {
            sql,
            gate,
            since,
            summary,
        }
    }
}

/// Every migration's SQL, for the few tests that need one step's text rather
/// than a store built out of them.
#[cfg(test)]
pub(crate) fn migration_sql() -> Vec<&'static str> {
    MIGRATIONS.iter().map(|m| m.sql).collect()
}

/// Build a store at a released schema version — what an older musefs build
/// would have left behind — by running that version's migrations for real.
///
/// The obvious shortcut is to create a current store and rewind its
/// `user_version`. That does not produce a store at the older version: V4
/// rebuilds tables by reading the old shape column by column, so a rewound
/// store is one whose `art` has already given up columns the rebuild selects,
/// and the migration fails on the missing column. It has to be built.
///
/// WAL is set here because every other open of a musefs store leaves it that
/// way, and the exclusive-claim check `musefs migrate` runs reads other
/// connections' shared marks, which exist only in that mode.
///
/// Test scaffolding, so it is compiled only for this crate's own tests and
/// under the `test-support` feature, which the integration tests here and in
/// `musefs-cli` switch on through a dev-dependency (#751). One definition serves
/// them all.
#[cfg(any(test, feature = "test-support"))]
pub fn seed_store_at_version(path: &std::path::Path, version: i64) -> rusqlite::Result<()> {
    let conn = Connection::open(path)?;
    let _: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    let upto = usize::try_from(version).expect("a schema version fits a usize");
    for (target, migration) in (1i64..).zip(MIGRATIONS).take(upto) {
        conn.execute_batch(migration.sql)?;
        conn.pragma_update(None, "user_version", target)?;
    }
    Ok(())
}

const MIGRATIONS: &[Migration] = &[
    Migration::new(
        MIGRATION_V1,
        Gate::Transparent,
        "1.0.0",
        "creates the baseline schema",
    ),
    Migration::new(
        MIGRATION_V2,
        Gate::Transparent,
        "1.1.0",
        "adds the scanner-owned fingerprint and content_hash columns",
    ),
    Migration::new(
        MIGRATION_V3,
        Gate::Transparent,
        "2.0.0",
        "widens the tags.value and track_art.description caps",
    ),
    // The 2.0.0 store change. It rewrites data the user did not ask to have
    // rewritten and ends compatibility with every older musefs build, which is
    // more than anyone running `mount` can reasonably expect (#705).
    Migration::new(
        MIGRATION_V4,
        Gate::Gated,
        "2.0.0",
        "clears every stored fingerprint and content hash; a revalidate recomputes them",
    ),
];

impl Gate {
    const fn is_gated(self) -> bool {
        matches!(self, Gate::Gated)
    }
}

/// Whether `version` is a major release, i.e. the minor and patch of its semver
/// core are both 0. A pre-release or build-metadata suffix is allowed and
/// ignored, so `2.0.0-rc.1` is a major release and `2.1.0-rc.0.0` is not.
///
/// Parses the core rather than matching a suffix, which is the difference
/// between the two examples above: a `.0.0` anywhere in the string is not a
/// major release, and this gate is the only thing standing between a gated
/// migration and a point release. Written by hand because no semver parser is
/// available in a const context.
const fn is_major_release(version: &str) -> bool {
    let b = version.as_bytes();
    let mut i = 0;
    // Major: one or more digits.
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 || i == b.len() || b[i] != b'.' {
        return false;
    }
    i += 1;
    // Minor and patch: each exactly the single digit `0`, separated by a dot.
    if i == b.len() || b[i] != b'0' {
        return false;
    }
    i += 1;
    if i == b.len() || b[i] != b'.' {
        return false;
    }
    i += 1;
    if i == b.len() || b[i] != b'0' {
        return false;
    }
    i += 1;
    // The core ends here: end of string, or the start of a pre-release or
    // build-metadata suffix. Anything else is a longer number or a fourth part.
    i == b.len() || b[i] == b'-' || b[i] == b'+'
}

/// One step a store has yet to receive, as [`pending`] reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PendingStep {
    /// The `user_version` the store carries once this step has been applied.
    pub version: i64,
    /// The musefs release that introduced it.
    pub since: &'static str,
    /// What the step does, in one line fit to print.
    pub summary: &'static str,
    /// Whether an ordinary open refuses this step rather than applying it.
    /// True only for a step a major release introduced — see the contract on
    /// `MIGRATIONS`.
    pub gated: bool,
}

/// Every step between `current` and [`LATEST_VERSION`], in the order they run.
/// Empty for a store already at the latest version, and for one past it.
pub fn pending(current: i64) -> Vec<PendingStep> {
    (1i64..)
        .zip(MIGRATIONS)
        .filter(|(version, _)| *version > current)
        .map(|(version, migration)| PendingStep {
            version,
            since: migration.since,
            summary: migration.summary,
            gated: migration.gate.is_gated(),
        })
        .collect()
}

/// The `user_version` a fully-migrated store carries. Exported so callers and
/// tests assert "the latest schema" rather than a literal that has to be chased
/// through every test file each time a migration is appended.
pub const LATEST_VERSION: i64 = 4;
const _: () = assert!(
    MIGRATIONS.len() == 4,
    "LATEST_VERSION must match MIGRATIONS"
);

#[cfg(test)]
thread_local! {
    static BEFORE_LOCK_HOOK: std::cell::RefCell<Option<Box<dyn FnMut()>>> =
        const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
fn fire_before_lock() {
    // Take the hook out before running it: it opens a second connection and
    // migrates, which re-enters `migrate` on this same thread and would
    // otherwise recurse until the stack gives out.
    let hook = BEFORE_LOCK_HOOK.with(|h| h.borrow_mut().take());
    if let Some(mut f) = hook {
        f();
    }
}
#[cfg(test)]
fn set_before_lock_hook(f: impl FnMut() + 'static) {
    BEFORE_LOCK_HOOK.with(|h| *h.borrow_mut() = Some(Box::new(f)));
}
#[cfg(test)]
fn clear_before_lock_hook() {
    BEFORE_LOCK_HOOK.with(|h| *h.borrow_mut() = None);
}

/// Whether a run of the schema runner may apply gated steps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GatePolicy {
    /// Stop at the first gated step and refuse. Every open of the store takes
    /// this path.
    Enforce,
    /// Apply every pending step. Only `musefs migrate` takes this path, after
    /// the user has been told what the gated step does and has agreed to it.
    Bypass,
}

/// The highest version a run under `policy` may take a store sitting at
/// `current`, which is `current` itself whenever any step still to come is
/// gated. A transparent step ahead of a gate is held back with it: every step
/// bumps `user_version`, which the previous release refuses, so applying one on
/// the way to a refusal would lock that release out with no snapshot taken
/// (#749). It waits for `musefs migrate`, which applies the lot.
///
/// A store at version 0 is one this binary is *creating*: it has no data to
/// endanger, and gating it would stop `scan` from ever building a new library.
/// That is the same "creating versus upgrading" distinction the announcement
/// below already draws for its log level (#706).
fn reachable(current: i64, policy: GatePolicy) -> i64 {
    reachable_in(MIGRATIONS, current, policy)
}

/// [`reachable`] over any migration table, so the rule can be tested against a
/// shape this build does not ship yet: a transparent step after an applied
/// gate, which the first migration after 2.0.0 will be.
fn reachable_in(migrations: &[Migration], current: i64, policy: GatePolicy) -> i64 {
    let latest = i64::try_from(migrations.len()).expect("a migration count fits an i64");
    if current == 0 || policy == GatePolicy::Bypass {
        return latest;
    }
    for (target, migration) in (1i64..).zip(migrations) {
        if target > current && migration.gate.is_gated() {
            return current;
        }
    }
    latest
}

/// The refusal a gated step raises, naming the version reached and the command
/// that finishes the job.
fn gated(found: i64) -> crate::error::DbError {
    crate::error::DbError::StoreNeedsMigration {
        found,
        target: LATEST_VERSION,
    }
}

/// Bring the store up to the latest version it may transparently reach, and
/// refuse if a gated step stands between that and [`LATEST_VERSION`].
pub fn migrate(conn: &mut Connection) -> Result<()> {
    run(conn, GatePolicy::Enforce)
}

/// Bring the store all the way to [`LATEST_VERSION`], gated steps included.
///
/// The intended caller is `musefs migrate` and nothing else: a gated step is
/// gated because it does something the user has to be told about first, and
/// this function is the point at which they already have been.
pub fn migrate_all(conn: &mut Connection) -> Result<()> {
    run(conn, GatePolicy::Bypass)
}

fn run(conn: &mut Connection, policy: GatePolicy) -> Result<()> {
    let latest = LATEST_VERSION;
    let current = conn.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))?;
    // A store at a user_version past anything this binary knows about was written
    // by a newer (or third-party) tool that bumped the schema. Refuse it loudly
    // rather than treating it as already-migrated and silently misreading the
    // external-writer contract. Distinct from the gated refusal below, and with
    // the opposite remedy: upgrade the binary, not the store. Repeated under the
    // write lock, which is where the decision actually binds; this one only
    // saves taking the lock in the common case.
    if current > latest {
        return Err(crate::error::DbError::StoreTooNew {
            found: current,
            supported: latest,
        });
    }
    // Fast path: already at the latest version, no transaction needed.
    if current >= latest {
        return Ok(());
    }
    // Test seam: the window between the read above and the write lock below is
    // exactly where a competing writer can migrate the store out from under us.
    // Nothing in a single-process test can land there on its own, so the race
    // arm of the announcement guard below is only reachable through this hook.
    #[cfg(test)]
    fire_before_lock();
    // Use an IMMEDIATE transaction so the write lock is acquired up front. The
    // user_version read below is then authoritative: a second process opening
    // the same database concurrently blocks here until the first commits, then
    // sees the updated version and skips re-applying the migration.
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
    // Both refusals are decided from the version read under the lock, not the
    // one read before it. A newer binary can commit a version past `latest`
    // while we wait here, and `reachable` would then find nothing left to apply
    // and report a successful migration of a store this build cannot read.
    if current > latest {
        return Err(crate::error::DbError::StoreTooNew {
            found: current,
            supported: latest,
        });
    }
    // The version this run will actually reach: short of `latest` when a gated
    // step stops it. Both the gate decision and the announcement are taken from
    // the version read under the lock, so neither can be decided on a reading
    // another process has already invalidated — a `mount` racing `musefs
    // migrate` waits here and then finds the store current, rather than being
    // refused for a gate that is no longer there.
    let stop = reachable(current, policy);
    let mut work = None;
    // Announce only an upgrade this call actually performs: another process may
    // have migrated the store while we waited for the write lock, in which case
    // the version read under the lock is already `stop` and the loop below
    // applies nothing.
    if current < stop {
        // `Connection::path` is `Some("")` for an in-memory or temporary store.
        let at = match tx.path().filter(|p| !p.is_empty()) {
            Some(path) => format!(" at {path}"),
            None => String::new(),
        };
        if current == 0 {
            // A store this binary is creating from scratch — nothing is being
            // taken anywhere it cannot come back from, so this stays quiet at
            // the default filter.
            log::info!("creating store schema{at} at version {stop}");
        } else {
            // A pre-existing store is about to be rewritten in place, one way.
            // The user gets this once per store, and wants it in their
            // scrollback if they ever try to roll musefs back.
            log::warn!(
                "upgrading store schema{at} from version {current} to version {stop}; \
                 this is irreversible and the store will no longer open with musefs \
                 builds older than this one"
            );
        }
        work = Some((at, std::time::Instant::now()));
    }
    for (target, migration) in (1i64..).zip(MIGRATIONS) {
        if current < target && target <= stop {
            tx.execute_batch(migration.sql)?;
            tx.pragma_update(None, "user_version", target)?;
        }
    }
    tx.commit()?;
    if let Some((at, started)) = work {
        let secs = started.elapsed().as_secs_f64();
        log::info!("store schema{at} is now at version {stop} (took {secs:.1}s)");
    }
    // A pending gated step held every step back (`reachable` answered
    // `current`), so the refusal leaves the store exactly as it was (#749).
    if stop < latest {
        return Err(gated(stop));
    }
    Ok(())
}

#[cfg(test)]
mod migration_logging_tests {
    use log::{Level, LevelFilter, Log, Metadata, Record};
    use rusqlite::Connection;
    use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
    use std::thread::ThreadId;

    /// Records emitted on the capturing thread, newest last.
    static RECORDS: Mutex<Vec<(Level, String)>> = Mutex::new(Vec::new());
    /// Serializes the capturing tests: `log::set_logger` installs one logger per
    /// process, so they share a single buffer.
    static SERIAL: Mutex<()> = Mutex::new(());
    /// The thread whose records are being captured. Everything the rest of the
    /// (parallel) test binary logs is dropped, so a capture only ever sees what
    /// the test itself provoked.
    static CAPTURING: Mutex<Option<ThreadId>> = Mutex::new(None);

    struct Capture;
    static CAPTURE: Capture = Capture;

    impl Log for Capture {
        fn enabled(&self, _: &Metadata) -> bool {
            true
        }
        fn log(&self, record: &Record) {
            if *CAPTURING.lock().unwrap() == Some(std::thread::current().id()) {
                RECORDS
                    .lock()
                    .unwrap()
                    .push((record.level(), record.args().to_string()));
            }
        }
        fn flush(&self) {}
    }

    /// Holds the capture open; the buffer is reachable only through the guard,
    /// so no test can read records it does not own.
    struct Captured {
        records: &'static Mutex<Vec<(Level, String)>>,
        _serial: MutexGuard<'static, ()>,
    }

    impl Captured {
        fn records(&self) -> Vec<(Level, String)> {
            self.records.lock().unwrap().clone()
        }
        /// Drop everything logged so far, so the next assertion sees only what
        /// follows this call.
        fn clear(&self) {
            self.records.lock().unwrap().clear();
        }
    }

    impl Drop for Captured {
        fn drop(&mut self) {
            *CAPTURING.lock().unwrap() = None;
            RECORDS.lock().unwrap().clear();
        }
    }

    /// Start capturing this thread's log records until the returned guard drops.
    fn capture() -> Captured {
        let serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        static INIT: OnceLock<()> = OnceLock::new();
        INIT.get_or_init(|| {
            log::set_logger(&CAPTURE).expect("no other logger is installed in this test binary");
            log::set_max_level(LevelFilter::Trace);
        });
        RECORDS.lock().unwrap().clear();
        *CAPTURING.lock().unwrap() = Some(std::thread::current().id());
        Captured {
            records: &RECORDS,
            _serial: serial,
        }
    }

    /// A store stamped at v1 with only MIGRATION_V1 applied — what an older
    /// musefs build left behind.
    fn store_at_v1(conn: &Connection) {
        conn.execute_batch(super::MIGRATION_V1).unwrap();
        conn.pragma_update(None, "user_version", 1i64).unwrap();
    }

    /// The announcement fires only for an upgrade this call actually performs.
    /// Another writer can migrate the store while we wait for the write lock, in
    /// which case the version read *under* the lock is already the latest and
    /// the loop applies nothing — announcing there would report an irreversible
    /// upgrade that this process did not do, on a store it did not change.
    ///
    /// Only reachable through the before-lock seam: the pre-lock read has to see
    /// an old version and the post-lock read a current one, which no
    /// single-threaded test can arrange otherwise.
    #[test]
    fn a_migration_lost_to_a_competing_writer_says_nothing() {
        struct HookGuard;
        impl Drop for HookGuard {
            fn drop(&mut self) {
                super::clear_before_lock_hook();
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        let mut conn = Connection::open(&path).unwrap();
        store_at_v1(&conn);

        // Stand in for the competing process: migrate the store to the latest
        // version on a second connection, after our pre-lock read but before we
        // take the write lock.
        let racer_path = path.clone();
        let captured = capture();
        super::set_before_lock_hook(move || {
            let mut racer = Connection::open(&racer_path).unwrap();
            super::migrate_all(&mut racer).unwrap();
            // The racer re-enters `migrate` on this thread, so its own — entirely
            // correct — announcement lands in the same capture buffer. Drop it,
            // leaving only whatever the call that lost the race goes on to say.
            RECORDS
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clear();
        });
        let _guard = HookGuard;

        super::migrate(&mut conn).unwrap();
        let records = captured.records();

        assert!(
            records.is_empty(),
            "a call that migrated nothing must say nothing; got {records:?}"
        );
        // The store is still correctly migrated — by the racer, not by us.
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, super::LATEST_VERSION);
    }

    #[test]
    fn upgrading_an_existing_store_warns_with_both_versions() {
        let captured = capture();
        let mut conn = Connection::open_in_memory().unwrap();
        store_at_v1(&conn);
        captured.clear();

        super::migrate_all(&mut conn).unwrap();

        let records = captured.records();
        let warnings: Vec<&String> = records
            .iter()
            .filter(|(level, _)| *level == Level::Warn)
            .map(|(_, msg)| msg)
            .collect();
        assert_eq!(
            warnings.len(),
            1,
            "an in-place upgrade must announce itself exactly once at warn, \
             which is the default filter level; got {records:?}"
        );
        let warning = warnings[0];
        assert!(
            warning.contains("from version 1")
                && warning.contains(&format!("to version {}", super::LATEST_VERSION)),
            "the warning must name the version found and the version reached: {warning}"
        );
        assert!(
            records.iter().any(|(level, msg)| *level == Level::Info
                && msg.contains(&format!("version {}", super::LATEST_VERSION))),
            "a completion line must follow the upgrade: {records:?}"
        );
    }

    #[test]
    fn the_warning_names_the_store_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        {
            let conn = Connection::open(&path).unwrap();
            store_at_v1(&conn);
        }
        let captured = capture();
        let mut conn = Connection::open(&path).unwrap();
        super::migrate_all(&mut conn).unwrap();

        let records = captured.records();
        let path = path.to_str().unwrap();
        assert!(
            records
                .iter()
                .any(|(level, msg)| *level == Level::Warn && msg.contains(path)),
            "the upgrade warning must identify which store was upgraded: {records:?}"
        );
    }

    #[test]
    fn opening_an_already_current_store_logs_nothing() {
        let captured = capture();
        let mut conn = Connection::open_in_memory().unwrap();
        super::migrate(&mut conn).unwrap();
        captured.clear();

        // The fast path, taken on every open and every mount.
        super::migrate(&mut conn).unwrap();

        assert!(
            captured.records().is_empty(),
            "a store already at the latest version must stay silent: {:?}",
            captured.records()
        );
    }

    /// A store the gate refuses is left exactly as it was (#749), so the run must
    /// announce no upgrade. The warning is the user's record of an irreversible
    /// change, and nothing irreversible happened.
    #[test]
    fn a_gated_refusal_announces_no_upgrade() {
        let captured = capture();
        let mut conn = Connection::open_in_memory().unwrap();
        store_at_v1(&conn);
        captured.clear();

        super::migrate(&mut conn).expect_err("V4 is gated, so a V1 store cannot open");

        let records = captured.records();
        assert!(
            records.iter().all(|(level, _)| *level != Level::Warn),
            "nothing was applied, so no upgrade may be announced: {records:?}"
        );
        assert_eq!(
            conn.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
                .unwrap(),
            1,
            "and the store is still at the version it was"
        );
    }

    #[test]
    fn creating_a_fresh_store_does_not_warn() {
        let captured = capture();
        let mut conn = Connection::open_in_memory().unwrap();
        super::migrate(&mut conn).unwrap();

        let records = captured.records();
        assert!(
            records.iter().all(|(level, _)| *level != Level::Warn),
            "creating a store is not an irreversible upgrade of the user's data, \
             so nothing may reach the default filter: {records:?}"
        );
        assert!(
            records.iter().any(|(level, _)| *level == Level::Info),
            "creating a store should still be visible under -v: {records:?}"
        );
    }
}

#[cfg(test)]
mod gate_tests {
    use rusqlite::Connection;

    use super::{Gate, GatePolicy, LATEST_VERSION, MIGRATIONS};
    use crate::error::DbError;

    /// The version an ordinary open of an existing store can reach today: the
    /// step before the first gated one. Every literal below leans on this, and
    /// `the_gated_step_is_v4_and_nothing_before_it_is` is what keeps it honest.
    const WALL: i64 = 3;

    fn user_version(conn: &Connection) -> i64 {
        conn.pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap()
    }

    /// Apply migrations 1..=`upto` by hand and stamp the version, the way an
    /// older musefs build left the store behind.
    fn store_at(conn: &Connection, upto: i64) {
        for (target, migration) in (1i64..).zip(MIGRATIONS) {
            if target > upto {
                break;
            }
            conn.execute_batch(migration.sql).unwrap();
            conn.pragma_update(None, "user_version", target).unwrap();
        }
        assert_eq!(user_version(conn), upto);
    }

    /// The classification itself, stated once. Every other test here reads
    /// `WALL` as 3, which is only meaningful while V4 is the first gated step —
    /// so a change to the table has to come through this assertion first.
    #[test]
    fn the_gated_step_is_v4_and_nothing_before_it_is() {
        let gates: Vec<Gate> = MIGRATIONS.iter().map(|m| m.gate).collect();
        assert_eq!(
            gates,
            vec![
                Gate::Transparent,
                Gate::Transparent,
                Gate::Transparent,
                Gate::Gated
            ]
        );
    }

    /// The contract is enforced by a `const` assertion, which by construction
    /// no test can observe failing — a violation is a build error. What a test
    /// can pin is the predicate it rests on, so a rewrite of the parsing cannot
    /// quietly turn the assertion into one that accepts everything.
    #[test]
    fn only_an_x_0_0_version_counts_as_a_major_release() {
        for major in [
            "1.0.0",
            "2.0.0",
            "10.0.0",
            // A suffix describes the same core, so a release candidate for a
            // major is still a major.
            "2.0.0-rc.1",
            "2.0.0+build.7",
        ] {
            assert!(super::is_major_release(major), "{major}");
        }
        for not_major in [
            "1.1.0",
            "1.0.1",
            "0.2.0",
            "1.0.10",
            // The core is 2.1.0; only a suffix ends in `.0.0`. Matching the end
            // of the string rather than parsing the core would let this one
            // carry a gated migration into a minor release.
            "2.1.0-rc.0.0",
            "1.1.0+0.0",
            // Not three parts, or not a number where one belongs.
            "1.0.0.0",
            "1.00.0",
            "v1.0.0",
            // The digit run has to end *at* a dot. Without that the parse would
            // skip over the offending byte and land on a `0.0` that reads like
            // a minor and patch, which is the one way a non-version could pass.
            "1-0.0",
            "1x0.0",
            "2.0",
            "",
            "0.0",
            ".0.0",
        ] {
            assert!(!super::is_major_release(not_major), "{not_major}");
        }
    }

    /// Every step says which release brought it, and the one the user is being
    /// asked to run a command for says a major.
    #[test]
    fn every_step_names_its_release_and_the_gated_one_names_a_major() {
        for migration in MIGRATIONS {
            assert!(!migration.since.is_empty());
            assert!(!migration.summary.is_empty());
            if migration.gate.is_gated() {
                assert!(
                    super::is_major_release(migration.since),
                    "gated step from {}",
                    migration.since
                );
            }
        }
    }

    /// What `musefs migrate` prints comes from the table, so the report has to
    /// carry the release and the summary through, not just the version.
    #[test]
    fn pending_carries_the_release_and_the_summary() {
        let steps = super::pending(WALL);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].version, LATEST_VERSION);
        assert_eq!(steps[0].since, "2.0.0");
        assert!(steps[0].gated);
        assert!(super::pending(LATEST_VERSION).is_empty());
        // A store older than the wall sees the transparent steps too.
        assert_eq!(super::pending(1).len(), 3);
        assert!(!super::pending(1)[0].gated);
    }

    /// The rule over a table this build does not ship: a transparent step after
    /// the gate, as the first post-2.0.0 migration will be. A store behind the
    /// gate stays put; one whose gated step is already applied takes the later
    /// transparent step on open, like any other.
    #[test]
    fn a_transparent_step_after_an_applied_gate_still_applies_on_open() {
        let table = [
            super::Migration::new("", super::Gate::Transparent, "1.0.0", "before"),
            super::Migration::new("", super::Gate::Gated, "2.0.0", "the gate"),
            super::Migration::new("", super::Gate::Transparent, "2.1.0", "after"),
        ];
        assert_eq!(super::reachable_in(&table, 1, GatePolicy::Enforce), 1);
        assert_eq!(
            super::reachable_in(&table, 2, GatePolicy::Enforce),
            3,
            "an applied gated step is not pending"
        );
        assert_eq!(super::reachable_in(&table, 3, GatePolicy::Enforce), 3);
    }

    #[test]
    fn reachable_holds_an_existing_store_behind_a_gate() {
        // An existing store behind the gate stays where it is, from wherever it
        // starts (#749). The `> current` filter is what keeps an already-applied
        // gated step from counting as pending.
        for current in [1, 2, 3] {
            assert_eq!(super::reachable(current, GatePolicy::Enforce), current);
        }
        // A store being created has no data to endanger, so it is exempt.
        assert_eq!(super::reachable(0, GatePolicy::Enforce), LATEST_VERSION);
        // `musefs migrate` runs the lot.
        for current in [0, 1, 2, 3] {
            assert_eq!(
                super::reachable(current, GatePolicy::Bypass),
                LATEST_VERSION
            );
        }
    }

    /// A store with a gated step anywhere ahead is refused before anything is
    /// applied, the transparent steps included: each bumps `user_version` past
    /// what the previous release opens, so applying them on the way to a
    /// refusal would lock that release out with no snapshot taken (#749).
    #[test]
    fn an_existing_store_behind_a_gate_is_refused_untouched() {
        let mut conn = Connection::open_in_memory().unwrap();
        store_at(&conn, 1);

        let err = super::migrate(&mut conn).expect_err("V4 is gated");

        assert!(
            matches!(
                err,
                DbError::StoreNeedsMigration { found, target }
                    if found == 1 && target == LATEST_VERSION
            ),
            "{err:?}"
        );
        assert_eq!(
            user_version(&conn),
            1,
            "nothing is applied while a gated step is pending"
        );
    }

    /// A store already sitting at the wall has nothing to apply, so the refusal
    /// is all that happens and it is idempotent.
    #[test]
    fn a_store_at_the_gate_applies_nothing_and_keeps_refusing() {
        let mut conn = Connection::open_in_memory().unwrap();
        store_at(&conn, WALL);

        for _ in 0..2 {
            let err = super::migrate(&mut conn).expect_err("V4 is gated");
            assert!(
                matches!(err, DbError::StoreNeedsMigration { .. }),
                "{err:?}"
            );
            assert_eq!(user_version(&conn), WALL);
        }
    }

    /// A store this binary is creating has no data to endanger and must run
    /// straight to the latest version — gating it would stop `scan` from ever
    /// building a new library.
    #[test]
    fn a_fresh_store_runs_through_the_gate() {
        let mut conn = Connection::open_in_memory().unwrap();
        assert_eq!(user_version(&conn), 0);

        super::migrate(&mut conn).expect("creating a store is never gated");

        assert_eq!(user_version(&conn), LATEST_VERSION);
    }

    #[test]
    fn migrate_all_applies_the_gated_step() {
        let mut conn = Connection::open_in_memory().unwrap();
        store_at(&conn, WALL);

        super::migrate_all(&mut conn).expect("the gated step is what this call is for");

        assert_eq!(user_version(&conn), LATEST_VERSION);
    }

    /// The whole point of the refusal is that it tells the user what to run.
    #[test]
    fn the_refusal_names_the_command_and_both_versions() {
        let msg = super::gated(WALL).to_string();
        assert!(msg.contains("musefs migrate"), "{msg}");
        assert!(msg.contains(&WALL.to_string()), "{msg}");
        assert!(msg.contains(&LATEST_VERSION.to_string()), "{msg}");
    }

    /// The gate is decided under the write lock, not from the version read
    /// before it. A `mount` that starts while `musefs migrate` is running waits
    /// for the lock and then finds the store current — refusing it for a gate
    /// that was lifted while it waited would be a spurious outage on exactly
    /// the day the user did the right thing.
    ///
    /// Only reachable through the before-lock seam: no single-threaded test can
    /// otherwise arrange for the pre-lock read to see the old version and the
    /// post-lock read the new one.
    #[test]
    fn a_gate_lifted_while_we_waited_for_the_lock_is_not_refused() {
        struct HookGuard;
        impl Drop for HookGuard {
            fn drop(&mut self) {
                super::clear_before_lock_hook();
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        let mut conn = Connection::open(&path).unwrap();
        store_at(&conn, WALL);

        let racer_path = path.clone();
        super::set_before_lock_hook(move || {
            let mut racer = Connection::open(&racer_path).unwrap();
            super::migrate_all(&mut racer).unwrap();
        });
        let _guard = HookGuard;

        super::migrate(&mut conn)
            .expect("the store is at the latest version by the time we hold the lock");
        assert_eq!(user_version(&conn), LATEST_VERSION);
    }

    /// The too-new refusal is decided under the write lock as well, not only
    /// from the read before it. A newer binary can take the store past this
    /// build's ceiling while we wait, and reporting a successful migration of a
    /// store this build cannot read would be the worst of both answers.
    ///
    /// Only reachable through the before-lock seam, like the race above.
    #[test]
    fn a_store_taken_past_the_ceiling_while_we_waited_is_refused() {
        struct HookGuard;
        impl Drop for HookGuard {
            fn drop(&mut self) {
                super::clear_before_lock_hook();
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        let mut conn = Connection::open(&path).unwrap();
        store_at(&conn, WALL);

        let racer_path = path.clone();
        super::set_before_lock_hook(move || {
            let racer = Connection::open(&racer_path).unwrap();
            racer
                .pragma_update(None, "user_version", LATEST_VERSION + 1)
                .unwrap();
        });
        let _guard = HookGuard;

        let err = super::migrate(&mut conn).expect_err("the store is now from the future");
        assert!(
            matches!(err, DbError::StoreTooNew { found, .. } if found == LATEST_VERSION + 1),
            "{err:?}"
        );
    }

    /// Two directions, two remedies. A store from a newer binary wants a newer
    /// binary; a store from an older one wants the command. Telling them apart
    /// is the reason they are separate variants.
    #[test]
    fn the_two_refusals_carry_different_remedies() {
        let mut conn = Connection::open_in_memory().unwrap();
        store_at(&conn, WALL);
        let old = super::migrate(&mut conn)
            .expect_err("V4 is gated")
            .to_string();

        let mut newer = Connection::open_in_memory().unwrap();
        newer
            .pragma_update(None, "user_version", LATEST_VERSION + 1)
            .unwrap();
        let new = super::migrate(&mut newer)
            .expect_err("a store from the future")
            .to_string();

        assert!(old.contains("musefs migrate"), "{old}");
        assert!(new.contains("upgrade musefs"), "{new}");
        assert_ne!(old, new);
    }
}

fn reference_objects() -> &'static std::collections::BTreeMap<(String, String), String> {
    static REF: std::sync::OnceLock<std::collections::BTreeMap<(String, String), String>> =
        std::sync::OnceLock::new();
    REF.get_or_init(|| {
        let mut conn =
            Connection::open_in_memory().expect("in-memory connection for schema reference");
        migrate(&mut conn).expect("reference migration must succeed on a fresh DB");
        read_schema_objects(&conn).expect("reading reference schema must succeed")
    })
}

fn read_schema_objects(
    conn: &Connection,
) -> crate::Result<std::collections::BTreeMap<(String, String), String>> {
    let mut stmt = conn.prepare(
        "SELECT type, name, COALESCE(sql, '') FROM sqlite_master \
         WHERE name NOT LIKE 'sqlite_%'",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            (r.get::<_, String>(0)?, r.get::<_, String>(1)?),
            r.get::<_, String>(2)?,
        ))
    })?;
    let mut map = std::collections::BTreeMap::new();
    for row in rows {
        let ((ty, name), sql) = row?;
        map.insert((ty, name), sql);
    }
    Ok(map)
}

fn schema_mismatch(key: &(String, String), what: &str) -> crate::error::DbError {
    crate::error::DbError::SchemaMismatch {
        object: format!("{} {} ({what})", key.0, key.1),
    }
}

pub(crate) fn validate_identity(conn: &Connection) -> crate::Result<()> {
    let reference = reference_objects();
    let actual = read_schema_objects(conn)?;

    let mut keys: Vec<&(String, String)> = reference.keys().chain(actual.keys()).collect();
    keys.sort();
    keys.dedup();
    for key in keys {
        match (reference.get(key), actual.get(key)) {
            (Some(r), Some(a)) if r != a => return Err(schema_mismatch(key, "altered")),
            (Some(_), None) => return Err(schema_mismatch(key, "missing")),
            (None, Some(_)) => return Err(schema_mismatch(key, "unexpected")),
            _ => {}
        }
    }

    let mut fk = conn.prepare("PRAGMA foreign_key_check")?;
    let mut rows = fk.query([])?;
    if let Some(row) = rows.next()? {
        let table: String = row.get(0)?;
        return Err(crate::error::DbError::SchemaMismatch {
            object: format!("foreign key violation in table {table}"),
        });
    }
    Ok(())
}

/// The `tracks` rebuild (#686): what it must preserve, and what it newly
/// refuses. Seeded from a real pre-V4 store rather than a hand-built one, so
/// these exercise the upgrade path an existing library actually takes.
#[cfg(test)]
mod v4_tracks_rebuild_tests {
    use rusqlite::Connection;

    /// A populated store stopped at `upto`, seeded the way a scanner of that era
    /// would have: two tracks with a tag, an art link and a structural block
    /// each, plus a checksum pair on the first.
    fn populated_store_at(upto: usize) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        for (target, migration) in (1i64..).zip(super::MIGRATIONS).take(upto) {
            conn.execute_batch(migration.sql).unwrap();
            conn.pragma_update(None, "user_version", target).unwrap();
        }
        // V1 has no checksum columns; V2 added them.
        let checksums = upto >= 2;
        for (i, path) in ["/lib/a.flac", "/lib/b.flac"].iter().enumerate() {
            conn.execute(
                "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
                 backing_size, backing_mtime_ns, backing_ctime_ns, updated_at) \
                 VALUES (?1,'flac',4,6,10,111,222,1700000000)",
                [path],
            )
            .unwrap();
            let id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO tags (track_id, key, value, ordinal) VALUES (?1,'artist',?2,0)",
                rusqlite::params![id, format!("Artist {i}")],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO structural_blocks (track_id, kind, ordinal, body) \
                 VALUES (?1,'STREAMINFO',0,X'0102')",
                [id],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO art (sha256, mime, width, height, byte_len, data) \
             VALUES (?1, 'image/png', 1, 1, 1, X'00')",
            [&"e".repeat(64)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO track_art (track_id, art_id, picture_type, description, ordinal) \
             VALUES (1,1,3,'cover',0)",
            [],
        )
        .unwrap();
        if checksums {
            conn.execute(
                "UPDATE tracks SET fingerprint = ?1, content_hash = ?2 WHERE id = 1",
                rusqlite::params!["f".repeat(64), "c".repeat(64)],
            )
            .unwrap();
        }
        conn
    }

    fn content_versions(conn: &Connection) -> Vec<(i64, i64)> {
        conn.prepare("SELECT id, content_version FROM tracks ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn path_of(conn: &Connection, id: i64) -> (String, Vec<u8>) {
        conn.query_row(
            "SELECT typeof(backing_path), backing_path FROM tracks WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    }

    /// Every row, every child and every id survives — and the children come back
    /// attached to the same track.
    #[test]
    fn the_rebuild_preserves_every_row_and_its_children() {
        let mut conn = populated_store_at(3);
        let before = content_versions(&conn);
        super::migrate_all(&mut conn).unwrap();

        assert_eq!(
            conn.query_row("SELECT count(*) FROM tracks", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
        let ids: Vec<i64> = conn
            .prepare("SELECT id FROM tracks ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            ids,
            vec![1, 2],
            "ids are identities, not sequence positions"
        );

        let tags: Vec<(i64, String)> = conn
            .prepare("SELECT track_id, value FROM tags ORDER BY track_id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            tags,
            vec![(1, "Artist 0".to_string()), (2, "Artist 1".to_string())]
        );
        assert_eq!(
            conn.query_row(
                "SELECT description FROM track_art WHERE track_id = 1",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "cover"
        );
        assert_eq!(
            conn.query_row(
                "SELECT body FROM structural_blocks WHERE track_id = 2",
                [],
                |r| r.get::<_, Vec<u8>>(0)
            )
            .unwrap(),
            vec![1u8, 2]
        );
        assert_eq!(before, content_versions(&conn));
    }

    /// The refill must not fire the child triggers. `content_version` is what
    /// every cache keys on and what the served virtual mtime derives from
    /// (#725), so a bump here would invalidate every layout in the store and
    /// move every file's mtime for a migration that changed no bytes.
    #[test]
    fn the_rebuild_does_not_bump_content_version() {
        let mut conn = populated_store_at(3);
        // Give the rows a non-zero, non-uniform history first, so an accidental
        // reset to the default would be as visible as an accidental bump.
        conn.execute("UPDATE tracks SET content_version = 7 WHERE id = 1", [])
            .unwrap();
        conn.execute("UPDATE tracks SET content_version = 12 WHERE id = 2", [])
            .unwrap();
        super::migrate_all(&mut conn).unwrap();
        assert_eq!(content_versions(&conn), vec![(1, 7), (2, 12)]);
    }

    /// The path survives the type change byte for byte, and is reachable by the
    /// byte-binding lookup the scanner uses.
    #[test]
    fn the_refill_casts_the_path_without_changing_its_bytes() {
        let mut conn = populated_store_at(3);
        super::migrate_all(&mut conn).unwrap();

        let (kind, bytes) = path_of(&conn, 1);
        assert_eq!(kind, "blob");
        assert_eq!(bytes, b"/lib/a.flac");
        let found: i64 = conn
            .query_row(
                "SELECT id FROM tracks WHERE backing_path = ?1",
                [&b"/lib/a.flac"[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(found, 1, "without the CAST every row would be unreachable");
    }

    /// #678: the whole point of AUTOINCREMENT. Deleting the highest-numbered
    /// track used to free exactly that id for the next insert, and the
    /// incremental refresh reads an id as a persistent identity.
    #[test]
    fn a_deleted_id_is_never_handed_out_again() {
        let mut conn = populated_store_at(3);
        super::migrate_all(&mut conn).unwrap();
        conn.execute("DELETE FROM tracks WHERE id = 2", []).unwrap();
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, updated_at) VALUES (?1,'flac',0,1,1,0,0)",
            [&b"/lib/c.flac"[..]],
        )
        .unwrap();
        assert_eq!(conn.last_insert_rowid(), 3, "id 2 is retired, not recycled");
    }

    /// #674: the column arrives as the `not yet known` sentinel on every
    /// upgraded row, and joins the geometry bump so that arming it later counts
    /// as a content change.
    #[test]
    fn backing_ino_starts_unknown_and_bumps_content_version_when_it_changes() {
        let mut conn = populated_store_at(3);
        super::migrate_all(&mut conn).unwrap();
        let ino: i64 = conn
            .query_row("SELECT backing_ino FROM tracks WHERE id = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(ino, 0);

        let before = conn
            .query_row("SELECT content_version FROM tracks WHERE id = 1", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap();
        conn.execute("UPDATE tracks SET backing_ino = 4242 WHERE id = 1", [])
            .unwrap();
        let after = conn
            .query_row("SELECT content_version FROM tracks WHERE id = 1", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap();
        assert_eq!(after, before + 1);
    }

    /// #696: the lower bounds are gone, so an archival rip dated before 1970
    /// reaches the store instead of dying at a CHECK.
    #[test]
    fn a_pre_epoch_stamp_is_accepted() {
        let mut conn = populated_store_at(3);
        super::migrate_all(&mut conn).unwrap();
        conn.execute(
            "UPDATE tracks SET backing_mtime_ns = -1500000000, \
             backing_ctime_ns = -1500000000 WHERE id = 1",
            [],
        )
        .unwrap();
    }

    /// #718: declaring the column BLOB is an affinity, not a guarantee. Without
    /// the typeof CHECK a TEXT path inserts happily and UNIQUE does not compare
    /// it equal to the same bytes, so one file occupies two rows.
    #[test]
    fn a_text_path_is_refused_even_though_it_spells_the_same_bytes() {
        let mut conn = populated_store_at(3);
        super::migrate_all(&mut conn).unwrap();
        let err = conn
            .execute(
                "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
                 backing_size, backing_mtime_ns, updated_at) \
                 VALUES ('/lib/a.flac','flac',0,1,1,0,0)",
                [],
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("typeof(backing_path)"), "{err}");
        assert_eq!(
            conn.query_row("SELECT count(*) FROM tracks", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    /// #718, the other half: an empty path and a NUL-bearing one are not paths.
    #[test]
    fn an_empty_or_nul_bearing_path_is_refused() {
        let mut conn = populated_store_at(3);
        super::migrate_all(&mut conn).unwrap();
        for bytes in [&b""[..], &b"/lib/\x00.flac"[..]] {
            assert!(
                conn.execute(
                    "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
                     backing_size, backing_mtime_ns, updated_at) VALUES (?1,'flac',0,1,1,0,0)",
                    [bytes],
                )
                .is_err(),
                "{bytes:?} is not a path"
            );
        }
    }

    /// #693: `length()` on TEXT stops at the first NUL, so a 64-character
    /// prefix followed by NUL and anything at all satisfied the old CHECK while
    /// storing something that is not a 64-character identity.
    #[test]
    fn a_nul_bearing_checksum_is_refused() {
        let mut conn = populated_store_at(3);
        super::migrate_all(&mut conn).unwrap();
        let hostile = format!("{}\0{}", "a".repeat(64), "junk".repeat(1000));
        for col in ["fingerprint", "content_hash"] {
            let err = conn
                .execute(
                    &format!("UPDATE tracks SET {col} = ?1 WHERE id = 1"),
                    [&hostile],
                )
                .unwrap_err()
                .to_string();
            assert!(err.contains(col), "{err}");
        }
    }

    /// A `content_hash` the new CHECK would reject cannot abort the upgrade:
    /// the refill carries no hash at all (#689), so it is nulled with the rest.
    #[test]
    fn the_refill_nulls_a_content_hash_the_new_check_would_reject() {
        let mut conn = populated_store_at(3);
        let hostile = format!("{}\0{}", "a".repeat(64), "junk");
        conn.execute(
            "UPDATE tracks SET content_hash = ?1 WHERE id = 1",
            [&hostile],
        )
        .unwrap();
        super::migrate_all(&mut conn).unwrap();

        let ch: Option<String> = conn
            .query_row("SELECT content_hash FROM tracks WHERE id = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(ch, None, "sanitized, not carried and not fatal");
        let kept: Option<String> = conn
            .query_row("SELECT content_hash FROM tracks WHERE id = 2", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(kept, None, "id 2 never had one");
    }

    /// #691 and #689, folded into the refill: the fingerprint is retired for
    /// every row, and so is `content_hash`, even a well-formed one, because an
    /// older rescan could leave it describing bytes the file no longer holds.
    #[test]
    fn the_refill_retires_the_fingerprint_and_the_content_hash() {
        let mut conn = populated_store_at(3);
        super::migrate_all(&mut conn).unwrap();
        let (fp, ch): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT fingerprint, content_hash FROM tracks WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(fp, None);
        assert_eq!(ch, None, "a well-formed hash is not proof it is current");
    }

    /// The upgrade rehearsal: a populated store from every released shape
    /// reaches the latest version with its rows intact. V1 predates the
    /// checksum columns entirely, which is the arm that would catch a refill
    /// naming a column that era does not have.
    #[test]
    fn a_populated_store_of_every_earlier_version_upgrades() {
        for upto in 1..=3 {
            let mut conn = populated_store_at(upto);
            super::migrate_all(&mut conn).unwrap();
            assert_eq!(
                conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                    .unwrap(),
                super::LATEST_VERSION,
                "V{upto} store"
            );
            assert_eq!(
                conn.query_row("SELECT count(*) FROM tags", [], |r| r.get::<_, i64>(0))
                    .unwrap(),
                2,
                "V{upto} store kept its tags"
            );
            assert_eq!(path_of(&conn, 2).1, b"/lib/b.flac");
        }
    }
}

/// The `tags` and `track_art` rebuild: the ordinal split, immutable ownership,
/// the per-embedding columns, and the constraints both tables gained.
#[cfg(test)]
mod v4_tags_and_track_art_rebuild_tests {
    use rusqlite::Connection;

    /// A V3 store with two tracks that share one deduplicated `art` row -- the
    /// shape #716 is about -- each linking it with its own description.
    fn populated_v3() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        for (target, migration) in (1i64..).zip(super::MIGRATIONS).take(3) {
            conn.execute_batch(migration.sql).unwrap();
            conn.pragma_update(None, "user_version", target).unwrap();
        }
        for path in ["/lib/a.flac", "/lib/b.mp3"] {
            conn.execute(
                "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
                 backing_size, backing_mtime_ns, backing_ctime_ns, updated_at) \
                 VALUES (?1, 'flac', 4, 6, 10, 0, 0, 0)",
                [path],
            )
            .unwrap();
        }
        // One blob, one row: the deduplication that made the geometry shared.
        conn.execute(
            "INSERT INTO art (sha256, mime, width, height, byte_len, data) \
             VALUES (?1, 'image/jpeg', 1200, 1200, 1, X'00')",
            [&"a".repeat(64)],
        )
        .unwrap();
        for (track, desc) in [(1, "front"), (2, "back")] {
            conn.execute(
                "INSERT INTO track_art (track_id, art_id, picture_type, description, ordinal) \
                 VALUES (?1, 1, 3, ?2, 0)",
                rusqlite::params![track, desc],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1, 'artist', 'A', 0)",
            [],
        )
        .unwrap();
        conn
    }

    fn migrated() -> Connection {
        let mut conn = populated_v3();
        super::migrate_all(&mut conn).unwrap();
        conn
    }

    /// #663: the primary key numbered a track's text rows and its binary rows in
    /// one ordinal space per key, so an external writer rewriting one class alone
    /// -- which both `contrib` helpers do -- could land on an ordinal the other
    /// class already held. The two partial indexes give them separate spaces.
    #[test]
    fn text_and_binary_rows_get_independent_ordinal_spaces() {
        let conn = migrated();
        conn.execute(
            "INSERT INTO tags (track_id, key, value, value_blob, ordinal) \
             VALUES (1, 'PRIV', '', X'DEADBEEF', 0)",
            [],
        )
        .unwrap();
        // The collision this issue is about: a text row on an ordinal a binary
        // row already holds, under the same key.
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1, 'PRIV', 'v', 0)",
            [],
        )
        .expect("text and binary ordinals are separate spaces now");

        // Within one class the uniqueness still holds, which is what the indexes
        // are for -- the split must not have simply removed the constraint.
        assert!(
            conn.execute(
                "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1, 'PRIV', 'w', 0)",
                [],
            )
            .is_err(),
            "two text rows may not share one ordinal"
        );
        assert!(
            conn.execute(
                "INSERT INTO tags (track_id, key, value, value_blob, ordinal) \
                 VALUES (1, 'PRIV', '', X'BEEF', 0)",
                [],
            )
            .is_err(),
            "two binary rows may not share one ordinal"
        );
    }

    /// Every shape that reads a track's tags must reach them through an index.
    ///
    /// Dropping the primary key for #663 took the only index a query that does
    /// *not* constrain `value_blob` could use, and the obvious replacement --
    /// two partial unique indexes -- cannot serve one, because a partial index
    /// only applies where the query's WHERE implies its predicate. That query
    /// exists: `tags_for_track` in the `contrib` helpers reads both classes at
    /// once. Against two partial indexes it planned as `SCAN tags` plus a temp
    /// B-tree for the ORDER BY, on the path a plugin uses per track.
    #[test]
    fn every_tags_read_shape_uses_an_index() {
        let conn = migrated();
        for (what, sql) in [
            (
                "both classes at once (contrib's tags_for_track)",
                "SELECT key, value, value_blob FROM tags \
                 WHERE track_id = 1 ORDER BY key, ordinal",
            ),
            (
                "text rows only",
                "SELECT key, value FROM tags \
                 WHERE track_id = 1 AND value_blob IS NULL ORDER BY key, ordinal",
            ),
            (
                "binary rows only",
                "SELECT key FROM tags \
                 WHERE track_id = 1 AND value_blob IS NOT NULL ORDER BY key, ordinal",
            ),
        ] {
            let plan: Vec<String> = conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap()
                .query_map([], |r| r.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            let plan = plan.join(" | ");
            assert!(
                plan.contains("USING INDEX") && !plan.contains("SCAN tags"),
                "{what} must not scan the table: {plan}"
            );
            assert!(
                !plan.contains("TEMP B-TREE"),
                "{what} must not sort: {plan}"
            );
        }
    }

    /// #717: reparenting left the old owner serving a stale layout with a
    /// `content_version` that still matched, because the `_au` bump named only
    /// the new owner. Ownership is now immutable, the way `art` already is.
    #[test]
    fn row_ownership_is_immutable() {
        let conn = migrated();
        let tag_err = conn
            .execute("UPDATE tags SET track_id = 2 WHERE track_id = 1", [])
            .unwrap_err()
            .to_string();
        assert!(tag_err.contains("tag ownership is immutable"), "{tag_err}");
        let art_err = conn
            .execute("UPDATE track_art SET track_id = 2 WHERE track_id = 1", [])
            .unwrap_err()
            .to_string();
        assert!(
            art_err.contains("art link ownership is immutable"),
            "{art_err}"
        );
    }

    /// The `WHEN` guard, which is load-bearing: `BEFORE UPDATE OF track_id`
    /// fires whenever the column appears in a SET list, so without it a writer
    /// rewriting a row wholesale without moving it would be refused.
    #[test]
    fn a_same_owner_rewrite_is_not_a_reparent() {
        let conn = migrated();
        conn.execute(
            "UPDATE tags SET track_id = 1, value = 'B' WHERE track_id = 1 AND key = 'artist'",
            [],
        )
        .expect("naming track_id without changing it is not a reparent");
    }

    /// The widened bump is correct on its own terms rather than only because the
    /// refusal forbids the case it used to mishandle -- which is what matters
    /// against a writer that drops triggers through `writable_schema`. Dropping
    /// the refusal is how that writer is simulated.
    #[test]
    fn the_bump_names_both_owners_when_the_refusal_is_gone() {
        let conn = migrated();
        conn.execute_batch("DROP TRIGGER tags_reject_reparent")
            .unwrap();
        let cv = |id: i64| -> i64 {
            conn.query_row(
                "SELECT content_version FROM tracks WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
        };
        let (before_1, before_2) = (cv(1), cv(2));
        conn.execute("UPDATE tags SET track_id = 2 WHERE track_id = 1", [])
            .unwrap();
        assert_eq!(cv(1), before_1 + 1, "the track that LOST the row must bump");
        assert_eq!(
            cv(2),
            before_2 + 1,
            "the track that gained it must bump too"
        );
    }

    /// #716's adding half: the per-embedding columns exist on the link and are
    /// backfilled from the shared `art` row. The migration cannot restore what
    /// ingest destroyed -- both links get the same values, and the true ones
    /// come back on a rescan -- but the column is where they now live.
    #[test]
    fn track_art_gains_the_per_embedding_columns_backfilled_from_art() {
        /// One `track_art` row's per-embedding columns, as the link now owns them.
        #[derive(Debug, PartialEq, Eq)]
        struct Embedding {
            track_id: i64,
            description: String,
            mime: String,
            width: Option<i64>,
            height: Option<i64>,
            depth: i64,
            colors: i64,
        }

        let conn = migrated();
        let mut stmt = conn
            .prepare(
                "SELECT track_id, description, mime, width, height, depth, colors \
                 FROM track_art ORDER BY track_id",
            )
            .unwrap();
        let rows: Vec<Embedding> = stmt
            .query_map([], |r| {
                Ok(Embedding {
                    track_id: r.get(0)?,
                    description: r.get(1)?,
                    mime: r.get(2)?,
                    width: r.get(3)?,
                    height: r.get(4)?,
                    depth: r.get(5)?,
                    colors: r.get(6)?,
                })
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        // Both links take the same geometry: the backfill can only copy what the
        // shared `art` row holds. The true per-embedding values come back on a
        // rescan, which is what the release notes have to say plainly.
        let shared = |track_id: i64, description: &str| Embedding {
            track_id,
            description: description.into(),
            mime: "image/jpeg".into(),
            width: Some(1200),
            height: Some(1200),
            depth: 0,
            colors: 0,
        };
        assert_eq!(
            rows,
            vec![shared(1, "front"), shared(2, "back")],
            "each link keeps its own description and takes the shared geometry"
        );
    }

    /// #693, following the column: the ban lands on `track_art.description` and
    /// on `tags.key`.
    #[test]
    fn a_nul_bearing_key_or_description_is_refused() {
        let conn = migrated();
        let key = format!("k{}more", '\0');
        assert!(
            conn.execute(
                "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1, ?1, 'v', 9)",
                [&key],
            )
            .is_err(),
            "a NUL in a tag key is refused"
        );
        assert!(
            conn.execute(
                "UPDATE track_art SET description = ?1 WHERE track_id = 1",
                [&key],
            )
            .is_err(),
            "a NUL in an art description is refused"
        );
    }

    /// #718 for these two tables: a schema-valid row must not be a Rust
    /// conversion failure.
    #[test]
    fn storage_classes_and_widths_are_pinned() {
        let conn = migrated();
        // A REAL ordinal satisfies `ordinal >= 0` but is not an integer.
        assert!(
            conn.execute(
                "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1, 'k', 'v', 0.5)",
                [],
            )
            .is_err()
        );
        // Geometry past u32 is a conversion failure against Option<u32>.
        assert!(
            conn.execute(
                "UPDATE track_art SET width = 1099511627776 WHERE track_id = 1",
                [],
            )
            .is_err()
        );
        // NULL geometry stays legal: an ID3 APIC carries no dimensions.
        conn.execute("UPDATE track_art SET width = NULL WHERE track_id = 1", [])
            .unwrap();
    }

    /// The rebuild is still a rebuild: rows, ownership and the reverse-edge
    /// index survive it.
    #[test]
    fn the_rebuild_preserves_rows_and_the_reverse_edge_index() {
        let conn = migrated();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM track_art", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            conn.query_row("SELECT value FROM tags WHERE track_id = 1", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "A"
        );
        let idx: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'track_art_art_id_idx'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            idx, 1,
            "the DROP TABLE took it; the rebuild must put it back"
        );
    }
}

/// The `art` rebuild: the last of the three, and the one with an ordering
/// constraint the others did not have.
#[cfg(test)]
mod v4_art_rebuild_tests {
    use rusqlite::Connection;

    fn populated_v3() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        for (target, migration) in (1i64..).zip(super::MIGRATIONS).take(3) {
            conn.execute_batch(migration.sql).unwrap();
            conn.pragma_update(None, "user_version", target).unwrap();
        }
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, backing_ctime_ns, updated_at) \
             VALUES ('/lib/a.flac', 'flac', 0, 0, 0, 0, 0, 0)",
            [],
        )
        .unwrap();
        for (ordinal, sha) in [(0i64, "a"), (1i64, "b")] {
            conn.execute(
                "INSERT INTO art (sha256, mime, width, height, byte_len, data) \
                 VALUES (?1, 'image/png', 64, 64, 3, X'ABCDEF')",
                [&sha.repeat(64)],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO track_art (track_id, art_id, picture_type, description, ordinal) \
                 VALUES (1, ?1, 3, 'cover', ?2)",
                rusqlite::params![conn.last_insert_rowid(), ordinal],
            )
            .unwrap();
        }
        conn
    }

    fn migrated() -> Connection {
        let mut conn = populated_v3();
        super::migrate_all(&mut conn).unwrap();
        conn
    }

    /// The blobs survive, ids and all. `art` is the one table whose rebuild
    /// copies real payload rather than a handful of scalar columns, so "did the
    /// data come through" is the first thing to pin.
    #[test]
    fn every_blob_survives_the_rebuild() {
        let conn = migrated();
        let rows: Vec<(i64, String, Vec<u8>)> = conn
            .prepare("SELECT id, sha256, data FROM art ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                (1, "a".repeat(64), vec![0xAB, 0xCD, 0xEF]),
                (2, "b".repeat(64), vec![0xAB, 0xCD, 0xEF]),
            ]
        );
        // The links still resolve, which is what the id preservation is for.
        let joined: i64 = conn
            .query_row(
                "SELECT count(*) FROM track_art t JOIN art a ON a.id = t.art_id",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(joined, 2);
    }

    /// #719: changing the key changed none of the content columns, so the old
    /// `WHEN` clause was false and the trigger never fired. Under the
    /// foreign-keys-off writer this store already defends against, that silently
    /// orphaned every link while nothing bumped `content_version`.
    #[test]
    fn the_immutability_trigger_covers_the_key() {
        let conn = migrated();
        conn.pragma_update(None, "foreign_keys", false).unwrap();
        let err = conn
            .execute("UPDATE art SET id = 99 WHERE id = 1", [])
            .unwrap_err()
            .to_string();
        assert!(err.contains("art rows are immutable"), "{err}");
        // And the content columns it always covered still abort.
        for sql in [
            "UPDATE art SET data = X'00' WHERE id = 1",
            "UPDATE art SET sha256 = ?1 WHERE id = 1",
        ] {
            assert!(
                conn.execute(sql, rusqlite::params!["c".repeat(64)])
                    .is_err()
            );
        }
        // A no-op update is still allowed: the guard is on change, not on touch.
        conn.execute("UPDATE art SET byte_len = byte_len WHERE id = 1", [])
            .unwrap();
    }

    /// #718 for this table. `byte_len = length(data)` was always there; what is
    /// new is that the columns must be the storage class the Rust model reads.
    ///
    /// Note what a `typeof` CHECK does *not* catch: column affinity converts
    /// what it can on the way in. `byte_len = '1'` is stored as the integer 1,
    /// and so is an exactly-integral `REAL` like `1.0` — neither is a violation
    /// at all. The constraint is there for what affinity cannot convert — text
    /// that is not a number, a `REAL` with a fractional part, and blobs — which
    /// is exactly the set that reaches the Rust side as a conversion failure
    /// rather than as a wrong number.
    #[test]
    fn storage_classes_are_pinned() {
        let conn = migrated();
        conn.pragma_update(None, "foreign_keys", false).unwrap();
        for (what, sql) in [
            (
                "a non-numeric byte_len",
                "INSERT INTO art (sha256, byte_len, data) \
                                 VALUES (?1, 'abc', X'00')",
            ),
            (
                "a text blob",
                "INSERT INTO art (sha256, byte_len, data) \
                             VALUES (?1, 1, 'x')",
            ),
            (
                "a blob sha256",
                "INSERT INTO art (sha256, byte_len, data) \
                                 VALUES (CAST(?1 AS BLOB), 1, X'00')",
            ),
        ] {
            assert!(
                conn.execute(sql, rusqlite::params!["c".repeat(64)])
                    .is_err(),
                "{what} must be refused"
            );
        }
    }

    /// The ordering constraint that decides where this step can sit at all:
    /// `track_art.art_id` references `art(id)` with no ON DELETE CASCADE, so
    /// with foreign keys enforced the table can only be dropped while no link
    /// row exists. A migration that rebuilt `art` after refilling the children
    /// would fail outright.
    #[test]
    fn art_cannot_be_dropped_while_a_link_exists() {
        let conn = migrated();
        let err = conn
            .execute_batch("DROP TABLE art")
            .unwrap_err()
            .to_string();
        assert!(err.contains("FOREIGN KEY"), "{err}");
        conn.execute("DELETE FROM track_art", []).unwrap();
        conn.execute_batch("DROP TABLE art")
            .expect("droppable once nothing references it -- the migration's window");
    }

    /// A row the tightened constraints reject fails the migration rather than
    /// being quietly repaired. `art` carries no scanner-owned column a rescan
    /// recomputes, so there is nothing here the sanitize-only-under-a-flag
    /// policy would let the refill null on its own. The violation is a text
    /// `data` -- a storage class V3 never checked and `BLOB` affinity does not
    /// convert, so it survives the copy and is caught on the way back in.
    #[test]
    fn a_row_the_new_constraints_reject_fails_the_migration() {
        let mut conn = populated_v3();
        conn.pragma_update(None, "ignore_check_constraints", true)
            .unwrap();
        conn.execute(
            "INSERT INTO art (sha256, mime, width, height, byte_len, data) \
             VALUES (?1, 'image/png', 64, 64, 1, 'x')",
            [&"c".repeat(64)],
        )
        .unwrap();
        conn.pragma_update(None, "ignore_check_constraints", false)
            .unwrap();
        let err = super::migrate_all(&mut conn).unwrap_err().to_string();
        assert!(err.contains("CHECK constraint failed"), "{err}");
    }
}

/// `structural_blocks` (#732): the table the other three rebuilds left behind.
#[cfg(test)]
mod v4_structural_blocks_rebuild_tests {
    use rusqlite::Connection;

    fn migrated_with_a_block() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        for (target, migration) in (1i64..).zip(super::MIGRATIONS).take(3) {
            conn.execute_batch(migration.sql).unwrap();
            conn.pragma_update(None, "user_version", target).unwrap();
        }
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, backing_ctime_ns, updated_at) \
             VALUES ('/lib/a.flac', 'flac', 0, 0, 0, 0, 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO structural_blocks (track_id, kind, ordinal, body) \
             VALUES (1, 'STREAMINFO', 0, X'0102')",
            [],
        )
        .unwrap();
        super::migrate_all(&mut conn).unwrap();
        conn
    }

    #[test]
    fn the_rows_survive_the_rebuild() {
        let conn = migrated_with_a_block();
        let (kind, body): (String, Vec<u8>) = conn
            .query_row(
                "SELECT kind, body FROM structural_blocks WHERE track_id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(kind, "STREAMINFO");
        assert_eq!(body, vec![1u8, 2]);
    }

    /// The gap this closes: each of these is a value affinity will not convert,
    /// so it reached the reader as a `rusqlite` conversion failure — a
    /// store-wide error rather than the malformed row it is.
    #[test]
    fn storage_classes_are_pinned() {
        let conn = migrated_with_a_block();
        for (what, sql) in [
            (
                // Non-integral deliberately: INTEGER affinity converts an
                // exactly-integral REAL, so `1.0` is stored as the integer 1
                // and is correctly not a violation.
                "a non-integral real ordinal",
                "INSERT INTO structural_blocks (track_id, kind, ordinal, body) \
                 VALUES (1, 'SEEKTABLE', 0.5, X'00')",
            ),
            (
                "a text body",
                "INSERT INTO structural_blocks (track_id, kind, ordinal, body) \
                 VALUES (1, 'SEEKTABLE', 1, 'not a blob')",
            ),
        ] {
            assert!(conn.execute(sql, []).is_err(), "{what} must be refused");
        }
        // `kind` needs no storage-class check of its own: the IN list the table
        // always had is strictly stronger, and refuses a wrong name and a wrong
        // storage class alike.
        for bad_kind in ["'NOT_A_KIND'", "X'4142'", "7"] {
            assert!(
                conn.execute(
                    &format!(
                        "INSERT INTO structural_blocks (track_id, kind, ordinal, body) \
                         VALUES (1, {bad_kind}, 1, X'00')"
                    ),
                    [],
                )
                .is_err(),
                "kind {bad_kind} must be refused"
            );
        }
    }

    /// And the migration's pre-flight sees them, so a store holding one is told
    /// before the upgrade starts rather than failing part-way through it.
    #[test]
    fn a_violating_row_is_reported_by_the_preflight() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        {
            let conn = Connection::open(&path).unwrap();
            for (target, migration) in (1i64..).zip(super::MIGRATIONS).take(3) {
                conn.execute_batch(migration.sql).unwrap();
                conn.pragma_update(None, "user_version", target).unwrap();
            }
            conn.execute(
                "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
                 backing_size, backing_mtime_ns, backing_ctime_ns, updated_at) \
                 VALUES ('/lib/a.flac', 'flac', 0, 0, 0, 0, 0, 0)",
                [],
            )
            .unwrap();
            // A TEXT `body`: the V1 table bounds its length and nothing else,
            // and `length()` answers for text as readily as for a blob -- so
            // this row goes in on the honest write path, with no pragma and no
            // hostile writer. It is what a buggy tool binding a string instead
            // of bytes leaves behind, and only V4 refuses it.
            conn.execute(
                "INSERT INTO structural_blocks (track_id, kind, ordinal, body) \
                 VALUES (1, 'STREAMINFO', 0, 'not a blob')",
                [],
            )
            .unwrap();
        }
        let pending = crate::PendingMigration::open(&path).unwrap();
        let found = pending.inspect_rejections().unwrap();
        assert_eq!(found.total(), 1, "{found:?}");
        assert_eq!(found.tables()[0].table, "structural_blocks");
        pending.repair().unwrap();
        pending.apply().unwrap();
    }
}

#[cfg(test)]
mod baseline_tests {
    use rusqlite::Connection;

    #[test]
    fn baseline_creates_value_blob_and_structural_blocks_and_is_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();
        super::migrate(&mut conn).unwrap();
        let uv: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(
            uv,
            super::LATEST_VERSION,
            "migrate() must reach the latest migration"
        );

        // value_blob exists on tags and defaults to NULL.
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, updated_at) \
             VALUES (CAST('/a.flac' AS BLOB),'flac',0,1,1,0,0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'artist','A',0)",
            [],
        )
        .unwrap();
        let blob_is_null: bool = conn
            .query_row(
                "SELECT value_blob IS NULL FROM tags WHERE track_id=1 AND key='artist'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(blob_is_null);

        // structural_blocks table accepts a row.
        conn.execute(
            "INSERT INTO structural_blocks (track_id, kind, ordinal, body) \
             VALUES (1,'STREAMINFO',0,X'00')",
            [],
        )
        .unwrap();

        // Re-running migrate is a no-op (idempotent).
        super::migrate(&mut conn).unwrap();
        let uv2: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(uv2, super::LATEST_VERSION);
    }

    #[test]
    fn migration_v2_adds_fingerprint_and_content_hash_columns() {
        let mut conn = Connection::open_in_memory().unwrap();
        super::migrate(&mut conn).unwrap();
        assert_eq!(
            conn.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
                .unwrap(),
            super::LATEST_VERSION,
            "migrate() must stamp user_version with the latest migration index"
        );
        // Both columns exist, are nullable, and default to NULL.
        conn.execute(
            "INSERT INTO tracks
                (backing_path, format, audio_offset, audio_length, backing_size,
                 backing_mtime_ns, backing_ctime_ns, updated_at)
             VALUES (CAST('/x.flac' AS BLOB),'flac',0,10,10,0,0,0)",
            [],
        )
        .unwrap();
        let (fp, ch): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT fingerprint, content_hash FROM tracks \
                 WHERE backing_path = CAST('/x.flac' AS BLOB)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(fp, None);
        assert_eq!(ch, None);
    }

    /// V4 retires every fingerprint written under the pre-audio-sampling
    /// algorithm (#691): an upgraded store must not carry values that claim to
    /// be fingerprints the current code no longer produces. It retires every
    /// `content_hash` too: the SHA-256 means what it always did, but a pre-#689
    /// rescan could leave a row's hash describing some earlier bytes, and
    /// nothing in the row tells that apart from a current one.
    #[test]
    fn migration_v4_clears_stale_fingerprints_and_content_hashes() {
        let mut conn = Connection::open_in_memory().unwrap();
        // Stop at V3 and seed a row the way a V3-era scanner would have.
        for (target, migration) in (1i64..).zip(super::MIGRATIONS).take(3) {
            conn.execute_batch(migration.sql).unwrap();
            conn.pragma_update(None, "user_version", target).unwrap();
        }
        conn.execute(
            "INSERT INTO tracks
                (backing_path, format, audio_offset, audio_length, backing_size,
                 backing_mtime_ns, backing_ctime_ns, updated_at, fingerprint, content_hash)
             VALUES ('/x.flac','flac',0,10,10,0,0,0, ?1, ?2)",
            rusqlite::params!["a".repeat(64), "d".repeat(64)],
        )
        .unwrap();

        // `migrate_all`, not `migrate`: V4 is gated, so an ordinary open of a
        // V3 store stops short of it. What this test is about is what the step
        // does once `musefs migrate` runs it.
        super::migrate_all(&mut conn).unwrap();

        let (fp, ch): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT fingerprint, content_hash FROM tracks \
                 WHERE backing_path = CAST('/x.flac' AS BLOB)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(fp, None, "a pre-V4 fingerprint must not be carried forward");
        assert_eq!(
            ch, None,
            "a pre-V4 content_hash must not be carried forward either"
        );
    }

    /// The SQL literal and the exported constant must not drift.
    #[test]
    fn changelog_cap_constant_matches_migration_sql() {
        assert!(super::MIGRATION_V1.contains(&format!("NEW.seq - {}", super::CHANGELOG_CAP)));
    }

    /// The caps a later migration has since widened live in V1/V2 as *frozen
    /// history* — those steps must stay replayable byte-for-byte for the
    /// V1 -> V2 -> V3 upgrade path, so their literals are pinned to the values
    /// they shipped with and deliberately NOT to `crate::limits`. Binding them
    /// to the live constants (as this test once did) makes any future cap
    /// change look like it must be back-edited into released migration text.
    #[test]
    fn superseded_migration_literals_are_frozen() {
        assert!(super::MIGRATION_V1.contains("length(value) <= 262144"));
        assert!(super::MIGRATION_V1.contains("length(description) <= 1024"));
        assert!(
            super::MIGRATION_V2.contains("length(CAST(value AS BLOB)) <= 262144"),
            "V2's byte-accurate rebuild (#505) shipped at the 256 KiB cap"
        );
    }

    /// Every superseded migration, byte for byte.
    ///
    /// The caps above are the literals most likely to be *meaningfully* edited,
    /// but they are three lines out of hundreds, and a released migration has to
    /// stay replayable in its entirety: a store upgrading from V1 replays this
    /// exact text, so an edit here rewrites history that is already on disk
    /// somewhere.
    ///
    /// The failure mode this exists for is not a deliberate edit — it is an
    /// accidental one. V1 through V4 carry near-identical trigger bodies, so a
    /// search-and-replace aimed at the current migration lands in a frozen one
    /// without a word of warning, and the behavioural tests only notice when the
    /// edit happens to change the *final* schema. Twice while writing V4's
    /// `tags`/`track_art` rebuild, they did not.
    ///
    /// A digest mismatch here means an edit reached a frozen migration. The fix
    /// is to move the change to the newest migration, not to update the digest.
    #[test]
    fn superseded_migrations_are_byte_for_byte_frozen() {
        use sha2::{Digest, Sha256};
        for (name, sql, want) in [
            (
                "MIGRATION_V1",
                super::MIGRATION_V1,
                "2f7cac01eae5c107c803466a152fa4659f02b6d43e271fa24c4271b9d56b12c1",
            ),
            (
                "MIGRATION_V2",
                super::MIGRATION_V2,
                "79d14a1ee3b04f4fa8a5d04196637fe2382d23cee102c9ece9e67c08ae10a128",
            ),
            (
                "MIGRATION_V3",
                super::MIGRATION_V3,
                "2eb68a1847c26a8ee862c9c89bdddd2292cbf251145b996dda0a241cc997c035",
            ),
        ] {
            let got = format!(
                "{:x}",
                base16ct::HexDisplay(&Sha256::digest(sql.as_bytes()))
            );
            assert_eq!(
                got, want,
                "{name} is released and replayable, so its text is frozen; put the \
                 change in the newest migration rather than updating this digest"
            );
        }
    }

    /// The literals in the *latest* definition of each table are what a fresh
    /// `migrate()` leaves behind, so those are the ones that must track
    /// `crate::limits`. V4 rebuilds every core table, so every assertion here
    /// reads V4: there is no longer a live definition in a superseded
    /// migration, and asserting against one would only restate what the
    /// byte-for-byte digest test already freezes.
    ///
    /// The ownership moves as tables are rebuilt, and pointing this at a
    /// superseded migration is worse than useless: it would assert against text
    /// the byte-for-byte digest above already freezes, while the definition a
    /// fresh store actually gets drifts unwatched.
    #[test]
    fn check_literals_match_limits_constants() {
        use crate::limits::*;
        let v4 = super::MIGRATION_V4;
        // V4 rebuilds `tags` and `track_art` (#663, #716, #718, #693).
        assert!(v4.contains(&format!("length(key) <= {MAX_TAG_KEY_LEN}")));
        assert!(v4.contains(&format!(
            "length(CAST(value AS BLOB)) <= {MAX_TAG_VALUE_LEN}"
        )));
        assert!(v4.contains(&format!("length(value_blob) <= {MAX_BINARY_TAG_BYTES}")));
        assert!(v4.contains(&format!("length(description) <= {MAX_ART_DESCRIPTION_LEN}")));
        // One home for the cap now that `art` has given the column up (#716):
        // the link that describes the embedding.
        assert_eq!(
            v4.matches(&format!("length(mime) <= {MAX_ART_MIME_LEN}"))
                .count(),
            1,
            "only `track_art` caps a mime"
        );
        // The picture geometry is Option<u32>/u32 in the Rust model, and the
        // upper bound is what stops a schema-valid row being a conversion
        // failure (#718) -- so it is pinned to the type, not to a magic number.
        assert_eq!(
            v4.matches(&format!("BETWEEN 0 AND {}", u32::MAX)).count(),
            4,
            "track_art\'s width, height, depth and colors carry the u32 ceiling"
        );
        // V4 rebuilds `art` too (#718, #719), so its literals moved with it.
        assert!(v4.contains(&format!("length(sha256) = {ART_SHA256_LEN}")));
        assert!(v4.contains(&format!("byte_len <= {MAX_ART_BYTES}")));
        // V4 rebuilds `structural_blocks` too (#732), so the last two assertions
        // that were still reading V1 move with it -- nothing here reads a
        // superseded migration any more.
        assert!(v4.contains(&format!("length(body) <= {MAX_STRUCTURAL_BODY_LEN}")));
        let kinds = STRUCTURAL_KINDS
            .iter()
            .map(|k| format!("'{k}'"))
            .collect::<Vec<_>>()
            .join(",");
        assert!(v4.contains(&format!("kind IN ({kinds})")));
    }
}

#[cfg(test)]
mod changelog_tests {
    use rusqlite::Connection;

    fn count_changes(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM track_changes", [], |r| r.get(0))
            .unwrap()
    }

    /// `backing_path` is a `BLOB` from V4 on (#680), so a migrated store takes
    /// the path's bytes — a TEXT bind fails the storage-class CHECK.
    fn insert_track(conn: &Connection, path: &str) {
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, updated_at) \
             VALUES (?1,'flac',0,1,1,0,0)",
            [path.as_bytes()],
        )
        .unwrap();
    }

    #[test]
    fn v3_changelog_records_insert_update_delete() {
        let mut conn = Connection::open_in_memory().unwrap();
        super::migrate(&mut conn).unwrap();
        let uv: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(uv, super::LATEST_VERSION);

        insert_track(&conn, "/a.flac"); // tracks AI -> 1 row
        assert_eq!(count_changes(&conn), 1);

        conn.execute(
            "UPDATE tracks SET backing_mtime_ns = 1 WHERE id = 1", // tracks AU -> 2 rows (geometry trigger nested UPDATE)
            [],
        )
        .unwrap();
        assert_eq!(count_changes(&conn), 3);

        conn.execute("DELETE FROM tracks WHERE id = 1", []).unwrap(); // tracks AD -> 1 row
        assert_eq!(count_changes(&conn), 4);

        let ids: Vec<i64> = conn
            .prepare("SELECT track_id FROM track_changes ORDER BY seq")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(ids, vec![1, 1, 1, 1]);
    }

    /// Load-bearing nested-trigger dependency (see spec): a bare tag write fires
    /// tags_ai -> UPDATE tracks -> tracks changelog trigger. If this fails, nested
    /// activation is off in this SQLite build; the fix is PRAGMA-level, not schema.
    #[test]
    fn v3_bare_tag_insert_produces_changelog_row_via_nested_trigger() {
        let mut conn = Connection::open_in_memory().unwrap();
        super::migrate(&mut conn).unwrap();
        insert_track(&conn, "/a.flac");
        let before = count_changes(&conn);
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'artist','A',0)",
            [],
        )
        .unwrap();
        assert_eq!(
            count_changes(&conn),
            before + 1,
            "tags_ai's UPDATE tracks must fire the changelog trigger (nested activation)"
        );
        let last_id: i64 = conn
            .query_row(
                "SELECT track_id FROM track_changes ORDER BY seq DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(last_id, 1);
    }

    #[test]
    fn v3_prune_keeps_ring_bounded_and_contiguous() {
        let mut conn = Connection::open_in_memory().unwrap();
        super::migrate(&mut conn).unwrap();
        insert_track(&conn, "/a.flac");
        // Drive CAP + 100 changelog inserts via track updates.
        for i in 0..(super::CHANGELOG_CAP + 100) {
            conn.execute("UPDATE tracks SET backing_mtime_ns = ?1 WHERE id = 1", [i])
                .unwrap();
        }
        let (min_seq, max_seq, rows): (i64, i64, i64) = conn
            .query_row(
                "SELECT MIN(seq), MAX(seq), COUNT(*) FROM track_changes",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            rows,
            super::CHANGELOG_CAP,
            "ring must hold exactly CAP rows"
        );
        assert_eq!(min_seq, max_seq - super::CHANGELOG_CAP + 1, "contiguous");
    }

    #[test]
    fn v4_metadata_edit_bumps_version_and_appends_one_changelog_row() {
        let mut conn = Connection::open_in_memory().unwrap();
        super::migrate(&mut conn).unwrap();
        insert_track(&conn, "/a.flac");
        let cv_before: i64 = conn
            .query_row("SELECT content_version FROM tracks WHERE id=1", [], |r| {
                r.get(0)
            })
            .unwrap();
        let changes_before = count_changes(&conn);

        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'artist','A',0)",
            [],
        )
        .unwrap();

        let cv_after: i64 = conn
            .query_row("SELECT content_version FROM tracks WHERE id=1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(cv_after, cv_before + 1, "content_version must bump by one");
        assert_eq!(
            count_changes(&conn),
            changes_before + 1,
            "exactly one changelog row from the edit (nested trigger)"
        );
    }
}

#[cfg(test)]
mod schema_py_tests {
    use std::fmt::Write as _;

    use rusqlite::Connection;

    use super::MIGRATIONS;

    /// Canonical SQL text: each migration verbatim, preceded by a banner and
    /// followed by the user_version stamp `migrate()` applies after that step.
    /// Equivalent to `migrate()` on a fresh DB only — no fast-path/partial-
    /// upgrade logic — which is what `schema_sql_matches_migrate` proves.
    fn render_schema_sql() -> String {
        let mut sql = String::new();
        for (i, migration) in MIGRATIONS.iter().enumerate() {
            let n = i + 1;
            if i > 0 {
                sql.push('\n');
            }
            // write!/writeln! (not push_str(&format!(..))): the workspace's
            // pedantic clippy lints deny format_push_string, and a bare
            // write! ending in '\n' would trip write_with_newline.
            let _ = write!(sql, "-- ── MIGRATION_V{n} ──");
            sql.push_str(migration.sql); // every MIGRATION_Vn starts and ends with '\n'
            let _ = writeln!(sql, "PRAGMA user_version = {n};");
        }
        sql
    }

    /// Full content of the generated musefs_common/schema.py. Must stay
    /// `ruff format --check`-clean (comment header + two assignments is).
    fn render_schema_py() -> String {
        format!(
            "# GENERATED from musefs-db/src/schema.rs — do not edit.\n\
             # Regenerate: MUSEFS_REGEN_SCHEMA_PY=1 cargo test -p musefs-db schema_py\n\
             # Re-vendor:  python contrib/python-musefs/vendor_to_picard.py\n\
             \n\
             SCHEMA_SQL = \"\"\"\\\n\
             {sql}\"\"\"\n\
             \n\
             USER_VERSION = {version}\n\
             \n\
             # Byte cap on `tags.value`, mirrored so an external writer can check a\n\
             # value before the `CHECK` does. Generated from the Rust constant: it\n\
             # moved once already (#644) and a hand-kept copy would silently rot.\n\
             MAX_TAG_VALUE_LEN = {max_tag_value_len}\n",
            sql = render_schema_sql(),
            version = MIGRATIONS.len(),
            max_tag_value_len = crate::limits::MAX_TAG_VALUE_LEN
        )
    }

    fn dump_master(conn: &Connection) -> Vec<(String, String, String, Option<String>)> {
        conn.prepare("SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY type, name")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn user_version(conn: &Connection) -> i64 {
        conn.pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap()
    }

    /// The rendering must stay semantically identical to migrate() on a fresh
    /// DB — guards against migrate() ever growing a non-SQL step the
    /// concatenation cannot represent.
    #[test]
    fn schema_sql_matches_migrate() {
        let rendered = Connection::open_in_memory().unwrap();
        rendered.execute_batch(&render_schema_sql()).unwrap();

        let mut migrated = Connection::open_in_memory().unwrap();
        super::migrate(&mut migrated).unwrap();

        assert_eq!(dump_master(&rendered), dump_master(&migrated));
        assert_eq!(user_version(&rendered), user_version(&migrated));
        assert_eq!(user_version(&rendered), super::LATEST_VERSION);
    }

    #[test]
    fn migrate_does_not_reapply_an_already_applied_step() {
        // A DB sitting at version 1 (only V1 applied) must receive ONLY the
        // remaining steps on the next migrate. `current < target` skips the
        // already-applied V1; `<=` would re-run V1 (`CREATE TABLE tracks` ->
        // "table already exists"), so a clean upgrade to the latest version proves
        // the loop never re-applies a step it already ran. (The current==latest
        // case can't exercise this — migrate fast-paths out before the loop.)
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(MIGRATIONS[0].sql).unwrap(); // apply V1 only
        conn.pragma_update(None, "user_version", 1i64).unwrap();
        super::migrate_all(&mut conn)
            .expect("upgrading from v1 must apply only the remaining steps");
        assert_eq!(user_version(&conn), super::LATEST_VERSION);
    }

    #[test]
    fn v2_rebuild_enforces_byte_cap_and_drops_oversize_rows() {
        // #505: V2 rebuilds `tags` with a byte-accurate value cap. Simulate a v1
        // store, plant an over-cap multibyte value (legal under V1's char-counting
        // CHECK: 150_000 chars / 300_000 bytes) plus a normal one, then upgrade.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(MIGRATIONS[0].sql).unwrap(); // V1 only
        conn.pragma_update(None, "user_version", 1i64).unwrap();
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, backing_ctime_ns, updated_at) \
             VALUES ('/a.flac','flac',0,0,0,0,0,0)",
            [],
        )
        .unwrap();
        let big = "é".repeat(150_000);
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'big',?1,0)",
            rusqlite::params![big],
        )
        .expect("V1 char-counting CHECK accepts a 150_000-char value");
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'ok','fine',0)",
            [],
        )
        .unwrap();

        // V2 only, applied directly. `migrate()` would run on through V3, whose
        // widened cap (#644) accepts this value — that is the later step's
        // business, asserted separately below. This test is about what V2 did.
        conn.execute_batch(MIGRATIONS[1].sql).unwrap();
        conn.pragma_update(None, "user_version", 2i64).unwrap();

        // The over-cap row is dropped; the valid row survives.
        let keys: Vec<String> = {
            let mut stmt = conn.prepare("SELECT key FROM tags ORDER BY key").unwrap();
            let rows = stmt.query_map([], |r| r.get(0)).unwrap();
            rows.collect::<rusqlite::Result<_>>().unwrap()
        };
        assert_eq!(keys, vec!["ok".to_string()]);

        // The rebuilt CHECK rejects an over-cap multibyte value at write.
        assert!(
            conn.execute(
                "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'big2',?1,0)",
                rusqlite::params![big],
            )
            .is_err(),
            "byte-accurate CHECK must reject the write"
        );

        // The tag triggers were recreated: an insert still bumps content_version.
        let cv = |c: &Connection| -> i64 {
            c.query_row("SELECT content_version FROM tracks WHERE id=1", [], |r| {
                r.get(0)
            })
            .unwrap()
        };
        let before = cv(&conn);
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'extra','v',0)",
            [],
        )
        .unwrap();
        assert!(
            cv(&conn) > before,
            "tags_ai trigger must survive the rebuild"
        );
    }

    /// #644: V3 widens `tags.value` to FLAC's block ceiling and
    /// `track_art.description` to 8 KiB. Both are widenings, so unlike V2's
    /// narrowing the rebuild must carry every existing row across — losing user
    /// tags to a migration that only relaxes a limit would be gratuitous.
    #[test]
    fn v3_rebuild_widens_caps_and_preserves_rows() {
        use crate::limits::{MAX_ART_DESCRIPTION_LEN, MAX_TAG_VALUE_LEN};
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(MIGRATIONS[0].sql).unwrap();
        conn.execute_batch(MIGRATIONS[1].sql).unwrap();
        conn.pragma_update(None, "user_version", 2i64).unwrap();
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, backing_ctime_ns, updated_at) \
             VALUES ('/a.flac','flac',0,0,0,0,0,0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO art (sha256, mime, width, height, byte_len, data) \
             VALUES (?1,'image/png',1,1,1,X'00')",
            [&"a".repeat(64)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'artist','A',0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO track_art (track_id, art_id, picture_type, description, ordinal) \
             VALUES (1,1,3,'cover',0)",
            [],
        )
        .unwrap();

        super::migrate_all(&mut conn).expect("upgrade to v3");

        assert_eq!(
            conn.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
                .unwrap(),
            super::LATEST_VERSION
        );
        // Rows survive: a widening must never drop data.
        let (key, value): (String, String) = conn
            .query_row("SELECT key, value FROM tags", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((key.as_str(), value.as_str()), ("artist", "A"));
        let desc: String = conn
            .query_row("SELECT description FROM track_art", [], |r| r.get(0))
            .unwrap();
        assert_eq!(desc, "cover");

        // A value the V2 cap rejected now writes cleanly — the point of #644.
        let over_v2 = "é".repeat(150_000);
        assert!(
            over_v2.len() > 262_144 && i64::try_from(over_v2.len()).unwrap() < MAX_TAG_VALUE_LEN
        );
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'lyrics',?1,0)",
            rusqlite::params![over_v2],
        )
        .expect("V3 accepts a value the 256 KiB cap rejected");
        conn.execute(
            "INSERT INTO track_art (track_id, art_id, picture_type, description, ordinal) \
             VALUES (1,1,3,?1,1)",
            rusqlite::params!["d".repeat(2048)],
        )
        .expect("V3 accepts a description the 1 KiB cap rejected");

        // The triggers and the reverse-edge index came back with the rebuild.
        let objects: Vec<String> = {
            let mut stmt = conn
                .prepare(
                    "SELECT name FROM sqlite_master \
                     WHERE name IN ('tags_ai','tags_au','tags_ad','track_art_ai', \
                                    'track_art_au','track_art_ad','art_ad', \
                                    'track_art_art_id_idx') ORDER BY name",
                )
                .unwrap();
            let rows = stmt.query_map([], |r| r.get(0)).unwrap();
            rows.collect::<rusqlite::Result<_>>().unwrap()
        };
        assert_eq!(
            objects,
            vec![
                "art_ad",
                "tags_ad",
                "tags_ai",
                "tags_au",
                "track_art_ad",
                "track_art_ai",
                "track_art_art_id_idx",
                "track_art_au",
            ],
            "V3 must restore every object its two DROP TABLEs took with them"
        );
        // `art_ad` reads `track_art`; a rebuild that renamed the table out from
        // under that trigger leaves a body that errors at *fire* time, not at
        // migration time, so the object-name check above would not catch it.
        // Fire it, in the exact shape it was written for: an art row deleted
        // while track_art still references it (only reachable with FKs off).
        conn.pragma_update(None, "foreign_keys", false).unwrap();
        conn.execute("DELETE FROM art WHERE id = 1", [])
            .expect("art_ad must still resolve track_art after the V3 rebuild");
        let bumped: i64 = conn
            .query_row("SELECT content_version FROM tracks WHERE id=1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(bumped > 0, "art_ad must bump the referencing track");
        assert_eq!(MAX_ART_DESCRIPTION_LEN, 8192);
    }

    /// NOT #[ignore]d on purpose: the compare path must run under plain
    /// `cargo test` or the CI drift gate doesn't exist. Only the write
    /// behavior is env-gated.
    #[test]
    fn schema_py_fixture_is_fresh() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../contrib/python-musefs/src/musefs_common/schema.py");
        let rendered = render_schema_py();
        if std::env::var_os("MUSEFS_REGEN_SCHEMA_PY").is_some() {
            std::fs::write(&path, &rendered).expect("write schema.py");
            return;
        }
        let on_disk = std::fs::read_to_string(&path).expect(
            "musefs_common/schema.py missing — regenerate with \
             MUSEFS_REGEN_SCHEMA_PY=1 cargo test -p musefs-db schema_py",
        );
        assert_eq!(
            on_disk, rendered,
            "musefs_common/schema.py is stale. Regenerate: \
             MUSEFS_REGEN_SCHEMA_PY=1 cargo test -p musefs-db schema_py, \
             then: python contrib/python-musefs/vendor_to_picard.py"
        );
    }
}

#[cfg(test)]
mod constraint_tests {
    use rusqlite::Connection;

    /// A fresh, fully-migrated DB with foreign_keys ON — mirrors how
    /// `Db::configure` opens the real connection (lib.rs:78).
    fn fresh(conn: &mut Connection) {
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        super::migrate(conn).unwrap();
    }

    /// `backing_path` is a `BLOB` from V4 on (#680), so a migrated store takes
    /// the path's bytes — a TEXT bind fails the storage-class CHECK.
    fn insert_track(conn: &Connection, path: &str) {
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, updated_at) \
             VALUES (?1,'flac',0,1,1,0,0)",
            [path.as_bytes()],
        )
        .unwrap();
    }

    /// A complete, valid row across all four tables migrates and reads back.
    #[test]
    fn v4_valid_rows_migrate_and_read_cleanly() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        let uv: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(uv, super::LATEST_VERSION);

        insert_track(&conn, "/a.flac");
        conn.execute(
            "INSERT INTO art (sha256, byte_len, data) \
             VALUES (?1, 1, X'00')",
            [&"a".repeat(64)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'artist','A',0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO track_art (track_id, art_id, picture_type, ordinal) \
             VALUES (1,1,3,0)",
            [],
        )
        .unwrap();

        let (off, len, sz): (i64, i64, i64) = conn
            .query_row(
                "SELECT audio_offset, audio_length, backing_size FROM tracks WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((off, len, sz), (0, 1, 1));
        let pic: i64 = conn
            .query_row(
                "SELECT picture_type FROM track_art WHERE track_id=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pic, 3);
    }

    /// SQL expression yielding `n` copies of `c`, for cap-boundary inserts.
    /// `hex(zeroblob(k))` is `2k` ASCII '0's, so one `replace` plus a `substr`
    /// builds any length without materializing a multi-MiB Rust string just to
    /// interpolate it into a statement.
    fn repeated_char_sql(c: char, n: i64) -> String {
        let half = n / 2 + n % 2;
        format!("substr(replace(hex(zeroblob({half})), '0', '{c}'), 1, {n})")
    }

    fn rejected(conn: &Connection, sql: &str) {
        assert!(
            conn.execute(sql, []).is_err(),
            "expected rejection for: {sql}"
        );
    }

    #[test]
    fn v4_tracks_rejects_unknown_format() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        rejected(
            &conn,
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, updated_at) \
             VALUES ('/x','aiff',0,0,0,0,0)",
        );
    }

    #[test]
    fn v4_tracks_accepts_every_pinned_format() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        for (i, fmt) in ["flac", "mp3", "m4a", "opus", "vorbis", "oggflac", "wav"]
            .iter()
            .enumerate()
        {
            conn.execute(
                "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
                 backing_size, backing_mtime_ns, updated_at) \
                 VALUES (?1, ?2, 0, 0, 0, 0, 0)",
                rusqlite::params![format!("/t{i}").into_bytes(), fmt],
            )
            .unwrap();
        }
    }

    #[test]
    fn v4_tracks_rejects_negative_audio_offset() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        rejected(
            &conn,
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, updated_at) \
             VALUES (CAST('/x' AS BLOB),'flac',-1,0,0,0,0)",
        );
    }

    #[test]
    fn v4_tracks_rejects_negative_audio_length() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        rejected(
            &conn,
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, updated_at) \
             VALUES ('/x','flac',0,-1,0,0,0)",
        );
    }

    #[test]
    fn v4_tracks_rejects_negative_backing_size() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        rejected(
            &conn,
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, updated_at) \
             VALUES ('/x','flac',0,0,-1,0,0)",
        );
    }

    #[test]
    fn v4_tracks_rejects_negative_backing_mtime_ns() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        rejected(
            &conn,
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, updated_at) \
             VALUES ('/x','flac',0,0,0,-1,0)",
        );
    }

    #[test]
    fn v4_tracks_rejects_negative_content_version() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        rejected(
            &conn,
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, content_version, updated_at) \
             VALUES ('/x','flac',0,0,0,0,-1,0)",
        );
    }

    #[test]
    fn v4_tracks_rejects_negative_updated_at() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        rejected(
            &conn,
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, updated_at) \
             VALUES ('/x','flac',0,0,0,0,-1)",
        );
    }

    #[test]
    fn v4_tracks_rejects_audio_range_past_backing_size() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        rejected(
            &conn,
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, updated_at) \
             VALUES (CAST('/x' AS BLOB),'flac',5,10,14,0,0)",
        );
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, updated_at) \
             VALUES (CAST('/ok' AS BLOB),'flac',5,10,15,0,0)",
            [],
        )
        .unwrap();
    }

    #[test]
    fn v4_tracks_rejects_update_pushing_audio_past_backing() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        insert_track(&conn, "/x.flac");
        rejected(&conn, "UPDATE tracks SET backing_size = 0 WHERE id = 1");
    }

    fn seed_track_and_art(conn: &Connection) {
        insert_track(conn, "/seed.flac");
        conn.execute(
            "INSERT INTO art (sha256, byte_len, data) VALUES (?1, 1, X'00')",
            [&"c".repeat(64)],
        )
        .unwrap();
    }

    #[test]
    fn v4_tags_rejects_negative_ordinal() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        seed_track_and_art(&conn);
        rejected(
            &conn,
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'artist','A',-1)",
        );
    }

    #[test]
    fn v4_tags_rejects_blob_with_nonempty_value() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        seed_track_and_art(&conn);
        rejected(
            &conn,
            "INSERT INTO tags (track_id, key, value, ordinal, value_blob) \
             VALUES (1,'cover','nonempty',0,X'00')",
        );
    }

    #[test]
    fn v4_tags_accepts_blob_with_empty_value() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        seed_track_and_art(&conn);
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal, value_blob) \
             VALUES (1,'cover','',0,X'00')",
            [],
        )
        .unwrap();
    }

    #[test]
    fn v4_tags_accepts_empty_text_value_without_blob() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        seed_track_and_art(&conn);
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'comment','',0)",
            [],
        )
        .unwrap();
    }

    #[test]
    fn v4_art_rejects_byte_len_mismatch() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        rejected(
            &conn,
            "INSERT INTO art (sha256, byte_len, data) \
             VALUES ('aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 5, X'00')",
        );
    }

    #[test]
    fn v4_art_rejects_sha256_wrong_length() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        rejected(
            &conn,
            "INSERT INTO art (sha256, byte_len, data) \
             VALUES ('tooshort', 1, X'00')",
        );
    }

    // The geometry these two guard moved to `track_art` with the rest of what
    // describes one embedding (#716), so they follow it: the bound is the same
    // `BETWEEN 0 AND u32::MAX`, only its owner changed.
    #[test]
    fn v4_track_art_rejects_negative_width() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        seed_track_and_art(&conn);
        rejected(
            &conn,
            "INSERT INTO track_art (track_id, art_id, picture_type, ordinal, width) \
             VALUES (1, 1, 3, 0, -1)",
        );
    }

    #[test]
    fn v4_track_art_rejects_negative_height() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        seed_track_and_art(&conn);
        rejected(
            &conn,
            "INSERT INTO track_art (track_id, art_id, picture_type, ordinal, height) \
             VALUES (1, 1, 3, 0, -1)",
        );
    }

    #[test]
    fn v4_track_art_rejects_picture_type_above_range() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        seed_track_and_art(&conn);
        rejected(
            &conn,
            "INSERT INTO track_art (track_id, art_id, picture_type, ordinal) \
             VALUES (1,1,21,0)",
        );
    }

    #[test]
    fn v4_track_art_rejects_negative_picture_type() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        seed_track_and_art(&conn);
        rejected(
            &conn,
            "INSERT INTO track_art (track_id, art_id, picture_type, ordinal) \
             VALUES (1,1,-1,0)",
        );
    }

    #[test]
    fn v4_track_art_accepts_picture_type_bounds() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        seed_track_and_art(&conn);
        conn.execute(
            "INSERT INTO track_art (track_id, art_id, picture_type, ordinal) \
             VALUES (1,1,0,0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO track_art (track_id, art_id, picture_type, ordinal) \
             VALUES (1,1,20,1)",
            [],
        )
        .unwrap();
    }

    #[test]
    fn v4_track_art_rejects_negative_ordinal() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        seed_track_and_art(&conn);
        rejected(
            &conn,
            "INSERT INTO track_art (track_id, art_id, picture_type, ordinal) \
             VALUES (1,1,3,-1)",
        );
    }

    #[test]
    fn v4_tags_rejects_oversize_key() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        insert_track(&conn, "/a.flac");
        let key = "k".repeat(257);
        rejected(
            &conn,
            &format!(
                "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1, '{key}', 'v', 0)"
            ),
        );
    }

    #[test]
    fn v4_tags_accepts_key_at_cap() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        insert_track(&conn, "/a.flac");
        let key = "k".repeat(256);
        conn.execute(
            &format!(
                "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1, '{key}', 'v', 0)"
            ),
            [],
        )
        .unwrap();
    }

    #[test]
    fn v4_tags_rejects_oversize_value() {
        use crate::limits::MAX_TAG_VALUE_LEN;
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        insert_track(&conn, "/a.flac");
        // Built in SQL rather than Rust: at the post-#644 cap the string is
        // 16 MiB, and interpolating one into a statement costs twice that for a
        // test whose whole point is the `<=` boundary.
        let over = repeated_char_sql('v', MAX_TAG_VALUE_LEN + 1);
        rejected(
            &conn,
            &format!("INSERT INTO tags (track_id, key, value, ordinal) VALUES (1, 'k', {over}, 0)"),
        );
    }

    /// The widened cap (#644) accepts exactly at the boundary, so the pair pins
    /// the `CHECK`'s `<=` against an off-by-one in either direction.
    #[test]
    fn v4_tags_accepts_value_at_cap() {
        use crate::limits::MAX_TAG_VALUE_LEN;
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        insert_track(&conn, "/a.flac");
        let at = repeated_char_sql('v', MAX_TAG_VALUE_LEN);
        conn.execute_batch(&format!(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1, 'k', {at}, 0)"
        ))
        .unwrap();
        let len: i64 = conn
            .query_row("SELECT length(CAST(value AS BLOB)) FROM tags", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(len, MAX_TAG_VALUE_LEN);
    }

    #[test]
    fn v4_structural_rejects_unknown_kind_and_negative_ordinal_and_oversize_body() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        insert_track(&conn, "/a.flac");
        rejected(
            &conn,
            "INSERT INTO structural_blocks (track_id, kind, ordinal, body) VALUES (1, 'APPLICATION', 0, X'00')",
        );
        rejected(
            &conn,
            "INSERT INTO structural_blocks (track_id, kind, ordinal, body) VALUES (1, 'STREAMINFO', -1, X'00')",
        );
        // length(body) cap: a blob of MAX+1 zero bytes via zeroblob().
        rejected(
            &conn,
            "INSERT INTO structural_blocks (track_id, kind, ordinal, body) VALUES (1, 'STREAMINFO', 0, zeroblob(16777216))",
        );
    }

    #[test]
    fn v4_structural_accepts_body_at_cap() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        insert_track(&conn, "/a.flac");
        conn.execute(
            "INSERT INTO structural_blocks (track_id, kind, ordinal, body) VALUES (1, 'STREAMINFO', 0, zeroblob(16777215))",
            [],
        )
        .unwrap();
    }

    #[test]
    fn v4_art_rejects_oversize_byte_len() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        // The mime half of this test went with the column (#716): the cap now
        // lives on `track_art`, where `v4_track_art_*` covers it.
        // byte_len cap (byte_len must equal length(data), so use a zeroblob).
        rejected(
            &conn,
            &format!(
                "INSERT INTO art (sha256, byte_len, data) VALUES ('{}', 16711681, zeroblob(16711681))",
                "b".repeat(64)
            ),
        );
    }

    #[test]
    fn v4_track_art_rejects_oversize_description() {
        use crate::limits::MAX_ART_DESCRIPTION_LEN;
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        seed_track_and_art(&conn);
        let over = repeated_char_sql('d', MAX_ART_DESCRIPTION_LEN + 1);
        rejected(
            &conn,
            &format!(
                "INSERT INTO track_art (track_id, art_id, picture_type, description, ordinal) VALUES (1, 1, 3, {over}, 0)"
            ),
        );
    }

    #[test]
    fn v4_track_art_accepts_description_at_cap() {
        use crate::limits::MAX_ART_DESCRIPTION_LEN;
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        seed_track_and_art(&conn);
        let at = repeated_char_sql('d', MAX_ART_DESCRIPTION_LEN);
        conn.execute_batch(&format!(
            "INSERT INTO track_art (track_id, art_id, picture_type, description, ordinal) VALUES (1, 1, 3, {at}, 0)"
        ))
        .unwrap();
        let len: i64 = conn
            .query_row("SELECT length(description) FROM track_art", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(len, MAX_ART_DESCRIPTION_LEN);
    }

    #[test]
    fn fresh_db_has_all_baseline_triggers() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        let names: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='trigger' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for expected in [
            "tags_ai",
            "tags_au",
            "tags_ad",
            "track_art_ai",
            "track_art_au",
            "track_art_ad",
            "tracks_changelog_ai",
            "tracks_changelog_au",
            "tracks_changelog_ad",
            "track_changes_prune",
            "art_reject_content_update",
            "art_ad",
            "tracks_geometry_au",
            "structural_blocks_ai",
            "structural_blocks_ad",
            "tags_reject_reparent",
            "track_art_reject_reparent",
            "tracks_reject_rekey",
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "missing trigger on fresh DB: {expected}"
            );
        }
        assert_eq!(names.len(), 18, "unexpected trigger count: {names:?}");
    }

    #[test]
    fn v4_tags_rejects_empty_and_control_char_keys() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, updated_at) \
             VALUES (CAST('/x' AS BLOB),'flac',0,0,0,0,0)",
            [],
        )
        .unwrap();
        rejected(
            &conn,
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'','v',0)",
        );
        rejected(
            &conn,
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,char(7),'v',0)",
        );
        // '=' is NOT a DB-floor violation — only Vorbis synthesis bars it.
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'a=b','c',0)",
            [],
        )
        .unwrap();
    }

    #[test]
    fn v2_fingerprint_check_rejects_wrong_length_and_accepts_null_and_64_chars() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        insert_track(&conn, "/fp.flac");

        // NULL is accepted (no fingerprint yet).
        conn.execute(
            "UPDATE tracks SET fingerprint = NULL WHERE backing_path = CAST('/fp.flac' AS BLOB)",
            [],
        )
        .unwrap();

        // A valid 64-char SHA-256 hex string is accepted.
        conn.execute(
            &format!(
                "UPDATE tracks SET fingerprint = '{}' WHERE backing_path = CAST('/fp.flac' AS BLOB)",
                "a".repeat(64)
            ),
            [],
        )
        .unwrap();

        // A too-short fingerprint (1 char) is rejected.
        rejected(
            &conn,
            "UPDATE tracks SET fingerprint = 'x' WHERE backing_path = CAST('/fp.flac' AS BLOB)",
        );

        // A too-long fingerprint (65 chars) is also rejected.
        rejected(
            &conn,
            &format!(
                "UPDATE tracks SET fingerprint = '{}' WHERE backing_path = CAST('/fp.flac' AS BLOB)",
                "a".repeat(65)
            ),
        );
    }
}

#[cfg(test)]
mod identity_tests {
    use super::*;
    use crate::error::DbError;

    fn migrated() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        migrate(&mut conn).unwrap();
        conn
    }

    #[test]
    fn honest_schema_passes() {
        let conn = migrated();
        validate_identity(&conn).unwrap();
    }

    #[test]
    fn honest_schema_with_rows_passes() {
        let conn = migrated();
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, updated_at) \
             VALUES (CAST('/a.flac' AS BLOB),'flac',0,1,1,0,0)",
            [],
        )
        .unwrap();
        let has_seq: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'sqlite_sequence'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_seq, 1, "precondition: insert created sqlite_sequence");
        validate_identity(&conn).unwrap();
    }

    #[test]
    fn missing_trigger_is_rejected() {
        let conn = migrated();
        conn.execute_batch("DROP TRIGGER tags_ai").unwrap();
        let err = validate_identity(&conn).unwrap_err();
        match err {
            DbError::SchemaMismatch { object } => {
                assert!(object.contains("tags_ai"), "names the object: {object}");
                assert!(object.contains("missing"), "classifies it: {object}");
            }
            other => panic!("expected SchemaMismatch, got {other:?}"),
        }
    }

    #[test]
    fn extra_object_is_rejected() {
        let conn = migrated();
        conn.execute_batch("CREATE TABLE sneaky (x)").unwrap();
        let err = validate_identity(&conn).unwrap_err();
        assert!(matches!(err, DbError::SchemaMismatch { .. }));
    }

    #[test]
    fn altered_table_is_rejected() {
        let conn = migrated();
        conn.execute_batch(
            "PRAGMA foreign_keys=OFF; \
             DROP TABLE tags; \
             CREATE TABLE tags (track_id INTEGER NOT NULL, key TEXT, value TEXT, \
                ordinal INTEGER, value_blob BLOB, PRIMARY KEY (track_id, key, ordinal));",
        )
        .unwrap();
        let err = validate_identity(&conn).unwrap_err();
        match err {
            DbError::SchemaMismatch { object } => assert!(object.contains("tags")),
            other => panic!("expected SchemaMismatch, got {other:?}"),
        }
    }

    #[test]
    fn altered_object_with_no_other_diffs_is_rejected() {
        // `art` has no triggers and (when empty) no FK children to cascade, so
        // recreating it with a different shape makes the *altered* table the
        // ONLY schema difference — isolating the `r != a` guard so a
        // `r != a -> false` mutant cannot survive on the back of an unrelated
        // missing/extra object.
        let conn = migrated();
        conn.execute_batch(
            "PRAGMA foreign_keys=OFF; \
             DROP TABLE art; \
             CREATE TABLE art (id INTEGER PRIMARY KEY, sha256 TEXT, mime TEXT, \
                width INTEGER, height INTEGER, byte_len INTEGER, data BLOB);",
        )
        .unwrap();
        let err = validate_identity(&conn).unwrap_err();
        match err {
            DbError::SchemaMismatch { object } => {
                assert!(object.contains("art"), "names the object: {object}");
                assert!(
                    object.contains("altered"),
                    "classifies it as altered: {object}"
                );
            }
            other => panic!("expected SchemaMismatch (altered), got {other:?}"),
        }
    }

    #[test]
    fn foreign_key_violation_is_rejected() {
        let conn = migrated();
        conn.execute_batch(
            "PRAGMA foreign_keys=OFF; \
             INSERT INTO art (sha256, byte_len, data) \
             VALUES ('aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 1, X'00'); \
             INSERT INTO track_art (track_id, art_id, picture_type, ordinal) VALUES (999, 1, 3, 0);",
        )
        .unwrap();
        let err = validate_identity(&conn).unwrap_err();
        match err {
            DbError::SchemaMismatch { object } => assert!(object.contains("foreign key")),
            other => panic!("expected SchemaMismatch (fk), got {other:?}"),
        }
    }

    #[test]
    fn first_offender_is_deterministic_in_type_name_order() {
        let conn = migrated();
        conn.execute_batch(
            "PRAGMA foreign_keys=OFF; DROP TRIGGER track_art_ai; DROP TRIGGER tags_ai;",
        )
        .unwrap();
        let err = validate_identity(&conn).unwrap_err();
        match err {
            DbError::SchemaMismatch { object } => assert!(object.contains("tags_ai"), "{object}"),
            other => panic!("expected SchemaMismatch, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod art_immutability_tests {
    use rusqlite::{Connection, params};

    /// A fresh, fully-migrated DB with `foreign_keys` OFF — that is what lets
    /// `deleting_referenced_art_bumps_tracks` produce the orphan case.
    fn migrated() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        super::migrate(&mut conn).unwrap();
        conn.pragma_update(None, "foreign_keys", false).unwrap();
        conn
    }

    fn insert_track(conn: &Connection, path: &str) -> i64 {
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, updated_at) \
             VALUES (?1,'flac',0,1,1,0,0)",
            [path.as_bytes()],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn insert_art(conn: &Connection, sha: &str, data: &[u8]) -> i64 {
        conn.execute(
            "INSERT INTO art (sha256, byte_len, data) \
             VALUES (?1, ?2, ?3)",
            params![sha, i64::try_from(data.len()).unwrap(), data],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn migration_reaches_latest_user_version() {
        let conn = migrated();
        let uv: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(uv, super::LATEST_VERSION);
    }

    #[test]
    fn art_content_update_is_rejected() {
        let conn = migrated();
        let a = insert_art(&conn, &"a".repeat(64), &[1, 2, 3]);
        assert!(
            conn.execute("UPDATE art SET mime='image/jpeg' WHERE id=?1", [a])
                .is_err()
        );
        assert!(
            conn.execute("UPDATE art SET byte_len=99 WHERE id=?1", [a])
                .is_err()
        );
        assert!(
            conn.execute("UPDATE art SET data=X'04050607' WHERE id=?1", [a])
                .is_err()
        );
        assert!(
            conn.execute("UPDATE art SET width=10 WHERE id=?1", [a])
                .is_err()
        );
        assert!(
            conn.execute(
                "UPDATE art SET sha256=?1 WHERE id=?2",
                params![&"b".repeat(64), a],
            )
            .is_err()
        );
    }

    #[test]
    fn art_noop_update_is_allowed() {
        let conn = migrated();
        let a = insert_art(&conn, &"a".repeat(64), &[1, 2, 3]);
        conn.execute("UPDATE art SET sha256=sha256 WHERE id=?1", [a])
            .unwrap();
    }

    #[test]
    fn deleting_referenced_art_bumps_tracks() {
        let conn = migrated();
        let t = insert_track(&conn, "/a.flac");
        let a = insert_art(&conn, &"a".repeat(64), &[1, 2, 3]);
        conn.execute(
            "INSERT INTO track_art (track_id, art_id, picture_type, ordinal) \
             VALUES (?1,?2,3,0)",
            [t, a],
        )
        .unwrap();
        let cv0: i64 = conn
            .query_row("SELECT content_version FROM tracks WHERE id=?1", [t], |r| {
                r.get(0)
            })
            .unwrap();
        conn.execute("DELETE FROM art WHERE id=?1", [a]).unwrap();
        let cv1: i64 = conn
            .query_row("SELECT content_version FROM tracks WHERE id=?1", [t], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(cv1, cv0 + 1, "art delete must bump the referencing track");
    }

    #[test]
    fn deleting_unreferenced_art_bumps_nothing() {
        let conn = migrated();
        let t = insert_track(&conn, "/a.flac");
        let a = insert_art(&conn, &"a".repeat(64), &[1, 2, 3]);
        let cv0: i64 = conn
            .query_row("SELECT content_version FROM tracks WHERE id=?1", [t], |r| {
                r.get(0)
            })
            .unwrap();
        conn.execute("DELETE FROM art WHERE id=?1", [a]).unwrap();
        let cv1: i64 = conn
            .query_row("SELECT content_version FROM tracks WHERE id=?1", [t], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(cv1, cv0, "deleting an unreferenced art row must not bump");
    }
}

/// `tracks.id` is the identity the incremental refresh keys on (#678), so it
/// cannot be rewritten in place (#762).
#[cfg(test)]
mod track_id_immutability_tests {
    use rusqlite::Connection;

    /// Foreign keys ON, as `Db::configure` opens the real connection. A
    /// childless track is the shape enforcement does not protect: with no
    /// child to reference the old id, nothing but the trigger stands in the way.
    fn migrated_with_track() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        super::migrate(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, updated_at) \
             VALUES (CAST('/a.flac' AS BLOB),'flac',0,1,1,0,0)",
            [],
        )
        .unwrap();
        conn.execute("DELETE FROM track_changes", []).unwrap();
        conn
    }

    fn logged(conn: &Connection) -> Vec<i64> {
        conn.prepare("SELECT track_id FROM track_changes ORDER BY seq")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn a_rekey_is_refused() {
        let conn = migrated_with_track();
        let err = conn
            .execute("UPDATE tracks SET id = 99 WHERE id = 1", [])
            .unwrap_err()
            .to_string();
        assert!(err.contains("track ids are immutable"), "{err}");
    }

    /// The `WHEN` guard: `BEFORE UPDATE OF id` fires whenever the column is
    /// named in a `SET` list, so a writer rewriting a row wholesale without
    /// moving it must still get through.
    #[test]
    fn naming_id_without_changing_it_is_allowed() {
        let conn = migrated_with_track();
        conn.execute("UPDATE tracks SET id = id, updated_at = 5 WHERE id = 1", [])
            .unwrap();
        assert_eq!(
            logged(&conn),
            vec![1],
            "an ordinary update spends one ring slot, not two"
        );
    }

    /// The changelog is correct on its own terms: against a writer that has
    /// dropped the refusal, a rekey still names the id that went away, which
    /// is the one the incremental refresh has to remove.
    #[test]
    fn with_the_refusal_dropped_a_rekey_logs_both_ids() {
        let conn = migrated_with_track();
        conn.execute_batch("DROP TRIGGER tracks_reject_rekey")
            .unwrap();
        conn.execute("UPDATE tracks SET id = 99 WHERE id = 1", [])
            .unwrap();
        assert_eq!(logged(&conn), vec![1, 99]);
    }
}
