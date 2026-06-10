# beets Field Coverage + Stateful Merge Sync — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make a beets sync reflect the user's full library in the musefs mount — every tag beets writes to a file, under recognizable names, with beets winning over embedded values, other embedded tags preserved, and deletions that stick.

**Architecture:** A new `merge_tags` primitive in the shared `python-musefs` library does per-key replacement (M-wins) instead of full text-tag replacement, leaving unmanaged embedded tags (`B`) untouched. The beets plugin derives the managed set `M` from beets' own `_media_tag_fields`, tracks the last-synced key set in a per-item `musefs_managed` beets flexattr, and on each sync deletes keys dropped from `M` (unless `--restore-backing`). Picard keeps full-replace and gains only the naming additions. No Rust or schema changes.

**Tech Stack:** Python 3 (beets plugin + `musefs_common` shared lib), pytest, SQLite. The Rust workspace is untouched.

**Spec:** `docs/superpowers/specs/2026-06-10-beets-field-coverage-design.md`

**Working tree:** git worktree on branch `plugin-field-coverage` at `/home/cfutro/git/musefs-plugin-fields`. All paths below are relative to that root.

**Environment setup (once, before Task 1):** the contrib suites need editable installs.
```bash
cd contrib/python-musefs && python -m pytest -q   # sanity: shared lib tests pass today
cd ../beets && python -m pytest -q                # sanity: beets unit tests pass today
```
If imports fail, install per `contrib/beets/README.md` (`pip install -e contrib/python-musefs` then `pip install -e "contrib/beets[test]"`), using the beets venv (`contrib/beets/.venv`) — system Python is PEP-668 managed.

---

## File structure

**Shared library (`contrib/python-musefs/src/musefs_common/`):**
- `store.py` — add `merge_tags()` beside `replace_tags()`.
- `sync.py` — `Record` gains `delete_keys`; `sync_one`/`sync_files` gain a `merge` flag.
- `__init__.py` — export `merge_tags`.

**beets plugin (`contrib/beets/beetsplug/`):**
- `_core.py` — rewrite `map_fields` (boundary + rename + twins + formatters + drop predicate); add managed-state helpers and `delete_keys`/`musefs_managed` wiring to `build_records`.
- `musefs.py` — `restore_backing` config + `--restore-backing` option; `_sync` uses merge and persists managed state; `_reconcile_pending` threads `restore_backing`.

**Picard plugin (`contrib/picard/musefs/`):**
- `_core.py` — extend `DIRECT_FIELDS` + `_MULTI_VALUE_KEYS` (naming additions only).

**Docs:** `contrib/beets/README.md`, `contrib/python-musefs/README.md`, `ARCHITECTURE.md`.

**Tests:** `contrib/python-musefs/tests/test_merge_tags.py` (new), `contrib/beets/tests/test_map_fields.py` (rewrite), `contrib/beets/tests/test_build_records.py` (extend), `contrib/beets/tests/test_managed_state.py` (new), `contrib/beets/tests/test_e2e.py` (extend).

---

## Task 1: `merge_tags` primitive in the shared library

**Files:**
- Modify: `contrib/python-musefs/src/musefs_common/store.py` (add after `replace_tags`, ~line 70)
- Modify: `contrib/python-musefs/src/musefs_common/__init__.py` (export)
- Test: `contrib/python-musefs/tests/test_merge_tags.py` (create)

- [ ] **Step 1: Write the failing test**

Create `contrib/python-musefs/tests/test_merge_tags.py`:

```python
from conftest import insert_track

from musefs_common import connect
from musefs_common.store import merge_tags, replace_tags


def _text_tags(conn, track_id):
    """Return {key: [values in ordinal order]} for text rows only."""
    rows = conn.execute(
        "SELECT key, value FROM tags WHERE track_id=? AND value_blob IS NULL "
        "ORDER BY key, ordinal",
        (track_id,),
    ).fetchall()
    out = {}
    for key, value in rows:
        out.setdefault(key, []).append(value)
    return out


def test_merge_overwrites_managed_keeps_unmanaged(db_path):
    conn = connect(db_path)
    try:
        tid = insert_track(conn, "/m/a.flac")
        # Baseline B (as a scan would seed it).
        replace_tags(conn, tid, [("artist", "Old"), ("comment", "keep me"),
                                  ("replaygain_track_gain", "-3.00 dB")])
        # M overrides artist + replaygain, does not mention comment.
        merge_tags(conn, tid,
                   [("artist", "New"), ("replaygain_track_gain", "-7.50 dB")],
                   delete_keys=[])
        conn.commit()
        tags = _text_tags(conn, tid)
        assert tags["artist"] == ["New"]                 # M wins
        assert tags["comment"] == ["keep me"]            # unmanaged B persists
        assert tags["replaygain_track_gain"] == ["-7.50 dB"]
    finally:
        conn.close()


def test_merge_delete_keys_suppresses_backing(db_path):
    conn = connect(db_path)
    try:
        tid = insert_track(conn, "/m/b.flac")
        replace_tags(conn, tid, [("artist", "Band"), ("comment", "drop me")])
        # M keeps artist; comment was managed before and is now dropped.
        merge_tags(conn, tid, [("artist", "Band")], delete_keys=["comment"])
        conn.commit()
        tags = _text_tags(conn, tid)
        assert tags["artist"] == ["Band"]
        assert "comment" not in tags                     # suppressed
    finally:
        conn.close()


def test_merge_multivalue_ordinals_contiguous(db_path):
    conn = connect(db_path)
    try:
        tid = insert_track(conn, "/m/c.flac")
        merge_tags(conn, tid,
                   [("artist", "A"), ("artist", "B"), ("genre", "Rock")],
                   delete_keys=[])
        conn.commit()
        ords = conn.execute(
            "SELECT ordinal FROM tags WHERE track_id=? AND key='artist' "
            "ORDER BY ordinal", (tid,)).fetchall()
        assert [o[0] for o in ords] == [0, 1]            # 0..n per key
        assert _text_tags(conn, tid)["artist"] == ["A", "B"]
    finally:
        conn.close()


def test_merge_preserves_binary_tags(db_path):
    conn = connect(db_path)
    try:
        tid = insert_track(conn, "/m/d.flac")
        # A scanner-written binary tag sharing key 'comment' (value_blob NOT NULL).
        conn.execute(
            "INSERT INTO tags (track_id, key, value_blob, ordinal) VALUES (?,?,?,0)",
            (tid, "comment", b"\x00\x01"))
        merge_tags(conn, tid, [("comment", "text")], delete_keys=[])
        conn.commit()
        # Binary row survives; text row added.
        bin_rows = conn.execute(
            "SELECT COUNT(*) FROM tags WHERE track_id=? AND value_blob IS NOT NULL",
            (tid,)).fetchone()[0]
        assert bin_rows == 1
        assert _text_tags(conn, tid)["comment"] == ["text"]
    finally:
        conn.close()
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cd contrib/python-musefs && python -m pytest tests/test_merge_tags.py -v`
Expected: FAIL — `ImportError: cannot import name 'merge_tags'`.

