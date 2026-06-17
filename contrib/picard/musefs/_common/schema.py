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
"""

USER_VERSION = 2
