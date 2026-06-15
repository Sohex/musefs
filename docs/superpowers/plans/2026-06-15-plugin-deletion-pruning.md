# Plugin Deletion Pruning Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the beets and Lidarr integrations prune musefs store rows when the source tool reports a deletion, so the mount stops presenting files the source no longer tracks.

**Architecture:** Two mechanisms, forced by each source's file topology. beets manages audio files in place, so `beet remove -d` deletes the backing file and the existing existence-based `prune_missing` is correct — we only need a removal to *trigger* the end-of-command reconcile. Lidarr never touches the backing directory (it only unlinks its own symlink tree), so existence-based pruning can never fire; instead we map Lidarr's `AlbumDeleted`/`ArtistDeleted` events to rows by the MusicBrainz id already stored as a tag and delete them outright. Picard is out of scope (no deletion concept). No `musefs-core`/scan or schema changes.

**Tech Stack:** Python 3 (contrib plugins), `python-musefs` shared store-contract library, SQLite, pytest, beets plugin API, Lidarr custom-script API.

**Spec:** `docs/superpowers/specs/2026-06-15-plugin-deletion-pruning-design.md`

---

## Environment setup (do this once before starting)

The contrib Python suites run in per-package virtualenvs with editable installs (see each package's README and `CONTRIBUTING.md#python-plugins-contrib`). The pre-commit hook runs `ruff` over Python paths and the full Rust workspace tests, but **not** the contrib pytest suites — those are CI gates, so run them locally per task.

- **python-musefs** — from `contrib/python-musefs`: `python -m venv .venv && . .venv/bin/activate && pip install -e ".[test]"`. Run tests with `python -m pytest`.
- **lidarr** — from `contrib/lidarr`: `python -m venv .venv && . .venv/bin/activate && pip install -e ../python-musefs && pip install -e ".[test]"`. Run tests with `python -m pytest`.
- **beets** — a venv already exists at `contrib/beets/.venv` (system Python is PEP 668 externally managed). Use `contrib/beets/.venv/bin/python -m pytest` and `contrib/beets/.venv/bin/pip`. If editable installs are stale, re-run `contrib/beets/.venv/bin/pip install -e ../python-musefs && contrib/beets/.venv/bin/pip install -e ".[test]"` from `contrib/beets`.

In the steps below, "run pytest" means the appropriate invocation above for that package.

## File structure

| File | Responsibility | Change |
| --- | --- | --- |
| `contrib/python-musefs/src/musefs_common/store.py` | Shared store-contract primitives | Add `track_ids_by_tag`, `delete_tracks` |
| `contrib/python-musefs/src/musefs_common/__init__.py` | Public API surface | Export the two new names |
| `contrib/python-musefs/tests/test_store_db.py` | Store unit tests | Add tests for the two new helpers |
| `contrib/python-musefs/tests/test_public_api.py` | Public-API contract test | Add the two new names |
| `contrib/picard/musefs/_common/*` | Vendored copy of `musefs_common` | Regenerate via `vendor_to_picard.py` |
| `contrib/lidarr/src/musefs_lidarr/events.py` | Parse Lidarr custom-script events | Add `ARTIST_DELETED`/`ALBUM_DELETED` + MBID fields |
| `contrib/lidarr/src/musefs_lidarr/sync.py` | musefs-side sync/prune operations | Add `prune_deleted` |
| `contrib/lidarr/src/musefs_lidarr/cli_sync.py` | Event dispatch / CLI entrypoint | Dispatch delete events to `prune_deleted` |
| `contrib/lidarr/tests/test_events.py` | Event-parsing tests | Add delete-event parse tests |
| `contrib/lidarr/tests/test_sync.py` | Sync/prune tests | Add `prune_deleted` tests |
| `contrib/lidarr/tests/test_cli.py` | CLI dispatch tests | Add delete-dispatch tests |
| `contrib/beets/beetsplug/musefs.py` | beets plugin | Add removal listeners + reconcile guard |
| `contrib/beets/tests/test_reconcile.py` | beets reconcile tests | Add removal-triggers-prune tests |
| `contrib/lidarr/README.md`, `contrib/beets/README.md`, `ARCHITECTURE.md` | Docs | Document deletion pruning |

---

## Task 1: Store helper `track_ids_by_tag`

**Files:**
- Modify: `contrib/python-musefs/src/musefs_common/store.py` (add a function after `track_ids_for_paths`, which ends at line 134)
- Test: `contrib/python-musefs/tests/test_store_db.py`

- [ ] **Step 1: Write the failing test**

Add to `contrib/python-musefs/tests/test_store_db.py`:

```python
def test_track_ids_by_tag_matches_text_rows(db_path):
    from musefs_common import track_ids_by_tag
    from musefs_common.store import replace_tags

    conn = connect(db_path)
    try:
        a = insert_track(conn, "/m/a.flac")
        b = insert_track(conn, "/m/b.flac")
        c = insert_track(conn, "/m/c.flac")
        replace_tags(conn, a, [("musicbrainz_albumid", "rg-1")])
        replace_tags(conn, b, [("musicbrainz_albumid", "rg-1")])
        replace_tags(conn, c, [("musicbrainz_albumid", "rg-2")])
        conn.commit()
        assert set(track_ids_by_tag(conn, "musicbrainz_albumid", "rg-1")) == {a, b}
        assert track_ids_by_tag(conn, "musicbrainz_albumid", "rg-2") == [c]
        assert track_ids_by_tag(conn, "musicbrainz_albumid", "nope") == []
    finally:
        conn.close()


def test_track_ids_by_tag_ignores_binary_tags(db_path):
    from musefs_common import track_ids_by_tag

    conn = connect(db_path)
    try:
        tid = insert_track(conn, "/m/a.flac")
        # A scanner-written binary tag (value_blob NOT NULL) must never match.
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal, value_blob) "
            "VALUES (?, 'cover', '', 0, ?)",
            (tid, b"\x00\x01"),
        )
        conn.commit()
        assert track_ids_by_tag(conn, "cover", "") == []
    finally:
        conn.close()
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python -m pytest tests/test_store_db.py::test_track_ids_by_tag_matches_text_rows -v`
Expected: FAIL with `ImportError: cannot import name 'track_ids_by_tag'`

- [ ] **Step 3: Write minimal implementation**

In `contrib/python-musefs/src/musefs_common/store.py`, immediately after the `track_ids_for_paths` function (after line 134):

```python
def track_ids_by_tag(conn, key, value):
    """Return a list of track ids whose plugin-owned text tag ``(key, value)``
    matches (order unspecified, possibly empty).

    Scoped to text rows (``value_blob IS NULL``); scanner-written binary tags
    never match. The intent-based counterpart to ``prune_missing``'s
    existence-based scoping: used to map a source's "I deleted this album/artist"
    signal back to the rows it tagged.
    """
    rows = conn.execute(
        "SELECT track_id FROM tags WHERE key = ? AND value = ? AND value_blob IS NULL",
        (key, value),
    )
    return [track_id for (track_id,) in rows]
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `python -m pytest tests/test_store_db.py -k track_ids_by_tag -v`
Expected: PASS (both new tests)

- [ ] **Step 5: Commit**

```bash
git add contrib/python-musefs/src/musefs_common/store.py contrib/python-musefs/tests/test_store_db.py
git commit -m "feat(python-musefs): add track_ids_by_tag store helper"
```

---

## Task 2: Store helper `delete_tracks`

**Files:**
- Modify: `contrib/python-musefs/src/musefs_common/store.py` (add a function after `track_ids_by_tag`)
- Test: `contrib/python-musefs/tests/test_store_db.py`

- [ ] **Step 1: Write the failing test**

Add to `contrib/python-musefs/tests/test_store_db.py`:

```python
def test_delete_tracks_removes_rows_and_cascades(db_path):
    from musefs_common import delete_tracks, tags_for_track, track_id_for_path
    from musefs_common.store import replace_tags

    conn = connect(db_path)
    try:
        a = insert_track(conn, "/m/a.flac")
        b = insert_track(conn, "/m/b.flac")
        replace_tags(conn, a, [("artist", "Alice"), ("genre", "Rock")])
        # An art row referencing the track, to prove the cascade reaches track_art.
        conn.execute("INSERT INTO art (sha256, mime, bytes) VALUES ('h', 'image/jpeg', ?)", (b"x",))
        conn.execute(
            "INSERT INTO track_art (track_id, art_sha256, ordinal) VALUES (?, 'h', 0)", (a,)
        )
        conn.commit()

        deleted = delete_tracks(conn, [a])
        conn.commit()

        assert deleted == 1
        assert track_id_for_path(conn, "/m/a.flac") is None
        assert tags_for_track(conn, a) == []  # tags cascaded
        assert conn.execute(
            "SELECT COUNT(*) FROM track_art WHERE track_id=?", (a,)
        ).fetchone()[0] == 0
        assert track_id_for_path(conn, "/m/b.flac") == b  # untouched
    finally:
        conn.close()


def test_delete_tracks_counts_only_rows_actually_deleted(db_path):
    from musefs_common import delete_tracks

    conn = connect(db_path)
    try:
        a = insert_track(conn, "/m/a.flac")
        conn.commit()
        # 999 is not a real id, so it contributes 0 to the count.
        assert delete_tracks(conn, [a, 999]) == 1
        conn.commit()
    finally:
        conn.close()


def test_delete_tracks_empty_input(db_path):
    from musefs_common import delete_tracks

    conn = connect(db_path)
    try:
        assert delete_tracks(conn, []) == 0
    finally:
        conn.close()
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python -m pytest tests/test_store_db.py::test_delete_tracks_removes_rows_and_cascades -v`
Expected: FAIL with `ImportError: cannot import name 'delete_tracks'`

- [ ] **Step 3: Write minimal implementation**

In `contrib/python-musefs/src/musefs_common/store.py`, immediately after `track_ids_by_tag`:

```python
def delete_tracks(conn, track_ids):
    """Unconditionally delete the given track rows; return the count actually
    deleted (an already-gone id contributes 0).

    The intent-based delete: unlike ``prune_missing`` it does not check on-disk
    existence. ``tags`` and ``track_art`` rows cascade away via the schema's
    ``ON DELETE CASCADE`` (``connect`` enables ``foreign_keys = ON``).
    """
    deleted = 0
    for track_id in track_ids:
        deleted += conn.execute("DELETE FROM tracks WHERE id = ?", (track_id,)).rowcount
    return deleted
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `python -m pytest tests/test_store_db.py -k delete_tracks -v`
Expected: PASS (all three new tests)

- [ ] **Step 5: Commit**

```bash
git add contrib/python-musefs/src/musefs_common/store.py contrib/python-musefs/tests/test_store_db.py
git commit -m "feat(python-musefs): add delete_tracks store helper"
```

---

## Task 3: Export the new helpers and re-vendor to Picard

**Files:**
- Modify: `contrib/python-musefs/src/musefs_common/__init__.py:13-26` (import block) and `:31-57` (`__all__`)
- Test: `contrib/python-musefs/tests/test_public_api.py`
- Regenerate: `contrib/picard/musefs/_common/store.py` and `contrib/picard/musefs/_common/__init__.py`

- [ ] **Step 1: Write the failing test**

In `contrib/python-musefs/tests/test_public_api.py`, find the list/set of expected public names (it currently includes `"prune_missing"`) and add the two new names. The existing test asserts the package exports a known set; add:

```python
        "track_ids_by_tag",
        "delete_tracks",
```

next to `"prune_missing"` in that expected collection.

- [ ] **Step 2: Run test to verify it fails**

Run: `python -m pytest tests/test_public_api.py -v`
Expected: FAIL — the exported set is missing `track_ids_by_tag`/`delete_tracks`.

- [ ] **Step 3: Add the exports**

In `contrib/python-musefs/src/musefs_common/__init__.py`, add both names to the `from .store import (...)` block (alphabetically near the others):

```python
from .store import (
    TagRow,
    check_schema_version,
    connect,
    delete_tracks,
    merge_tags,
    prune_missing,
    replace_tags,
    replace_track_art,
    sniff_mime,
    tags_for_track,
    track_id_for_path,
    track_ids_by_tag,
    track_ids_for_paths,
    upsert_art,
)
```

And add both to `__all__` (after `"prune_missing"`):

```python
    "prune_missing",
    "track_ids_by_tag",
    "delete_tracks",
```

- [ ] **Step 4: Run test to verify it passes**

Run: `python -m pytest tests/test_public_api.py -v`
Expected: PASS

- [ ] **Step 5: Re-vendor into Picard**

The Picard plugin vendors `musefs_common` byte-for-byte; a drift test (`contrib/picard/tests/test_vendor_sync.py`) fails until the vendored copy is regenerated. Picard does not call the new functions, but the byte-identical gate still requires the refresh.

Run from the repo root:

```bash
python contrib/python-musefs/vendor_to_picard.py
```

- [ ] **Step 6: Verify the Picard drift gate passes**

Run (Picard tests use the system package per `CONTRIBUTING.md`; the vendor-sync test is pure-Python and needs no Picard import):

```bash
python -m pytest contrib/picard/tests/test_vendor_sync.py -v
```

Expected: PASS (`test_vendored_file_set_matches_canonical`, `test_vendored_bodies_are_byte_identical`).

- [ ] **Step 7: Commit**

```bash
git add contrib/python-musefs/src/musefs_common/__init__.py contrib/python-musefs/tests/test_public_api.py contrib/picard/musefs/_common/store.py contrib/picard/musefs/_common/__init__.py
git commit -m "feat(python-musefs): export track_ids_by_tag/delete_tracks; re-vendor Picard"
```

---

## Task 4: Lidarr — parse `ArtistDeleted`/`AlbumDeleted` events

**Files:**
- Modify: `contrib/lidarr/src/musefs_lidarr/events.py`
- Test: `contrib/lidarr/tests/test_events.py`

- [ ] **Step 1: Write the failing tests**

Add to `contrib/lidarr/tests/test_events.py`:

```python
def test_parse_album_deleted_event():
    event = parse_event(
        {
            "Lidarr_EventType": "AlbumDeleted",
            "Lidarr_Artist_Id": "12",
            "Lidarr_Album_Id": "34",
            "Lidarr_Album_MBId": "rg-mbid",
            "Lidarr_Artist_MBId": "artist-mbid",
            "Lidarr_Artist_DeletedFiles": "True",
        }
    )

    assert event.event_type == EventType.ALBUM_DELETED
    assert event.raw_type == "AlbumDeleted"
    assert event.album_mbid == "rg-mbid"
    assert event.artist_mbid == "artist-mbid"


def test_parse_artist_deleted_event():
    event = parse_event(
        {
            "Lidarr_EventType": "ArtistDeleted",
            "Lidarr_Artist_Id": "12",
            "Lidarr_Artist_MBId": "artist-mbid",
            "Lidarr_Artist_DeletedFiles": "False",
        }
    )

    assert event.event_type == EventType.ARTIST_DELETED
    assert event.raw_type == "ArtistDeleted"
    assert event.artist_mbid == "artist-mbid"
    assert event.album_mbid is None


def test_parse_delete_event_with_missing_mbid():
    event = parse_event({"Lidarr_EventType": "AlbumDeleted", "Lidarr_Artist_Id": "12"})

    assert event.event_type == EventType.ALBUM_DELETED
    assert event.album_mbid is None
    assert event.artist_mbid is None


def test_parse_album_deleted_event_with_lowercase_keys():
    event = parse_event(
        {
            "lidarr_eventtype": "AlbumDeleted",
            "lidarr_album_mbid": "rg-mbid",
            "lidarr_artist_mbid": "artist-mbid",
        }
    )

    assert event.event_type == EventType.ALBUM_DELETED
    assert event.album_mbid == "rg-mbid"
    assert event.artist_mbid == "artist-mbid"
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `python -m pytest tests/test_events.py::test_parse_album_deleted_event -v`
Expected: FAIL with `AttributeError: ALBUM_DELETED` (no such `EventType` member).

- [ ] **Step 3: Implement the parsing**

In `contrib/lidarr/src/musefs_lidarr/events.py`:

Add two members to `EventType` (after `TRACK_RETAG`):

```python
class EventType(Enum):
    TEST = "Test"
    ALBUM_DOWNLOAD = "AlbumDownload"
    RENAME = "Rename"
    TRACK_RETAG = "TrackRetag"
    ARTIST_DELETED = "ArtistDeleted"
    ALBUM_DELETED = "AlbumDeleted"
    UNSUPPORTED = "Unsupported"
```

Add two fields to `LidarrEvent` (after `album_id`):

```python
@dataclass(frozen=True)
class LidarrEvent:
    event_type: EventType
    raw_type: str
    paths: list[str] = field(default_factory=list)
    previous_paths: list[str] = field(default_factory=list)
    artist_id: int | None = None
    album_id: int | None = None
    album_mbid: str | None = None
    artist_mbid: str | None = None
```

In `parse_event`, extend the event-type dispatch and extract the MBIDs. Replace the `else` branch of the type ladder and add MBID extraction before the `return`:

```python
    elif raw == EventType.TRACK_RETAG.value:
        event_type = EventType.TRACK_RETAG
    elif raw == EventType.ARTIST_DELETED.value:
        event_type = EventType.ARTIST_DELETED
    elif raw == EventType.ALBUM_DELETED.value:
        event_type = EventType.ALBUM_DELETED
    else:
        event_type = EventType.UNSUPPORTED
```

Add a helper for stripped-or-None text near `_int_or_none`:

```python
def _text_or_none(value: str | None) -> str | None:
    if value is None:
        return None
    value = value.strip()
    return value or None
```

Then in `parse_event`, populate the new fields in the returned `LidarrEvent`:

```python
    return LidarrEvent(
        event_type=event_type,
        raw_type=raw,
        paths=paths,
        previous_paths=previous_paths,
        artist_id=_int_or_none(lidarr_get(env, "Lidarr_Artist_Id")),
        album_id=_int_or_none(lidarr_get(env, "Lidarr_Album_Id")),
        album_mbid=_text_or_none(lidarr_get(env, "Lidarr_Album_MBId")),
        artist_mbid=_text_or_none(lidarr_get(env, "Lidarr_Artist_MBId")),
    )
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `python -m pytest tests/test_events.py -v`
Expected: PASS (new tests plus all existing event tests still green).

- [ ] **Step 5: Commit**

```bash
git add contrib/lidarr/src/musefs_lidarr/events.py contrib/lidarr/tests/test_events.py
git commit -m "feat(lidarr): parse ArtistDeleted/AlbumDeleted events with MBIDs"
```

---

## Task 5: Lidarr — `prune_deleted` in sync.py

**Files:**
- Modify: `contrib/lidarr/src/musefs_lidarr/sync.py` (imports near the top; add `prune_deleted` after `sync_rename_prune`)
- Test: `contrib/lidarr/tests/test_sync.py`

- [ ] **Step 1: Write the failing tests**

Add to `contrib/lidarr/tests/test_sync.py` (it already imports from `musefs_lidarr.sync`; the `db_path`, `make_track` fixtures come from the test conftests — `make_track` is in `python-musefs`'s conftest, mirrored for lidarr; if `make_track` is unavailable in the lidarr suite, insert rows via `musefs_common.connect` + the conftest `insert_track` exactly as below):

```python
def test_prune_deleted_album_removes_matching_rows(db_path, tmp_path):
    from musefs_common import connect
    from musefs_common.store import replace_tags
    from musefs_lidarr.events import EventType, LidarrEvent
    from musefs_lidarr.import_link import LinkMode
    from musefs_lidarr.sync import SyncConfig, prune_deleted

    backing = tmp_path / "a.flac"
    backing.write_bytes(b"audio")  # backing file stays on disk

    conn = connect(db_path)
    try:
        a = conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, "
            "backing_size, backing_mtime_ns, updated_at) VALUES (?, 'flac', 0, 0, 0, 0, 0)",
            (str(backing),),
        ).lastrowid
        b = conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, "
            "backing_size, backing_mtime_ns, updated_at) VALUES ('/m/b.flac', 'flac', 0, 0, 0, 0, 0)",
        ).lastrowid
        replace_tags(conn, a, [("musicbrainz_albumid", "rg-1")])
        replace_tags(conn, b, [("musicbrainz_albumid", "rg-2")])
        conn.commit()
    finally:
        conn.close()

    config = SyncConfig(db_path=db_path, link_mode=LinkMode.SYMLINK)
    event = LidarrEvent(
        event_type=EventType.ALBUM_DELETED, raw_type="AlbumDeleted", album_mbid="rg-1"
    )
    pruned = prune_deleted(config=config, event=event)

    assert pruned == 1
    assert backing.exists()  # invariant: backing bytes untouched
    conn = connect(db_path)
    try:
        ids = {row[0] for row in conn.execute("SELECT id FROM tracks")}
        assert ids == {b}
    finally:
        conn.close()


def test_prune_deleted_artist_removes_all_artist_rows(db_path):
    from musefs_common import connect
    from musefs_common.store import replace_tags
    from musefs_lidarr.events import EventType, LidarrEvent
    from musefs_lidarr.import_link import LinkMode
    from musefs_lidarr.sync import SyncConfig, prune_deleted

    conn = connect(db_path)
    try:
        ids = []
        for i, art in enumerate(["art-1", "art-1", "art-2"]):
            tid = conn.execute(
                "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, "
                "backing_size, backing_mtime_ns, updated_at) VALUES (?, 'flac', 0, 0, 0, 0, 0)",
                (f"/m/{i}.flac",),
            ).lastrowid
            replace_tags(conn, tid, [("musicbrainz_artistid", art)])
            ids.append(tid)
        conn.commit()
    finally:
        conn.close()

    config = SyncConfig(db_path=db_path, link_mode=LinkMode.SYMLINK)
    event = LidarrEvent(
        event_type=EventType.ARTIST_DELETED, raw_type="ArtistDeleted", artist_mbid="art-1"
    )
    assert prune_deleted(config=config, event=event) == 2
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `python -m pytest tests/test_sync.py::test_prune_deleted_album_removes_matching_rows -v`
Expected: FAIL with `ImportError: cannot import name 'prune_deleted'`

- [ ] **Step 3: Implement `prune_deleted`**

In `contrib/lidarr/src/musefs_lidarr/sync.py`:

Extend the `musefs_common` import block to add `delete_tracks` and `track_ids_by_tag`:

```python
from musefs_common import (
    SCAN_TIMEOUT_SECONDS,
    ArtImage,
    SyncStats,
    check_schema_version,
    connect,
    delete_tracks,
    prune_missing,
    realpath_key,
    run_scan,
    sniff_mime,
    sync_files,
    track_id_for_path,
    track_ids_by_tag,
    track_ids_for_paths,
)
```

Extend the events import to include `EventType`:

```python
from .events import EventType, LidarrEvent
```

Add the function immediately after `sync_rename_prune`:

```python
def prune_deleted(*, config: SyncConfig, event: LidarrEvent) -> int:
    """Delete store rows for a Lidarr album/artist deletion, mapped by MusicBrainz id.

    Lidarr never touches the backing files (it only unlinks its own symlink
    tree), so this is intent-based, not existence-based: rows are removed by
    matching the stored ``musicbrainz_albumid`` / ``musicbrainz_artistid`` tag
    against the id Lidarr reports in the delete event. Returns the count deleted.

    The caller guarantees the relevant MBID is present (see ``cli_sync``); an
    album event matches ``musicbrainz_albumid``, an artist event
    ``musicbrainz_artistid``.
    """
    if event.event_type is EventType.ALBUM_DELETED:
        key, value = "musicbrainz_albumid", event.album_mbid
    else:
        key, value = "musicbrainz_artistid", event.artist_mbid

    conn = connect(config.db_path)
    try:
        deleted = delete_tracks(conn, track_ids_by_tag(conn, key, value))
        conn.commit()
        return deleted
    except Exception:
        conn.rollback()
        raise
    finally:
        conn.close()
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `python -m pytest tests/test_sync.py -k prune_deleted -v`
Expected: PASS (both new tests)

- [ ] **Step 5: Commit**

```bash
git add contrib/lidarr/src/musefs_lidarr/sync.py contrib/lidarr/tests/test_sync.py
git commit -m "feat(lidarr): add prune_deleted to remove rows by MusicBrainz id"
```

---

## Task 6: Lidarr — dispatch delete events in cli_sync.py

**Files:**
- Modify: `contrib/lidarr/src/musefs_lidarr/cli_sync.py` (import block lines 10-15; insert dispatch after the `TRACK_RETAG` block at lines 97-102, before `config = LidarrConfig.from_env(env)` at line 104)
- Test: `contrib/lidarr/tests/test_cli.py`

- [ ] **Step 1: Write the failing tests**

Add to `contrib/lidarr/tests/test_cli.py` (it already imports `run` and constructs envs; the `run` signature is `run(argv=None, environ=None, *, client_factory=LidarrClient, sync_runner=...)`):

```python
def test_sync_cli_album_deleted_prunes_without_api(tmp_path, capsys):
    import sqlite3

    from musefs_common import connect
    from musefs_common.schema import SCHEMA_SQL
    from musefs_common.store import replace_tags
    from musefs_lidarr.cli_sync import run

    db = tmp_path / "musefs.db"
    raw = sqlite3.connect(str(db))
    raw.executescript(SCHEMA_SQL)
    raw.commit()
    raw.close()

    conn = connect(str(db))
    try:
        tid = conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, "
            "backing_size, backing_mtime_ns, updated_at) VALUES ('/m/a.flac', 'flac', 0, 0, 0, 0, 0)"
        ).lastrowid
        replace_tags(conn, tid, [("musicbrainz_albumid", "rg-1")])
        conn.commit()
    finally:
        conn.close()

    def boom(_config):
        raise AssertionError("delete events must not construct a Lidarr client")

    rc = run(
        [],
        {
            "Lidarr_EventType": "AlbumDeleted",
            "Lidarr_Album_MBId": "rg-1",
            "MUSEFS_DB": str(db),
        },
        client_factory=boom,
    )

    assert rc == 0
    assert "pruned 1 rows" in capsys.readouterr().out
    conn = connect(str(db))
    try:
        assert conn.execute("SELECT COUNT(*) FROM tracks").fetchone()[0] == 0
    finally:
        conn.close()


def test_sync_cli_delete_without_mbid_is_skipped(tmp_path, capsys):
    from musefs_lidarr.cli_sync import run

    def boom(_config):
        raise AssertionError("must not construct a client")

    rc = run(
        [],
        {"Lidarr_EventType": "AlbumDeleted", "MUSEFS_DB": str(tmp_path / "musefs.db")},
        client_factory=boom,
    )

    assert rc == 0
    captured = capsys.readouterr()
    assert "no MusicBrainz id" in captured.err
    # No DB was created/opened — the skip happens before any connection.
    assert not (tmp_path / "musefs.db").exists()
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `python -m pytest tests/test_cli.py::test_sync_cli_album_deleted_prunes_without_api -v`
Expected: FAIL — `AlbumDeleted` currently parses to a recognized type but no dispatch exists, so it falls through to the API block and constructs a client (hitting `boom`) or errors.

- [ ] **Step 3: Implement the dispatch**

In `contrib/lidarr/src/musefs_lidarr/cli_sync.py`:

Extend the `.sync` import to add `prune_deleted`:

```python
from .sync import (
    collect_all_payloads,
    collect_event_payloads,
    config_from_env,
    prune_deleted,
    sync_event_with_payloads,
)
```

Insert the delete-event handling after the `TRACK_RETAG` early-return block (after line 102) and before `config = LidarrConfig.from_env(env)`:

```python
        if event.event_type in (EventType.ALBUM_DELETED, EventType.ARTIST_DELETED):
            mbid = (
                event.album_mbid
                if event.event_type is EventType.ALBUM_DELETED
                else event.artist_mbid
            )
            if not mbid:
                print(
                    "musefs-lidarr-sync: delete event carried no MusicBrainz id; "
                    "cannot prune, leaving rows for the next scan/reconcile",
                    file=sys.stderr,
                )
                return 0
            sync_config = config_from_env(env)
            pruned = prune_deleted(config=sync_config, event=event)
            print(f"musefs-lidarr-sync: pruned {pruned} rows")
            return 0
```

Note: `config_from_env(env)` is called here, inside the existing `try`, so a missing `MUSEFS_DB` raises `ConfigError` and maps to the exit-1 path; `LidarrConfig`/`client_factory` are never touched for delete events.

- [ ] **Step 4: Run tests to verify they pass**

Run: `python -m pytest tests/test_cli.py -v`
Expected: PASS (both new tests plus all existing CLI tests).

- [ ] **Step 5: Commit**

```bash
git add contrib/lidarr/src/musefs_lidarr/cli_sync.py contrib/lidarr/tests/test_cli.py
git commit -m "feat(lidarr): dispatch AlbumDeleted/ArtistDeleted to prune_deleted"
```

---

## Task 7: beets — prune on `item_removed`/`album_removed`

**Files:**
- Modify: `contrib/beets/beetsplug/musefs.py` — `__init__` (lines 26-44), add an `_on_removed` handler, and `_reconcile_pending` (lines 116-149)
- Test: `contrib/beets/tests/test_reconcile.py`

- [ ] **Step 1: Write the failing tests**

`contrib/beets/tests/test_reconcile.py` builds the plugin via `MusefsPlugin.__new__(MusefsPlugin)` (bypassing beets' `__init__`) and stubs collaborators — follow that pattern, but leave the **real** `_prune_missing` in place and point `_db_path` at a real test DB so the existence-based prune actually runs. The beets conftest provides the `db_path` fixture and a module-level `insert_track` helper.

Add a removal-test helper near the top of `test_reconcile.py` (after the existing imports):

```python
from musefs_common import connect as musefs_connect  # noqa: E402
from conftest import insert_track  # noqa: E402,F401


def _removal_plugin(db_path):
    """A MusefsPlugin (init bypassed) wired to a real DB, real _prune_missing,
    and no pending writes — the removals-only reconcile path."""
    plugin = MusefsPlugin.__new__(MusefsPlugin)
    plugin._log = FakeLog()
    plugin._pending = []
    plugin._saw_removal = False
    plugin._db_path = lambda: db_path
    plugin._autoscan = lambda: False
    plugin._restore_backing = lambda: False
    return plugin
```

Then add the two tests:

```python
def test_removal_triggers_reconcile_and_prunes_deleted_file(db_path, tmp_path):
    # A removed-and-deleted backing file is pruned even with no writes pending.
    gone = tmp_path / "gone.flac"  # never created on disk == already deleted
    conn = musefs_connect(db_path)
    try:
        tid = insert_track(conn, str(gone))
        conn.commit()
    finally:
        conn.close()

    plugin = _removal_plugin(db_path)
    plugin._on_removed(item=SimpleNamespace(path=str(gone).encode()))
    plugin._reconcile_pending()  # no writes pending; runs because a removal fired

    conn = musefs_connect(db_path)
    try:
        assert conn.execute("SELECT COUNT(*) FROM tracks WHERE id=?", (tid,)).fetchone()[0] == 0
    finally:
        conn.close()


def test_removal_keeps_row_when_file_still_present(db_path, tmp_path):
    # `beet remove` without -d leaves the file on disk -> existence-based prune keeps it.
    present = tmp_path / "present.flac"
    present.write_bytes(b"audio")
    conn = musefs_connect(db_path)
    try:
        tid = insert_track(conn, str(present))
        conn.commit()
    finally:
        conn.close()

    plugin = _removal_plugin(db_path)
    plugin._on_removed(item=SimpleNamespace(path=str(present).encode()))
    plugin._reconcile_pending()

    conn = musefs_connect(db_path)
    try:
        assert conn.execute("SELECT COUNT(*) FROM tracks WHERE id=?", (tid,)).fetchone()[0] == 1
    finally:
        conn.close()
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `contrib/beets/.venv/bin/python -m pytest contrib/beets/tests/test_reconcile.py -k removal -v` (from repo root, or `python -m pytest tests/test_reconcile.py -k removal -v` from `contrib/beets` with the venv active)
Expected: FAIL with `AttributeError: 'MusefsPlugin' object has no attribute '_on_removed'`

- [ ] **Step 3: Implement listeners and the reconcile guard**

In `contrib/beets/beetsplug/musefs.py`:

In `__init__`, initialize the flag and register the two listeners (add after the existing `self._pending = []` / listener registrations, lines 41-44):

```python
        self._pending = []
        self._saw_removal = False
        self.register_listener("after_write", self._record)
        self.register_listener("item_imported", self._record)
        self.register_listener("album_imported", self._record_album)
        self.register_listener("item_removed", self._on_removed)
        self.register_listener("album_removed", self._on_removed)
        self.register_listener("cli_exit", self._reconcile_pending)
```

Add the handler (after `_record_album`, around line 114):

```python
    def _on_removed(self, **kwargs):
        # item_removed/album_removed only flip the reconcile guard so a
        # removals-only command still runs the end-of-command prune. We do not
        # scan or sync removed items; the unscoped prune_missing handles them.
        self._saw_removal = True
```

Replace the body of `_reconcile_pending` (lines 116-149) so it runs on a removal even with no writes, and skips scan/sync when there are no written items:

```python
    def _reconcile_pending(self, lib=None, **kwargs):
        """End-of-command reconcile: sync every touched item at its final path,
        then prune rows whose backing file is gone (moved away or deleted at the
        source). Best-effort — a passive hook must never abort the beets
        operation, so errors become warnings."""
        pending, self._pending = self._pending, []
        saw_removal, self._saw_removal = self._saw_removal, False
        # Dedup by final on-disk path (an item may fire several events).
        items = list({os.fsdecode(i.path): i for i in pending if i is not None}.values())
        if not items and not saw_removal:
            return
        db_path = self._db_path()
        if not db_path:
            self._log.warning("musefs: no `musefs.db` configured; skipping sync")
            return
        try:
            if items:
                if self._autoscan():
                    self._run_scan(db_path, [os.fsdecode(i.path) for i in items])
                self._sync(db_path, items, restore_backing=self._restore_backing())
            self._prune_missing(db_path)
        except (ui.UserError, sqlite3.Error, OSError, subprocess.SubprocessError) as exc:
            # A passive cli_exit hook must never abort the beets operation for an
            # environmental failure (locked DB, vanished file, wedged scan); those
            # degrade to a warning. An unexpected exception still propagates so a
            # real bug surfaces instead of hiding behind a one-line warning.
            if self._is_permission_error(exc):
                # A persistent setup failure (read-only DB / permission denied)
                # would otherwise be a silent no-op: beets hides plugin WARNINGs
                # at default verbosity, so the user gets no sign the sync did
                # nothing. Surface it via ui.print_ — but still don't abort.
                ui.print_(
                    f"musefs: cannot write {db_path} (read-only/permission denied) "
                    f"— metadata not synced"
                )
            else:
                self._log.warning("musefs: {}", exc)
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `contrib/beets/.venv/bin/python -m pytest contrib/beets/tests/test_reconcile.py -v`
Expected: PASS (new removal tests plus existing reconcile tests still green).

- [ ] **Step 5: Run the full beets suite to catch regressions**

Run: `contrib/beets/.venv/bin/python -m pytest contrib/beets/tests/ -q`
Expected: PASS (the existing `test_plugin.py`/`test_e2e.py` exercise the reconcile path; confirm the guard change didn't break the writes-only flow).

- [ ] **Step 6: Commit**

```bash
git add contrib/beets/beetsplug/musefs.py contrib/beets/tests/test_reconcile.py
git commit -m "feat(beets): prune store rows on item_removed/album_removed"
```

---

## Task 8: Documentation

**Files:**
- Modify: `contrib/lidarr/README.md` (Custom Script section, ~lines 73-83; Notes section, ~lines 188-202)
- Modify: `contrib/beets/README.md`
- Modify: `ARCHITECTURE.md` (external-writer-contract section)

- [ ] **Step 1: Update the Lidarr README**

In `contrib/lidarr/README.md`, in the "Lidarr Custom Script" list (currently "On Release Import" / "On Rename"), add:

```markdown
- On Album Delete: enabled.
- On Artist Delete: enabled.
```

And add a bullet to the "Notes" section:

```markdown
- **Deletions prune by MusicBrainz id.** On an Album/Artist delete, the sync
  removes the matching store rows (`musicbrainz_albumid` / `musicbrainz_artistid`)
  so the mount stops presenting them. The backing audio is never touched —
  pruning only drops the store rows, not the files Lidarr keeps in the backing
  directory. A delete event for a release with no MusicBrainz id cannot be mapped
  and is logged and skipped; those rows clear on the next scan/reconcile.
```

- [ ] **Step 2: Update the beets README**

In `contrib/beets/README.md`, add a note (near the description of the autoscan/sync behavior) documenting removal pruning:

```markdown
- **Removals prune the store.** `beet remove -d` deletes the backing file, so the
  store row is pruned at the end of the command. A bare `beet remove` (which keeps
  the file on disk) leaves the row in place — musefs can still serve those bytes.
```

- [ ] **Step 3: Update ARCHITECTURE.md**

In `ARCHITECTURE.md`, in the external-writer-contract section, add a sentence distinguishing the two pruning models:

```markdown
External writers prune in one of two ways depending on how they own files.
In-place writers (e.g. the beets plugin) prune by file existence — a removed
backing file drops its row via `prune_missing`. Link-tree writers (e.g. the
Lidarr integration) never delete the backing files they point at, so they prune
by identity instead: a source-reported album/artist deletion removes the rows
carrying the matching MusicBrainz id.
```

- [ ] **Step 4: Verify docs render and lint clean**

Run: `git diff --stat` to confirm only the three doc files changed. (Docs-only edits skip the cargo gate; the pre-commit hook still runs `ruff`/`yamllint`/`shellcheck` over any tracked code/config, none of which these touch.)

- [ ] **Step 5: Commit**

```bash
git add contrib/lidarr/README.md contrib/beets/README.md ARCHITECTURE.md
git commit -m "docs: document deletion pruning for beets and Lidarr (#422)"
```

---

## Final verification

- [ ] **Run all three contrib suites green:**

```bash
( cd contrib/python-musefs && python -m pytest -q )
( cd contrib/lidarr && python -m pytest -q )
contrib/beets/.venv/bin/python -m pytest contrib/beets/tests/ -q
```

Expected: all PASS.

- [ ] **Confirm the Picard vendor gate is clean:**

```bash
python -m pytest contrib/picard/tests/test_vendor_sync.py -q
```

Expected: PASS.

- [ ] **Confirm no stray changes / ruff clean** before opening the PR:

```bash
git status
ruff check contrib/
```

Expected: clean tree on the `issue-422-deletion-pruning` branch; ruff reports no issues.

---

## Notes for the implementer

- **Scope discipline:** No `musefs-core`/Rust or schema changes. Everything is in `contrib/` Python.
- **The cardinal invariant still holds:** pruning only deletes *store rows*; it never touches backing audio. The Lidarr `prune_deleted` test asserts the backing file still exists after a prune — keep that assertion.
- **Lidarr `prune_deleted` is intent-based on purpose** (it does not check file existence) because Lidarr keeps the backing files. Do not "harden" it into an existence check — that would make it a no-op for Lidarr.
- **Release-group granularity (accepted limitation):** `Lidarr_Album_MBId` is the release-group id, so album-delete prunes every row carrying that `musicbrainz_albumid`. Exact within a Lidarr-managed library; over-prune is only possible in a mixed store. Do not add new identity tags to tighten this — it is explicitly out of scope.