- [ ] **Step 3: Implement `merge_tags`**

In `contrib/python-musefs/src/musefs_common/store.py`, add immediately after `replace_tags`:

```python
def merge_tags(conn, track_id, managed_pairs, delete_keys):
    """Per-key replace of the plugin-managed text tags, leaving unmanaged text
    rows (the scan-seeded baseline) intact. ``managed_pairs`` is an ordered list
    of (key, value); every key it names is cleared and rewritten with contiguous
    ordinals. ``delete_keys`` names keys to clear without rewriting (tags the
    plugin previously managed and the user has now removed). Both deletes are
    scoped to ``value_blob IS NULL`` so scanner-written binary tags survive."""
    by_key = {}
    for key, value in managed_pairs:
        by_key.setdefault(key, []).append(value)

    for key in set(by_key) | set(delete_keys or ()):
        conn.execute(
            "DELETE FROM tags WHERE track_id = ? AND key = ? AND value_blob IS NULL",
            (track_id, key),
        )

    rows = [
        (track_id, key, value, ordinal)
        for key, values in by_key.items()
        for ordinal, value in enumerate(values)
    ]
    conn.executemany(
        "INSERT INTO tags (track_id, key, value, ordinal) VALUES (?, ?, ?, ?)",
        rows,
    )
```

- [ ] **Step 4: Export it**

In `contrib/python-musefs/src/musefs_common/__init__.py`: add `merge_tags` to the `from .store import (...)` block and to `__all__` (next to `replace_tags`).

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cd contrib/python-musefs && python -m pytest tests/test_merge_tags.py -v`
Expected: PASS (4 tests).

- [ ] **Step 6: Commit**

```bash
git add contrib/python-musefs/src/musefs_common/store.py \
        contrib/python-musefs/src/musefs_common/__init__.py \
        contrib/python-musefs/tests/test_merge_tags.py
git commit -m "feat(python-musefs): add merge_tags per-key text-tag merge primitive"
```

---

## Task 2: `Record.delete_keys` and a `merge` flag on sync

**Files:**
- Modify: `contrib/python-musefs/src/musefs_common/sync.py`
- Test: `contrib/python-musefs/tests/test_sync.py` (extend; if absent, create)

- [ ] **Step 1: Write the failing test**

Append to `contrib/python-musefs/tests/test_sync.py` (create the file with this import header if it does not exist):

```python
from conftest import insert_track

from musefs_common import connect
from musefs_common.store import replace_tags
from musefs_common.sync import Record, SyncStats, sync_files


def _text_tags(conn, track_id):
    rows = conn.execute(
        "SELECT key, value FROM tags WHERE track_id=? AND value_blob IS NULL "
        "ORDER BY key, ordinal", (track_id,)).fetchall()
    out = {}
    for key, value in rows:
        out.setdefault(key, []).append(value)
    return out


def test_sync_files_merge_keeps_unmanaged_and_deletes(db_path):
    conn = connect(db_path)
    try:
        tid = insert_track(conn, "/m/a.flac")
        replace_tags(conn, tid, [("artist", "Old"), ("comment", "keep"),
                                  ("grouping", "gone")])
        rec = Record(key="/m/a.flac",
                     pairs=[("artist", "New")],
                     delete_keys=["grouping"])
        sync_files(conn, [rec], merge=True, stats=SyncStats())
        conn.commit()
        tags = _text_tags(conn, tid)
        assert tags["artist"] == ["New"]      # merged
        assert tags["comment"] == ["keep"]    # untouched
        assert "grouping" not in tags         # deleted
    finally:
        conn.close()


def test_sync_files_default_is_full_replace(db_path):
    """merge defaults off -> Picard's behavior is unchanged (full replace)."""
    conn = connect(db_path)
    try:
        tid = insert_track(conn, "/m/b.flac")
        replace_tags(conn, tid, [("artist", "Old"), ("comment", "wiped")])
        rec = Record(key="/m/b.flac", pairs=[("artist", "New")])
        sync_files(conn, [rec], stats=SyncStats())   # no merge=
        conn.commit()
        tags = _text_tags(conn, tid)
        assert tags["artist"] == ["New"]
        assert "comment" not in tags                 # full replace wiped it
    finally:
        conn.close()
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cd contrib/python-musefs && python -m pytest tests/test_sync.py -v -k merge`
Expected: FAIL — `TypeError: __init__() got an unexpected keyword argument 'delete_keys'` (and `sync_files` has no `merge`).

- [ ] **Step 3: Add `delete_keys` to `Record`**

In `contrib/python-musefs/src/musefs_common/sync.py`, in the `Record` dataclass, add a field after `art`:

```python
    delete_keys: object = None  # list[str] of keys to clear without rewrite (merge mode)
```

- [ ] **Step 4: Thread `merge` through `sync_one`/`sync_files`**

In the same file, update the import and the two functions. Change the import line to:

```python
from .store import merge_tags, replace_tags, replace_track_art, track_id_for_path, upsert_art
```

Replace the tag-writing line in `sync_one` and add the `merge` parameter. The `sync_one` signature becomes `def sync_one(conn, record, stats, *, dry_run=False, merge=False):` and the write block becomes:

```python
    if not dry_run:
        if merge:
            merge_tags(conn, track_id, record.pairs, record.delete_keys or [])
        else:
            replace_tags(conn, track_id, record.pairs)
        if will_link_art:
            arts = [
                (upsert_art(conn, img.data, img.mime), img.picture_type, img.description)
                for img in kept
            ]
            replace_track_art(conn, track_id, arts)
```

`sync_files` signature becomes `def sync_files(conn, records, *, dry_run=False, stats=None, merge=False):` and its loop passes the flag:

```python
    for record in records:
        sync_one(conn, record, stats, dry_run=dry_run, merge=merge)
    return stats
```

- [ ] **Step 5: Run the full shared-lib suite**

Run: `cd contrib/python-musefs && python -m pytest -q`
Expected: PASS (new sync tests pass; all prior tests still pass — `merge` defaults off, so existing Picard/beets behavior is untouched).

- [ ] **Step 6: Commit**

```bash
git add contrib/python-musefs/src/musefs_common/sync.py \
        contrib/python-musefs/tests/test_sync.py
git commit -m "feat(python-musefs): Record.delete_keys and merge flag on sync_one/sync_files"
```

---

## Task 3: Rewrite beets `map_fields` (boundary, rename, twins, formatters, drops)

**Files:**
- Modify: `contrib/beets/beetsplug/_core.py` (replace `DIRECT_FIELDS`, `TWIN_FIELDS`, `map_fields`, `_values`; keep `_to_int`, `_format_date`, art + path helpers)
- Test: `contrib/beets/tests/test_map_fields.py` (rewrite)

- [ ] **Step 1: Write the failing tests**

Replace the entire contents of `contrib/beets/tests/test_map_fields.py`:

```python
from types import SimpleNamespace

from beetsplug._core import map_fields

# Fields a real beets Item exposes via Item._media_tag_fields. Tests attach this
# so map_fields iterates the same boundary it will in production.
_TAG_FIELDS = (
    "title", "artist", "artists", "albumartist", "albumartists", "album",
    "genre", "genres", "composer", "composers", "comments", "grouping",
    "isrc", "lyrics", "bpm", "comp", "track", "tracktotal", "disc", "disctotal",
    "year", "month", "day",
    "rg_track_gain", "rg_album_gain", "rg_track_peak", "rg_album_peak",
    "mb_albumid", "mb_artistid", "mb_trackid",
    "artist_sort", "artists_sort",
    "bitrate", "length", "format",   # file facts: present on item, NOT tag fields
)
_FILE_FACTS = {"bitrate", "length", "format"}
_TAG_ONLY = tuple(f for f in _TAG_FIELDS if f not in _FILE_FACTS)


def item(**kw):
    base = {f: "" for f in _TAG_FIELDS}
    base.update({f: 0 for f in ("track", "tracktotal", "disc", "disctotal",
                                 "year", "month", "day", "bpm")})
    base.update({"comp": False, "bitrate": 320000, "length": 210.0, "format": "FLAC"})
    base.update(kw)
    ns = SimpleNamespace(**base)
    ns._media_tag_fields = _TAG_ONLY   # boundary excludes the file facts
    return ns


def test_core_fields_copied():
    d = dict(map_fields(item(title="Song", artist="Band", album="Disc")))
    assert d["title"] == "Song" and d["artist"] == "Band" and d["album"] == "Disc"


def test_track_disc_renamed_and_zero_dropped():
    d = dict(map_fields(item(track=7, disc=2)))
    assert d["tracknumber"] == "7" and d["discnumber"] == "2"
    assert "tracknumber" not in dict(map_fields(item(track=0)))


def test_replaygain_renamed_and_formatted():
    pairs = dict(map_fields(item(rg_track_gain=-7.5, rg_track_peak=0.987654321)))
    assert pairs["replaygain_track_gain"] == "-7.50 dB"
    assert pairs["replaygain_track_peak"].startswith("0.98")
    assert "rg_track_gain" not in pairs


def test_replaygain_zero_gain_survives():
    # 0 dB is a real measured value and must NOT be dropped.
    assert dict(map_fields(item(rg_track_gain=0.0)))["replaygain_track_gain"] == "0.00 dB"


def test_musicbrainz_renamed():
    d = dict(map_fields(item(mb_albumid="abc", mb_artistid="def", mb_trackid="ghi")))
    assert d["musicbrainz_albumid"] == "abc"
    assert d["musicbrainz_artistid"] == "def"
    assert d["musicbrainz_trackid"] == "ghi"


def test_comments_renamed_to_comment():
    assert dict(map_fields(item(comments="hi")))["comment"] == "hi"


def test_plural_artist_wins_and_expands():
    pairs = map_fields(item(artist="Joined", artists=["A", "B"]))
    artists = [v for k, v in pairs if k == "artist"]
    assert artists == ["A", "B"]              # plural list wins, one row each


def test_singular_artist_used_when_plural_empty():
    pairs = map_fields(item(artist="Solo", artists=[]))
    assert [v for k, v in pairs if k == "artist"] == ["Solo"]


def test_genre_plural_collapses_to_genre_key():
    pairs = map_fields(item(genres=["Rock", "Pop"]))
    assert [v for k, v in pairs if k == "genre"] == ["Rock", "Pop"]


def test_comp_one_kept_zero_dropped():
    # beets `comp` is a 0/1 int; bool here too. 1 -> kept "1", 0/False -> dropped.
    assert dict(map_fields(item(comp=True)))["comp"] == "1"
    assert dict(map_fields(item(comp=1)))["comp"] == "1"
    assert "comp" not in dict(map_fields(item(comp=False)))
    assert "comp" not in dict(map_fields(item(comp=0)))


def test_file_facts_excluded():
    d = dict(map_fields(item()))
    assert "bitrate" not in d and "length" not in d and "format" not in d


def test_date_assembled_and_parts_not_emitted():
    d = dict(map_fields(item(year=1999, month=3, day=5)))
    assert d["date"] == "1999-03-05"
    assert "year" not in d and "month" not in d and "day" not in d


def test_arbitrary_passthrough_lowercased():
    d = dict(map_fields(item(grouping="Set", isrc="US-X", lyrics="la")))
    assert d["grouping"] == "Set" and d["isrc"] == "US-X" and d["lyrics"] == "la"


def test_extra_fields_override_wins():
    # `fields:` maps a beets field onto a store key, last-wins.
    d = dict(map_fields(item(comments="orig", bpm=120), extra_fields={"bpm": "comment"}))
    assert d["comment"] == "120"   # override beat the comments->comment rename


def test_bpm_int_no_trailing_dot_zero():
    assert dict(map_fields(item(bpm=120)))["bpm"] == "120"
```

Note on `comp`: beets stores compilation as a `0`/`1` int (some stubs use a bool). A non-compilation carries no information, so this plan **drops `comp` when zero/False** (via `_DROP_IF_ZERO`) and emits `"1"` for a compilation. The test above encodes both the int and bool forms.

- [ ] **Step 2: Run to verify failure**

Run: `cd contrib/beets && python -m pytest tests/test_map_fields.py -v`
Expected: FAIL (old `map_fields` ignores `_media_tag_fields`, has no rename/twin/formatter logic).

- [ ] **Step 3: Rewrite the mapping in `_core.py`**

In `contrib/beets/beetsplug/_core.py`, replace the `DIRECT_FIELDS` and `TWIN_FIELDS` constants and the `_values`/`map_fields` functions with the following. Keep `_to_int`, `_format_date`, `_album_art_path`, `_read_album_art`, `_computed_path`, `_computed_path_or_skip`, and the existing imports (add nothing new beyond what is shown):

```python
MANAGED_FLEXATTR = "musefs_managed"

# beets field name -> canonical musefs (Vorbis-lowercase) key, where they differ.
RENAME = {
    "track": "tracknumber",
    "disc": "discnumber",
    "comments": "comment",
    "rg_track_gain": "replaygain_track_gain",
    "rg_album_gain": "replaygain_album_gain",
    "rg_track_peak": "replaygain_track_peak",
    "rg_album_peak": "replaygain_album_peak",
    "mb_trackid": "musicbrainz_trackid",
    "mb_albumid": "musicbrainz_albumid",
    "mb_artistid": "musicbrainz_artistid",
    "mb_albumartistid": "musicbrainz_albumartistid",
    "mb_releasegroupid": "musicbrainz_releasegroupid",
    "mb_releasetrackid": "musicbrainz_releasetrackid",
    "mb_workid": "musicbrainz_workid",
}

# plural beets list field -> the singular beets field it collapses onto. The
# plural wins when present; both resolve to one output key (via RENAME below) and
# are emitted once so a value is never written twice.
TWINS = {
    "artists": "artist",
    "albumartists": "albumartist",
    "genres": "genre",
    "composers": "composer",
    "lyricists": "lyricist",
    "arrangers": "arranger",
    "remixers": "remixer",
    "artists_sort": "artist_sort",
    "albumartists_sort": "albumartist_sort",
    "artists_credit": "artist_credit",
    "albumartists_credit": "albumartist_credit",
}

# Assembled into `date`; never emitted under their own names.
_DATE_PARTS = ("year", "month", "day")

# Output keys dropped when their numeric value is zero (0 is noise here). `comp`
# is a 0/1 int in beets, so dropping on zero covers both the int and bool forms
# of a non-compilation.
_DROP_IF_ZERO = {"tracknumber", "discnumber", "tracktotal", "disctotal", "comp"}

# Used when an item has no _media_tag_fields (older beets / non-beets test stubs).
FALLBACK_TAG_FIELDS = (
    "title", "artist", "artists", "albumartist", "albumartists", "album",
    "genre", "genres", "composer", "composers",
    "track", "disc", "year", "month", "day",
)


def _fmt_db(value):
    return f"{float(value):.2f} dB"


def _fmt_peak(value):
    return f"{float(value):.6f}"


# Per-output-key value formatters: the explicit exceptions to default stringify.
FORMATTERS = {
    "replaygain_track_gain": _fmt_db,
    "replaygain_album_gain": _fmt_db,
    "replaygain_track_peak": _fmt_peak,
    "replaygain_album_peak": _fmt_peak,
}


def _output_key(field):
    """beets field name -> canonical musefs store key (collapse twin, then rename)."""
    base = TWINS.get(field, field)
    return RENAME.get(base, base.lower())


def _stringify(value, output_key):
    """Render one beets value to a store string. Formatter exceptions first, then
    the default: bool -> '1'/'0', integral float -> no trailing '.0', int -> str,
    else str().strip()."""
    formatter = FORMATTERS.get(output_key)
    if formatter is not None:
        return formatter(value)
    if isinstance(value, bool):
        return "1" if value else "0"
    if isinstance(value, float):
        return str(int(value)) if value.is_integer() else str(value)
    if isinstance(value, int):
        return str(value)
    return str(value).strip()


def _iter_values(value):
    """A beets field value as a list of raw (un-stringified) elements."""
    if value is None:
        return []
    return list(value) if isinstance(value, (list, tuple)) else [value]


def _is_zero(text):
    try:
        return float(text) == 0.0
    except ValueError:
        return False


def map_fields(item, extra_fields=None):
    """Map a beets item to a list of (musefs_key, value) pairs covering every tag
    beets writes to a file (``item._media_tag_fields``), renamed to canonical
    keys, multi-values expanded, file facts excluded automatically. ``extra_fields``
    (the ``fields:`` config) is a final ``beets_field -> store_key`` override layer."""
    field_names = list(getattr(item, "_media_tag_fields", FALLBACK_TAG_FIELDS))
    # Process plural twins before singulars so the plural list wins its key.
    ordered = sorted(field_names, key=lambda f: (f not in TWINS, f))

    emitted = {}  # output_key -> list[str], insertion-ordered
    for field in ordered:
        if field in _DATE_PARTS:
            continue
        key = _output_key(field)
        if key in emitted:
            continue  # already claimed (plural beat singular, or a duplicate)
        values = []
        for raw in _iter_values(getattr(item, field, None)):
            if raw is None:
                continue
            if isinstance(raw, str) and not raw.strip():
                continue
            if isinstance(raw, bool) and not raw:
                continue  # comp=False etc. carry no info -> drop
            text = _stringify(raw, key)
            if not text:
                continue
            if key in _DROP_IF_ZERO and _is_zero(text):
                continue
            values.append(text)
        if values:
            emitted[key] = values

    date = _format_date(item)
    if date:
        emitted.setdefault("date", [date])

    if extra_fields:
        for beets_field, store_key in extra_fields.items():
            values = []
            for raw in _iter_values(getattr(item, beets_field, None)):
                if raw is None or (isinstance(raw, str) and not raw.strip()):
                    continue
                values.append(_stringify(raw, store_key))
            if values:
                emitted[store_key] = values  # override (last wins)

    return [(key, value) for key, values in emitted.items() for value in values]
```

- [ ] **Step 4: Run the rewritten file, then the WHOLE beets suite**

Run: `cd contrib/beets && python -m pytest tests/test_map_fields.py -v`
Expected: PASS (all tests in the rewritten file).

Then run the full suite — the new `map_fields` must not regress `test_build_records.py`
or `test_plugin.py`, which exercise it via `build_records` with conftest `FakeItem`
stubs that expose **no** `_media_tag_fields` (so `map_fields` uses `FALLBACK_TAG_FIELDS`):

Run: `cd contrib/beets && python -m pytest -q`
Expected: PASS (whole suite green at this commit).

- [ ] **Step 5: Commit**

```bash
git add contrib/beets/beetsplug/_core.py contrib/beets/tests/test_map_fields.py
git commit -m "feat(beets): map full _media_tag_fields with rename, twins, formatters"
```

---

## Task 4: Managed-state helpers, accumulating delete-keys, and merge-wired `_sync`

This task adds the managed-state helpers, rewrites `build_records` to use the
**accumulating-union** model (a deleted tag stays deleted across re-scans), and —
critically — updates `_sync` in the same commit, because `build_records`'s return
shape changes from a list to a `(records, managed_writes)` tuple and `_sync` is its
only production caller. Splitting these would leave the suite red at the commit.

**Files:**
- Modify: `contrib/beets/beetsplug/_core.py` (add helpers; rewrite `build_records`)
- Modify: `contrib/beets/beetsplug/musefs.py` (`_sync` → tuple unpack + merge + persist)
- Modify: `contrib/beets/tests/conftest.py` (`FakeItem` gains flexattr write + `store()`)
- Test: `contrib/beets/tests/test_build_records.py` (fix call sites), `contrib/beets/tests/test_managed_state.py` (create)

- [ ] **Step 1: Teach the conftest `FakeItem` to persist a flexattr**

`build_records` reads the `musefs_managed` flexattr (via `getattr`, already works)
and `_sync` writes it via `persist_managed` → `item[key] = value` + `item.store()`.
The real beets Item supports both; the conftest stub does not yet. In
`contrib/beets/tests/conftest.py`, add these two methods to `FakeItem` (right after
its `get_album` method):

```python
    def __setitem__(self, key, value):
        # beets flexattr write; readable back via getattr (used by read_managed).
        setattr(self, key, value)

    def store(self):
        self.stored = getattr(self, "stored", 0) + 1
```

- [ ] **Step 2: Write the failing tests**

Create `contrib/beets/tests/test_managed_state.py` (reuses the conftest `FakeItem`,
now flexattr-capable):

```python
from conftest import FakeItem

from musefs_common import SyncStats

from beetsplug import _core


def _item(**kw):
    return FakeItem(b"/m/a.flac", **kw)


def test_read_managed_empty_and_parsed():
    it = _item()
    assert _core.read_managed(it) == []
    it["musefs_managed"] = "artist,comment,title"
    assert _core.read_managed(it) == ["artist", "comment", "title"]


def test_format_managed_sorts_and_dedupes():
    assert _core.format_managed(["title", "artist", "artist"]) == "artist,title"


def test_persist_managed_writes_flexattr_via_store():
    # store() persists to the beets DB; it does NOT call write() (which writes the
    # audio file and fires after_write). The stub has no write() at all, so a
    # regression to write() would raise here. The real guarantee is beets' event
    # model (store != after_write) plus the e2e reconcile path (Task 8).
    it = _item()
    _core.persist_managed([(it, ["artist", "title"])])
    assert it.musefs_managed == "artist,title"
    assert it.stored == 1


def test_build_records_delete_keys_and_union_persist():
    it = _item(title="T", artist="A")
    it["musefs_managed"] = "artist,title,grouping"   # grouping was managed before
    records, writes = _core.build_records(
        [it], fields={}, stats=SyncStats(), write_path=False, restore_backing=False)
    rec = records[0]
    assert rec.delete_keys == ["grouping"]           # dropped from M -> delete
    assert ("title", "T") in rec.pairs and ("artist", "A") in rec.pairs
    item, managed = writes[0]
    assert item is it
    # UNION: grouping stays in musefs_managed as a tombstone so the delete sticks
    # across future re-scans (not just one cycle).
    assert set(managed) == {"title", "artist", "grouping"}


def test_build_records_restore_backing_clears_deletes_and_tombstones():
    it = _item(title="T")
    it["musefs_managed"] = "title,grouping"
    records, writes = _core.build_records(
        [it], fields={}, stats=SyncStats(), write_path=False, restore_backing=True)
    assert records[0].delete_keys == []              # no deletes under the flag
    assert set(writes[0][1]) == {"title"}            # tombstones cleared (reset to M)


def test_build_records_beets_path_is_managed():
    it = _item(title="T", destination=b"Artist/Album/01 T.flac")
    records, writes = _core.build_records(
        [it], fields={}, stats=SyncStats(), write_path=True, restore_backing=False)
    assert "beets_path" in {k for k, _ in records[0].pairs}
    assert "beets_path" in writes[0][1]              # included in the managed set
```

- [ ] **Step 3: Run to verify failure**

Run: `cd contrib/beets && python -m pytest tests/test_managed_state.py -v`
Expected: FAIL — `AttributeError: module 'beetsplug._core' has no attribute 'read_managed'` (and `build_records` returns a list, not a tuple).

- [ ] **Step 4: Add the helpers and rewrite `build_records`**

In `contrib/beets/beetsplug/_core.py`, add the three helpers (anywhere after `MANAGED_FLEXATTR`):

```python
def read_managed(item):
    """Parse the per-item ``musefs_managed`` flexattr into a list of keys."""
    raw = getattr(item, MANAGED_FLEXATTR, None)
    if not raw:
        return []
    return [k for k in str(raw).split(",") if k]


def format_managed(keys):
    """Serialize a managed key set: sorted, de-duplicated, comma-joined."""
    return ",".join(sorted(set(keys)))


def persist_managed(writes):
    """Persist each ``(item, managed_keys)`` pair into the beets DB via
    ``item.store()``. Never calls ``item.write()`` — that writes the audio file and
    fires ``after_write``, which would re-enter the plugin's reconcile loop."""
    for item, keys in writes:
        item[MANAGED_FLEXATTR] = format_managed(keys)
        item.store()
```

Then replace the existing `build_records` with the accumulating-union version:

```python
def build_records(items, *, fields=None, stats, write_path=True, restore_backing=False, log=None):
    """Build ``Record``s for beets items and the parallel managed-key writes.

    Returns ``(records, managed_writes)`` where ``managed_writes`` is a list of
    ``(item, managed_keys)`` the caller persists *after a successful commit* via
    ``persist_managed``.

    ``musefs_managed`` is an *accumulating* set (keys ever managed): each record's
    ``delete_keys`` is ``prev - keys(M)`` and the persisted set is the union
    ``prev | keys(M)``, so a key dropped from M stays a tombstone and keeps getting
    re-deleted on every sync until it re-enters M or ``restore_backing`` clears it.
    Under ``restore_backing`` no keys are deleted and the set is reset to ``keys(M)``
    (tombstones forgotten), so restored backing values stay visible."""
    records = []
    managed_writes = []
    art_cache = {}
    for item in items:
        cover = _read_album_art(item, art_cache, stats)
        pairs = map_fields(item, fields)
        if write_path:
            path = _computed_path_or_skip(item, log)
            if path:
                pairs.append(("beets_path", path))
        keys_now = {key for key, _ in pairs}
        prev = set(read_managed(item))
        if restore_backing:
            delete_keys = []
            managed = sorted(keys_now)
        else:
            delete_keys = sorted(prev - keys_now)
            managed = sorted(prev | keys_now)
        records.append(
            Record(
                key=realpath_key(item.path),
                pairs=pairs,
                art=[ArtImage(*cover)] if cover else None,
                delete_keys=delete_keys,
            )
        )
        managed_writes.append((item, managed))
    return records, managed_writes
```

- [ ] **Step 5: Update `_sync` to the merge + persist version (same commit)**

In `contrib/beets/beetsplug/musefs.py`, replace `_sync` so it unpacks the new tuple,
syncs with `merge=True`, and persists managed state after commit. The
`restore_backing` parameter defaults `False` here; Task 5 wires the config/flag that
feeds it (until then, callers use the default and behavior is "deletions stick"):

```python
    def _sync(self, db_path, items, dry_run=False, restore_backing=False):
        if not os.path.exists(db_path):
            raise ui.UserError(
                f"musefs: DB not found at {db_path}; enable `musefs.autoscan` "
                f"or run `musefs scan` first"
            )
        conn = connect(db_path)
        try:
            check_schema_version(conn)
            stats = SyncStats()
            records, managed_writes = _core.build_records(
                items,
                fields=self._fields(),
                stats=stats,
                write_path=self._write_path(),
                restore_backing=restore_backing,
                log=self._log,
            )
            sync_files(conn, records, dry_run=dry_run, stats=stats, merge=True)
            if dry_run:
                conn.rollback()
            else:
                conn.commit()
                _core.persist_managed(managed_writes)
            return stats
        except SchemaMismatch as exc:
            conn.rollback()
            raise ui.UserError(f"musefs: {exc}")
        finally:
            conn.close()
```

- [ ] **Step 6: Fix existing `test_build_records.py` call sites**

`test_build_records.py` unpacks `build_records(...)` as a single list. Update each
call site to `records, _ = build_records(...)`. Run the file and fix every failure:

Run: `cd contrib/beets && python -m pytest tests/test_build_records.py -v`
Expected after fixes: PASS.

- [ ] **Step 7: Run the WHOLE beets suite (catches the `_sync`/`FakeItem` coupling)**

Run: `cd contrib/beets && python -m pytest -q`
Expected: PASS. This must include `test_plugin.py` — those tests drive `_command`/
`_reconcile_pending` → `_sync` → `persist_managed`, which now calls `item.store()` on
the conftest `FakeItem` (Step 1) and goes through `merge_tags` (Task 2). They use
`FALLBACK_TAG_FIELDS` because `FakeItem` exposes no `_media_tag_fields`, and the
`make_track` fixture seeds no text tags, so merge produces the same rows full-replace
did. If any `test_plugin.py` test is red here, the coupling above is the cause.

- [ ] **Step 8: Commit**

```bash
git add contrib/beets/beetsplug/_core.py contrib/beets/beetsplug/musefs.py \
        contrib/beets/tests/conftest.py \
        contrib/beets/tests/test_managed_state.py \
        contrib/beets/tests/test_build_records.py
git commit -m "feat(beets): accumulating managed-state flexattr + merge-wired _sync"
```

---

## Task 5: Config + flag + both-paths wiring (`--restore-backing`)

`_sync` already merges and persists (Task 4). This task adds the `restore_backing`
config/flag and threads it into both sync paths, then proves the merge + sticky-delete
behavior end to end through the plugin (including the passive `cli_exit` path).

**Files:**
- Modify: `contrib/beets/beetsplug/musefs.py` (config key, command option, helper, `_command`, `_reconcile_pending`)
- Test: `contrib/beets/tests/test_plugin.py` (extend — reuse its `FakeConfigView` + `monkeypatch` pattern)

- [ ] **Step 1: Write the failing tests**

Append to `contrib/beets/tests/test_plugin.py`. Reuse the module's existing
`FakeConfigView` (defined at the top of that file) and the `monkeypatch.setattr(plugin,
"config", FakeConfigView({...}), raising=False)` pattern used by the existing tests —
do **not** assign `plugin.config[...] = ...` (the real config view is read-only here):

```python
import os
import sqlite3

from musefs_common import connect
from musefs_common.schema import SCHEMA_SQL

from beetsplug.musefs import MusefsPlugin
from conftest import FakeItem, insert_track


def _seed_track_with_tag(db_path, real_path, key, value):
    conn = connect(db_path)
    tid = insert_track(conn, real_path)
    conn.execute("INSERT INTO tags (track_id, key, value, ordinal) VALUES (?,?,?,0)",
                 (tid, key, value))
    conn.commit()
    conn.close()
    return tid


def _text_tags(db_path, tid):
    conn = connect(db_path)
    rows = dict(conn.execute(
        "SELECT key, value FROM tags WHERE track_id=? AND value_blob IS NULL", (tid,)))
    conn.close()
    return rows


def test_sync_merges_keeps_unmanaged_and_persists(db_path, tmp_path, monkeypatch):
    """Command path: B persists, M wins, managed flexattr written via store()."""
    p = tmp_path / "a.flac"; p.write_bytes(b"")
    real = os.path.realpath(str(p))
    tid = _seed_track_with_tag(db_path, real, "comment", "keep")

    item = FakeItem(str(p).encode(), artist="New")
    plugin = MusefsPlugin()
    monkeypatch.setattr(
        plugin, "config",
        FakeConfigView({"db": db_path, "fields": {}, "write_path": False,
                        "restore_backing": False}),
        raising=False,
    )
    plugin._sync(db_path, [item], dry_run=False, restore_backing=False)

    tags = _text_tags(db_path, tid)
    assert tags["artist"] == "New"      # M wins
    assert tags["comment"] == "keep"    # unmanaged B persists (merge, not replace)
    assert "artist" in item.musefs_managed   # managed set persisted via store()


def test_reconcile_path_merges_and_sticky_deletes(db_path, tmp_path, monkeypatch):
    """Passive cli_exit path runs the same merge + managed-state cycle, and a key
    dropped from a prior managed set is deleted (tombstone)."""
    p = tmp_path / "b.flac"; p.write_bytes(b"")
    real = os.path.realpath(str(p))
    tid = _seed_track_with_tag(db_path, real, "grouping", "old")

    item = FakeItem(str(p).encode(), title="T")
    item["musefs_managed"] = "grouping,title"   # grouping managed before; now dropped
    plugin = MusefsPlugin()
    monkeypatch.setattr(
        plugin, "config",
        FakeConfigView({"db": db_path, "fields": {}, "write_path": False,
                        "autoscan": False, "restore_backing": False}),
        raising=False,
    )
    monkeypatch.setattr(plugin, "_run_scan", lambda db, targets: None)
    plugin._pending = [item]
    plugin._reconcile_pending(lib=None)

    tags = _text_tags(db_path, tid)
    assert tags.get("title") == "T"     # merged on the reconcile path
    assert "grouping" not in tags       # tombstoned delete applied on the reconcile path


def test_restore_backing_skips_deletes(db_path, tmp_path, monkeypatch):
    """With restore_backing, a previously-managed-now-dropped key is NOT deleted."""
    p = tmp_path / "c.flac"; p.write_bytes(b"")
    real = os.path.realpath(str(p))
    tid = _seed_track_with_tag(db_path, real, "grouping", "frombacking")

    item = FakeItem(str(p).encode(), title="T")
    item["musefs_managed"] = "grouping,title"
    plugin = MusefsPlugin()
    monkeypatch.setattr(
        plugin, "config",
        FakeConfigView({"db": db_path, "fields": {}, "write_path": False,
                        "restore_backing": True}),
        raising=False,
    )
    plugin._sync(db_path, [item], dry_run=False, restore_backing=True)

    tags = _text_tags(db_path, tid)
    assert tags["grouping"] == "frombacking"   # backing value left in place
    assert set(item.musefs_managed.split(",")) == {"title"}  # tombstones cleared
```

Note: the existing `FakeConfigView` (`test_plugin.py` top) returns raw data and its
`get(template)` ignores the template, so the existing tests whose config dicts omit
`restore_backing` still work — `self.config["restore_backing"].get(bool)` degrades to
`None` → `bool(None)` = `False`. Do **not** edit those existing fixtures.

- [ ] **Step 2: Run to verify failure**

Run: `cd contrib/beets && python -m pytest tests/test_plugin.py -v -k "restore_backing or merges"`
Expected: FAIL — `_restore_backing` / `--restore-backing` not present; `_command`/
`_reconcile_pending` don't thread `restore_backing`.

- [ ] **Step 3: Update the adapter**

In `contrib/beets/beetsplug/musefs.py`:

(a) Add the config default in `__init__`'s `self.config.add({...})`:
```python
            "restore_backing": False,  # on delete, let the backing tag value reappear
```

(b) Add the command option in `commands()` (after the `--dry-run` option):
```python
        cmd.parser.add_option(
            "--restore-backing",
            dest="restore_backing",
            action="store_true",
            default=False,
            help="when a tag is removed in beets, let the backing file's value reappear",
        )
```

(c) Add a config helper near `_write_path`:
```python
    def _restore_backing(self):
        return bool(self.config["restore_backing"].get(bool))
```

(d) In `_command`, resolve the effective flag and pass it through (replace the existing
`stats = self._sync(db_path, items, dry_run=opts.dry_run)` line):
```python
        restore_backing = bool(opts.restore_backing) or self._restore_backing()
        stats = self._sync(db_path, items, dry_run=opts.dry_run, restore_backing=restore_backing)
```

(e) In `_reconcile_pending`, thread the config default (replace the existing
`self._sync(db_path, items)` call):
```python
            self._sync(db_path, items, restore_backing=self._restore_backing())
```

- [ ] **Step 4: Run the full beets suite**

Run: `cd contrib/beets && python -m pytest -q`
Expected: PASS (new tests green; existing `FakeConfigView` tests unaffected — see the
Step 1 note on the benign `restore_backing` default).

- [ ] **Step 5: Commit**

```bash
git add contrib/beets/beetsplug/musefs.py contrib/beets/tests/test_plugin.py
git commit -m "feat(beets): restore_backing config + --restore-backing on both sync paths"
```

---

## Task 6: Picard naming additions (no merge)

**Files:**
- Modify: `contrib/picard/musefs/_core.py` (`DIRECT_FIELDS` only — the additions are all single-valued, so `_MULTI_VALUE_KEYS` is unchanged)
- Test: `contrib/picard/tests/test_map_fields.py` (extend)

- [ ] **Step 1: Write the failing test**

Append to `contrib/picard/tests/test_map_fields.py` (reuse the file's existing fake-`Metadata` helper):

```python
def test_replaygain_and_musicbrainz_and_comment_mapped():
    md = metadata(  # the file's existing dict-like Metadata stub
        replaygain_track_gain="-7.50 dB",
        musicbrainz_albumid="abc",
        comment="hello",
        lyrics="la",
        grouping="set",
        isrc="US-X",
    )
    d = dict(map_fields(md))
    assert d["replaygain_track_gain"] == "-7.50 dB"
    assert d["musicbrainz_albumid"] == "abc"
    assert d["comment"] == "hello"
    assert d["lyrics"] == "la" and d["grouping"] == "set" and d["isrc"] == "US-X"
```

If the existing test file has no `metadata()` helper, construct the stub the same way the neighboring tests in that file do (Picard `Metadata` is dict-like with `getall`).

- [ ] **Step 2: Run to verify failure**

Run: `cd contrib/picard && python -m pytest tests/test_map_fields.py -v -k replaygain_and_musicbrainz`
Expected: FAIL — keys absent (not in `DIRECT_FIELDS`).

- [ ] **Step 3: Extend Picard's `DIRECT_FIELDS`**

In `contrib/picard/musefs/_core.py`, extend `DIRECT_FIELDS` (Picard's internal tag names already equal musefs keys, so these are identity entries):

```python
DIRECT_FIELDS = {
    "title": "title",
    "artist": "artist",
    "albumartist": "albumartist",
    "album": "album",
    "genre": "genre",
    "composer": "composer",
    "tracknumber": "tracknumber",
    "discnumber": "discnumber",
    "date": "date",
    "comment": "comment",
    "lyrics": "lyrics",
    "grouping": "grouping",
    "isrc": "isrc",
    "replaygain_track_gain": "replaygain_track_gain",
    "replaygain_album_gain": "replaygain_album_gain",
    "replaygain_track_peak": "replaygain_track_peak",
    "replaygain_album_peak": "replaygain_album_peak",
    "musicbrainz_albumid": "musicbrainz_albumid",
    "musicbrainz_artistid": "musicbrainz_artistid",
}
```

(Picard already formats ReplayGain with the `dB` suffix and the MusicBrainz IDs under these exact keys, so no value formatting is needed here.)

- [ ] **Step 4: Run to verify pass + full Picard unit suite**

Run: `cd contrib/picard && python -m pytest -q`
Expected: PASS. (Per repo notes, the real-Picard/Qt tests skip without an importable Picard; the pure-mapping tests here run regardless.)

- [ ] **Step 5: Commit**

```bash
git add contrib/picard/musefs/_core.py contrib/picard/tests/test_map_fields.py
git commit -m "feat(picard): map ReplayGain, MusicBrainz, comment, lyrics, grouping, isrc"
```

---

## Task 7: Documentation

**Files:**
- Modify: `contrib/beets/README.md`
- Modify: `contrib/python-musefs/README.md`
- Modify: `ARCHITECTURE.md`

- [ ] **Step 1: beets README**

In `contrib/beets/README.md`, under "Notes", replace the field-coverage description with the new behavior. Add bullets covering: (a) the plugin now syncs everything beets writes to a file (`_media_tag_fields`) — ReplayGain, MusicBrainz IDs, comment, lyrics, grouping, isrc, multi-valued artists — under canonical names; (b) **merge semantics**: beets values win, the file's other embedded tags are preserved; (c) the `musefs_managed` beets flexattr records what the plugin manages, so deleting a tag in beets removes it from the view and it **stays** removed; (d) `--restore-backing` / `restore_backing: no` brings the backing value back for a removed tag; (e) the caveat that sticky deletion depends on `autoscan: yes` — with `autoscan: no`, a deletion reconciles only on the next manual `musefs scan`.

Use this block (insert under Notes):
```markdown
- **Field coverage:** every tag beets writes to a file (its `_media_tag_fields`)
  is synced — ReplayGain, MusicBrainz IDs, comment, lyrics, grouping, isrc,
  multi-valued artists, and any custom field — under canonical musefs keys.
  Read-only file facts (bitrate, length, …) are never written as tags.
- **Merge, not replace:** beets' values win for the fields it manages; any other
  tag already embedded in the file is preserved in the view.
- **Deletions stick:** the plugin records the keys it manages per track in a
  `musefs_managed` beets flexattr (stored in the beets DB only — never in your
  audio files or the musefs store). Remove a tag in beets and it is removed from
  the view and stays gone across re-scans.
- **`--restore-backing`** (or `restore_backing: yes`): when you remove a tag in
  beets, let the file's original embedded value reappear instead of disappearing.
- **Caveat:** sticky deletion relies on `autoscan: yes` (the default), which
  re-derives the file's embedded tags before each sync. With `autoscan: no`, a
  deletion only takes effect after your next manual `musefs scan`.
```

- [ ] **Step 2: python-musefs README**

In `contrib/python-musefs/README.md`, document `merge_tags` next to `replace_tags`:
```markdown
- `merge_tags(conn, track_id, managed_pairs, delete_keys)` — per-key replacement
  of plugin-managed text tags. Unlike `replace_tags` (which clears all text rows),
  `merge_tags` clears only the keys it rewrites plus `delete_keys`, leaving other
  scan-seeded text tags intact. Scanner-written binary tags survive either way.
```

- [ ] **Step 3: ARCHITECTURE.md external-writer contract**

In `ARCHITECTURE.md`, in the external-writer-contract section, add a paragraph: an external writer may **merge** rather than fully replace text tags — overwriting only the keys it manages and leaving the rest of the scan-seeded set in place — provided it tracks its own managed-key set out of band (the beets plugin uses a beets flexattr; the store is not the place for plugin state). Note that musefs renders tags outside its native VOCAB (`musefs-format/src/tagmap.rs`) by passthrough (Vorbis uppercased, mp3 `TXXX`, mp4 freeform), so such tags appear but are not guaranteed byte-identical to a given tagger's own per-format encoding.

- [ ] **Step 4: Commit**

```bash
git add contrib/beets/README.md contrib/python-musefs/README.md ARCHITECTURE.md
git commit -m "docs: document beets merge sync, musefs_managed, merge_tags contract"
```

---

## Task 8: End-to-end coverage

**Files:**
- Modify: `contrib/beets/tests/test_e2e.py`

This tier is **skip-acceptable**: it is gated on `ffmpeg` + `/dev/fuse` +
`fusermount` and skips cleanly when absent. The unit/integration tiers (Tasks 1–5)
carry the correctness load; this is the real-mount confirmation, and notably the
**multi-cycle** sticky-delete check that would have caught the one-cycle bug.

- [ ] **Step 1: Add a concrete e2e test**

Add a new `@pytest.mark.e2e` test to `contrib/beets/tests/test_e2e.py`, built from the
file's real helpers (`_imported_library` → `(cfg, env, db, mnt, library)`,
`_beet(cfg, env, *args)`, the `_mounted(mnt, db, template)` context manager) and
modeled on `test_e2e_import_retag_mount_playback`. Read tags with `FLAC(str(path))`
(already imported as `from mutagen.flac import FLAC`) so Vorbis keys like
`replaygain_track_gain` / `musicbrainz_albumid` are visible — `easy=True` would hide
them. FLAC-focused so the assertions are format-idiomatic per the spec §2 fidelity note.

```python
def test_e2e_full_fields_sticky_delete_and_restore(tmp_path):
    """Rich fields reach the mount; a deleted file-embedded tag stays gone across
    re-scans; --restore-backing brings the backing value back."""
    cfg, env, db, mnt, library = _imported_library(tmp_path)
    template = "$albumartist/$album/$title"

    # Embed a comment INTO the FLAC (no -W -> beets writes the file), so the tag
    # exists in the backing file (B), not just the beets DB.
    _beet(cfg, env, "modify", "-M", "-y", "format:FLAC",
          "comments=from file", "rg_track_gain=-7.5",
          "mb_albumid=11111111-1111-1111-1111-111111111111")
    _beet(cfg, env, "musefs")
    with _mounted(mnt, db, template):
        ft = FLAC(str(next(mnt.rglob("*.flac"))))
        assert ft["replaygain_track_gain"][0].endswith("dB")
        assert ft["musicbrainz_albumid"][0].startswith("11111111")
        assert ft["comment"][0] == "from file"

    # Delete the comment in beets WITHOUT writing the file (-W): the FLAC on disk
    # still embeds "from file", so this is the case the union model must handle.
    _beet(cfg, env, "modify", "-W", "-M", "-y", "format:FLAC", "comments!")
    _beet(cfg, env, "musefs")                       # cycle 1: delete applied
    _beet(cfg, env, "musefs")                       # cycle 2: must STAY gone
    with _mounted(mnt, db, template):
        ft = FLAC(str(next(mnt.rglob("*.flac"))))
        assert "comment" not in ft                  # tombstone held across re-scan

    # --restore-backing: the file's embedded "from file" comment returns.
    _beet(cfg, env, "musefs", "--restore-backing")
    with _mounted(mnt, db, template):
        ft = FLAC(str(next(mnt.rglob("*.flac"))))
        assert ft["comment"][0] == "from file"
```

If beets' field name for a tag differs on your beets version (e.g. it rejects
`rg_track_gain` as a settable field), set it via the `fields:` config or skip that one
assertion — the comment sticky-delete cycle is the load-bearing check.

- [ ] **Step 2: Run the e2e tier (requires tools)**

Run: `cd contrib/beets && python -m pytest -m e2e -v`
Expected: PASS if `ffmpeg` + `/dev/fuse` + `fusermount` are present; otherwise the tier
skips cleanly (acceptable — record the skip rather than treating it as success).

- [ ] **Step 3: Commit**

```bash
git add contrib/beets/tests/test_e2e.py
git commit -m "test(beets): e2e coverage for full-field merge, sticky delete, restore-backing"
```

---

## Final verification

- [ ] **Run every Python suite green:**

```bash
cd contrib/python-musefs && python -m pytest -q
cd ../beets && python -m pytest -q          # default tier (no Rust binary)
cd ../picard && python -m pytest -q
```

- [ ] **Optional, with tools:** `cd contrib/beets && python -m pytest -m musefs_bin -q` (path gate vs the real `musefs` binary — build it first from repo root with `cargo build`), then `python -m pytest -m e2e -q`.

- [ ] **No Rust/schema touched:** confirm `git diff --name-only main...HEAD` lists only files under `contrib/`, `ARCHITECTURE.md`, and `docs/superpowers/`. No `*.rs`, no `schema.py`/`schema.rs` — so the Rust workspace build and the schema-mirror gate are unaffected.

---

## Spec-coverage self-check (done while writing — recorded for the executor)

- Field boundary `_media_tag_fields` + auto file-fact exclusion → Task 3 (`map_fields`, `FALLBACK_TAG_FIELDS`, `test_file_facts_excluded`).
- Drop predicate, numeric-zero kept, ReplayGain 0 dB survives → Task 3 (`_DROP_IF_ZERO`, `test_replaygain_zero_gain_survives`).
- `fields:` override precedence (last-wins; may re-introduce a fact) → Task 3 (`extra_fields` block, `test_extra_fields_override_wins`).
- Rename table (`rg_*`, `mb_*`, `comments`) → Task 3 (`RENAME`).
- Twin pre-pass, plural-wins, multi-artist fix → Task 3 (`TWINS`, ordered loop, `test_plural_artist_wins_and_expands`).
- Stringification (bool `1`/`0`, int no `.0`) + formatter exceptions → Task 3 (`_stringify`, `FORMATTERS`).
- `beets_path` is a managed key → Task 4 (`build_records`, `test_build_records_beets_path_is_managed`).
- `merge_tags` with `value_blob IS NULL` scoping + per-key ordinals → Task 1.
- `Record.delete_keys` + `merge` flag (Picard unaffected by default) → Task 2 (`test_sync_files_default_is_full_replace`).
- `musefs_managed` flexattr read/compute/write via `item.store()` → Task 4 (`persist_managed`, `test_persist_managed_writes_flexattr_via_store`).
- **Accumulating-union model — deletions stay deleted across re-scans** (spec §4 union) → Task 4 (`build_records` persists `prev | keys(M)`, `test_build_records_delete_keys_and_union_persist`) + Task 8 multi-cycle e2e.
- Both sync paths (command + `_reconcile_pending`) → Task 4 (`_sync` shared by both) + Task 5 (`_reconcile_pending` threads `restore_backing`; `test_reconcile_path_merges_and_sticky_deletes`).
- `--restore-backing` / `restore_backing` default off; resets tombstones → Task 5 (`test_restore_backing_skips_deletes`).
- Picard naming-only → Task 6.
- Docs (beets, python-musefs, ARCHITECTURE) → Task 7.
- e2e + autoscan-off caveat documented → Task 8 + Task 7.
- Query/partial syncs per-item safe (spec §5) → true by construction (`build_records` iterates only passed `items`; each owns its flexattr); not separately tested — noted as a known minor coverage gap.
