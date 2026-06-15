# Picard Full Tag-Set Mapping Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the Picard plugin's static ~20-field whitelist with dynamic enumeration of Picard's full metadata, emitting standard on-disk store keys, and realign the beets sibling's three outlier keys so both plugins converge.

**Architecture:** `map_fields` enumerates `metadata.rawitems()`, skips `~`-hidden vars, canonicalizes each key via a `RENAME` swap table + colon-suffix handling (performer role-fold; comment/lyrics collapse), accumulates multi-values, and drops zero numerics. The beets plugin adds three `RENAME` entries (`artist_sort→artistsort`, `albumartist_sort→albumartistsort`, `comp→compilation`) so its on-disk-standard keys match Picard's. No Rust, schema, or shared-lib changes — store keys for non-VOCAB fields round-trip verbatim as the synthesized file's field name.

**Tech Stack:** Python 3 (Picard plugin: vendored `musefs._common`, no Picard import in `_core.py`; beets plugin: `beetsplug` + `python-musefs`), pytest.

**Spec:** `docs/superpowers/specs/2026-06-15-picard-full-tagset-mapping-design.md`

---

## File Structure

- `contrib/picard/musefs/_core.py` — **modify**: replace the mapping block (lines 15–105 inclusive: the `# Picard internal tag name` comment, `DIRECT_FIELDS`, `_NUMERIC_KEYS`, `MusefsError`, `_to_int`, `_values`, `_first_value`, `_MULTI_VALUE_KEYS`, `map_fields`) with `RENAME`, `_DROP_IF_ZERO`, `MusefsError` (re-included verbatim), a float-aware `_is_zero` (replacing `_to_int`), `_output_key`, and the rewritten `map_fields`. Do not touch line 14 (`from ._common.sync import ArtImage`) or above; everything from `_picture_type` onward stays untouched.
- `contrib/picard/tests/conftest.py` — **modify**: add `rawitems()` to `FakeMetadata`.
- `contrib/picard/tests/test_map_fields.py` — **rewrite**: new dynamic-shape test suite.
- `contrib/beets/beetsplug/_core.py` — **modify**: 3 new `RENAME` entries; `comp`→`compilation` in `_DROP_IF_ZERO`.
- `contrib/beets/tests/test_map_fields.py` — **modify**: rename `comp` test to `compilation`; add a sort-rename test (+ `albumartist_sort` to the stub `_TAG_FIELDS`).
- `contrib/picard/README.md`, `contrib/beets/README.md` — **modify if** they enumerate field keys.

Note: `contrib/picard/musefs/__init__.py` imports only `map_fields` (public API, signature unchanged: `map_fields(metadata, extra_fields=None)`), so no caller changes are needed. `test_batch_scan.py` monkeypatches `map_fields` with a compatible `(md, fields)` lambda — leave it.

---

## Task 1: Picard — dynamic full-tag-set mapping

**Files:**
- Modify: `contrib/picard/tests/conftest.py` (FakeMetadata)
- Test: `contrib/picard/tests/test_map_fields.py` (full rewrite)
- Modify: `contrib/picard/musefs/_core.py:15-105`

All commands run from `contrib/picard`. No venv or Picard import needed for these stub-based tests.

- [ ] **Step 1: Add `rawitems()` to the FakeMetadata stub**

In `contrib/picard/tests/conftest.py`, the `FakeMetadata` class currently ends with `getall`. Add a `rawitems` method right after it:

```python
    def getall(self, key):
        return self._tags.get(key, [])

    def rawitems(self):
        return self._tags.items()
```

Leave a comment-free addition; the surrounding class already documents itself. (The stub stores keys verbatim and does NOT replicate Picard's `normalize_tag` trailing-colon stripping — so tests use post-normalize key forms: `comment`, `performer`, never `comment:`.)

- [ ] **Step 2: Rewrite `test_map_fields.py` with the new suite**

Replace the **entire** contents of `contrib/picard/tests/test_map_fields.py` with:

```python
from musefs._core import map_fields


def test_direct_fields_copied(fake_metadata):
    d = dict(map_fields(fake_metadata(title="Song", artist="Band", album="Disc")))
    assert d["title"] == "Song"
    assert d["artist"] == "Band"
    assert d["album"] == "Disc"


def test_all_fields_multi_value_expand(fake_metadata):
    # The old _MULTI_VALUE_KEYS allowlist is gone: every field emits one row per
    # value, order preserved.
    pairs = map_fields(fake_metadata(artist=["First", "Second"]))
    assert [v for k, v in pairs if k == "artist"] == ["First", "Second"]
    pairs = map_fields(fake_metadata(genre=["Rock", "Pop"]))
    assert [v for k, v in pairs if k == "genre"] == ["Rock", "Pop"]
    pairs = map_fields(fake_metadata(mood=["Happy", "Sad"]))
    assert [v for k, v in pairs if k == "mood"] == ["Happy", "Sad"]


def test_empty_and_whitespace_omitted(fake_metadata):
    assert dict(map_fields(fake_metadata(title="", artist="   "))) == {}


def test_tracknumber_discnumber_passthrough_and_zero_dropped(fake_metadata):
    d = dict(map_fields(fake_metadata(tracknumber="7", discnumber="2")))
    assert d["tracknumber"] == "7" and d["discnumber"] == "2"
    z = dict(map_fields(fake_metadata(tracknumber="0", discnumber="0")))
    assert "tracknumber" not in z and "discnumber" not in z


def test_date_passthrough(fake_metadata):
    assert dict(map_fields(fake_metadata(date="1999-03-05")))["date"] == "1999-03-05"


def test_replaygain_and_misc_passthrough(fake_metadata):
    d = dict(
        map_fields(
            fake_metadata(
                replaygain_track_gain="-7.50 dB",
                musicbrainz_albumid="abc",
                grouping="set",
                isrc="US-X",
                label="Label",
            )
        )
    )
    assert d["replaygain_track_gain"] == "-7.50 dB"
    assert d["musicbrainz_albumid"] == "abc"
    assert d["grouping"] == "set" and d["isrc"] == "US-X" and d["label"] == "Label"


def test_musicbrainz_id_swap(fake_metadata):
    # Picard's recording id is the on-disk musicbrainz_trackid; Picard's track id
    # is the on-disk musicbrainz_releasetrackid. Both source vars present together.
    d = dict(
        map_fields(
            fake_metadata(musicbrainz_recordingid="rec", musicbrainz_trackid="trk")
        )
    )
    assert d["musicbrainz_trackid"] == "rec"
    assert d["musicbrainz_releasetrackid"] == "trk"
    assert "musicbrainz_recordingid" not in d


def test_musicbrainz_ids_passthrough(fake_metadata):
    d = dict(
        map_fields(
            fake_metadata(
                musicbrainz_albumid="al",
                musicbrainz_artistid="ar",
                musicbrainz_albumartistid="aa",
                musicbrainz_releasegroupid="rg",
                musicbrainz_workid="wk",
            )
        )
    )
    assert d["musicbrainz_albumid"] == "al"
    assert d["musicbrainz_artistid"] == "ar"
    assert d["musicbrainz_albumartistid"] == "aa"
    assert d["musicbrainz_releasegroupid"] == "rg"
    assert d["musicbrainz_workid"] == "wk"


def test_sort_fields_passthrough(fake_metadata):
    d = dict(map_fields(fake_metadata(artistsort="B, The", albumartistsort="A, An")))
    assert d["artistsort"] == "B, The"
    assert d["albumartistsort"] == "A, An"


def test_artist_and_artists_both_emitted(fake_metadata):
    # Picard exposes the credited join string (artist) and the individual list
    # (artists) as distinct on-disk tags; emit both, no twin-collapse.
    pairs = map_fields(
        fake_metadata(
            artist="Alice & Bob",
            artists=["Alice", "Bob"],
            albumartist="Alice & Bob",
            albumartists=["Alice", "Bob"],
        )
    )
    assert [v for k, v in pairs if k == "artist"] == ["Alice & Bob"]
    assert [v for k, v in pairs if k == "artists"] == ["Alice", "Bob"]
    assert [v for k, v in pairs if k == "albumartist"] == ["Alice & Bob"]
    assert [v for k, v in pairs if k == "albumartists"] == ["Alice", "Bob"]


def test_movement_swap(fake_metadata):
    d = dict(map_fields(fake_metadata(movement="Allegro", movementnumber="1")))
    assert d["movementname"] == "Allegro"
    assert d["movement"] == "1"


def test_totals_renamed_and_zero_dropped(fake_metadata):
    d = dict(map_fields(fake_metadata(totaltracks="12", totaldiscs="2")))
    assert d["tracktotal"] == "12" and d["disctotal"] == "2"
    z = dict(map_fields(fake_metadata(totaltracks="0", totaldiscs="0")))
    assert "tracktotal" not in z and "disctotal" not in z


def test_compilation_and_bpm_zero_dropped(fake_metadata):
    assert dict(map_fields(fake_metadata(compilation="1")))["compilation"] == "1"
    assert "compilation" not in dict(map_fields(fake_metadata(compilation="0")))
    assert dict(map_fields(fake_metadata(bpm="120")))["bpm"] == "120"
    assert "bpm" not in dict(map_fields(fake_metadata(bpm="0")))
    assert "bpm" not in dict(map_fields(fake_metadata(bpm="0.0")))
    # a fractional, non-zero value must survive (float-aware zero check)
    assert dict(map_fields(fake_metadata(bpm="128.5")))["bpm"] == "128.5"


def test_performer_role_folded_into_value(fake_metadata):
    pairs = map_fields(fake_metadata(**{"performer:Piano": "Joe Barr"}))
    assert pairs == [("performer", "Joe Barr (Piano)")]


def test_performer_bare_value_only(fake_metadata):
    pairs = map_fields(fake_metadata(performer="Joe Barr"))
    assert pairs == [("performer", "Joe Barr")]


def test_performer_multiple_roles_accumulate(fake_metadata):
    pairs = map_fields(
        fake_metadata(
            **{
                "performer:Piano": ["Joe Barr"],
                "performer:Guitar": ["Ann Lee", "Max Roe"],
            }
        )
    )
    performers = sorted(v for k, v in pairs if k == "performer")
    assert performers == ["Ann Lee (Guitar)", "Joe Barr (Piano)", "Max Roe (Guitar)"]


def test_comment_and_lyrics_collapse(fake_metadata):
    # Bare and described forms collapse to the base key; description dropped.
    d = dict(map_fields(fake_metadata(**{"comment:eng": "hello", "lyrics": "la"})))
    assert d["comment"] == "hello"
    assert d["lyrics"] == "la"


def test_comment_descriptions_accumulate(fake_metadata):
    pairs = map_fields(fake_metadata(**{"comment": "main", "comment:eng": "english"}))
    assert sorted(v for k, v in pairs if k == "comment") == ["english", "main"]


def test_hidden_vars_skipped(fake_metadata):
    pairs = map_fields(
        fake_metadata(**{"~length": "210000", "~rating": "5", "title": "Keep"})
    )
    keys = {k for k, _ in pairs}
    assert keys == {"title"}


def test_extra_fields_override_verbatim(fake_metadata):
    # The options-page map adds/overrides a store key, last-wins, value verbatim
    # (no role-fold, no zero-drop). Other fields' natural mapping is unaffected.
    md = fake_metadata(title="Song", **{"performer:Piano": "Joe Barr"})
    d = dict(map_fields(md, extra_fields={"performer:Piano": "soloist"}))
    assert d["soloist"] == "Joe Barr"  # verbatim: NOT "Joe Barr (Piano)"
    assert d["title"] == "Song"


def test_extra_fields_override_replaces_target_key(fake_metadata):
    md = fake_metadata(title="Song", subtitle="Sub")
    d = dict(map_fields(md, extra_fields={"title": "subtitle"}))
    assert d["subtitle"] == "Song"  # override wins over the natural subtitle row
```

- [ ] **Step 3: Run the rewritten tests to verify they FAIL**

Run: `cd contrib/picard && PYTHONPATH=. python3 -m pytest tests/test_map_fields.py -q`
Expected: FAIL — the old `map_fields` doesn't do the swaps/folds (e.g. `test_musicbrainz_id_swap`, `test_performer_role_folded_into_value` error/assert).

- [ ] **Step 4: Rewrite the mapping block in `_core.py`**

In `contrib/picard/musefs/_core.py`, replace **lines 15–105 inclusive** — from the `# Picard internal tag name -> musefs ...` comment (line 15) through the final `return pairs` of `map_fields` (line 105) — with the block below. This span contains, in order: the comment, `DIRECT_FIELDS`, `_NUMERIC_KEYS`, `MusefsError`, `_to_int`, `_values`, `_first_value`, `_MULTI_VALUE_KEYS`, `map_fields`. **Do not touch line 14 (`from ._common.sync import ArtImage`) or anything above it**, and leave `# Picard maintype → ID3 picture type ...` and all code below it unchanged. The replacement re-includes `MusefsError` verbatim and swaps `_to_int` for the float-aware `_is_zero`:

```python
# Picard internal tag name -> canonical musefs store key, for the few names whose
# on-disk form differs from Picard's variable name. Mirrors the format-agnostic
# subset of Picard's own var->tag translation (its vorbis.py ``__rtranslate``):
# the MusicBrainz recording/track id swap, the movement name/number swap, and the
# track/disc totals. Every other Picard name already equals its on-disk key and
# passes through verbatim.
RENAME = {
    "musicbrainz_recordingid": "musicbrainz_trackid",
    "musicbrainz_trackid": "musicbrainz_releasetrackid",
    "movementnumber": "movement",
    "movement": "movementname",
    "totaltracks": "tracktotal",
    "totaldiscs": "disctotal",
}

# Output keys whose value is dropped when it normalizes to zero (a 0
# number/total/compilation flag is noise, not data).
_DROP_IF_ZERO = {
    "tracknumber",
    "discnumber",
    "tracktotal",
    "disctotal",
    "compilation",
    "bpm",
}


class MusefsError(Exception):  # noqa: N818
    """A user-facing failure (binary missing, scan failed, DB absent)."""


def _is_zero(text):
    """True when ``text`` represents a numeric zero, so a 0 placeholder is
    dropped. Float-aware (matches the beets sibling's check): non-numeric and
    fractional non-zero values are not zero and pass through unharmed."""
    try:
        return float(text) == 0.0
    except ValueError:
        return False


def _output_key(name):
    """Map a Picard tag name to a ``(store_key, performer_role)`` pair.

    ``performer_role`` is ``None`` for ordinary keys; for ``performer`` /
    ``performer:<role>`` it is the role string ("" when bare) so the caller folds
    it into the value in Picard's own ``Name (Role)`` form. ``comment`` /
    ``lyrics`` (bare or with a description) collapse to their base key, dropping
    the description. Everything else is renamed via ``RENAME`` or passes through
    verbatim. Picard's ``normalize_tag`` strips trailing colons, so a key only
    carries a ``:`` when it has a non-empty description/role.
    """
    base, _, desc = name.partition(":")
    if base == "performer":
        return "performer", desc
    if base in ("comment", "lyrics"):
        return base, None
    return RENAME.get(name, name), None


def map_fields(metadata, extra_fields=None):
    """Map a Picard ``Metadata`` to a list of ``(store_key, value)`` pairs.

    Enumerates every populated tag via ``rawitems``, skipping Picard's hidden
    ``~``-prefixed internals. Each non-empty value becomes its own row (the store
    has set semantics); values sharing an output key accumulate, so multi-role
    performers and multi-description comments all survive. Keys are canonicalized
    by :func:`_output_key`; ``_DROP_IF_ZERO`` keys drop a zero value.
    ``extra_fields`` (the options-page ``picard_field -> store_key`` map) is a
    final override layer applied verbatim — no role-fold, no zero-drop.
    """
    emitted = {}  # store_key -> list[str], insertion-ordered, accumulating
    for name, values in metadata.rawitems():
        if name.startswith("~"):
            continue
        key, role = _output_key(name)
        for raw in values:
            text = str(raw).strip()
            if not text:
                continue
            if role:
                text = f"{text} ({role})"
            if key in _DROP_IF_ZERO and _is_zero(text):
                continue
            emitted.setdefault(key, []).append(text)

    if extra_fields:
        for pic_field, store_key in extra_fields.items():
            values = [t for v in metadata.getall(pic_field) if (t := str(v).strip())]
            if values:
                emitted[store_key] = values

    return [(key, value) for key, values in emitted.items() for value in values]
```

- [ ] **Step 5: Run the map_fields + contract tests to verify they PASS**

Run: `cd contrib/picard && PYTHONPATH=. python3 -m pytest tests/test_map_fields.py tests/test_contract.py -q`
Expected: PASS (all map_fields tests + the unchanged shared contract).

- [ ] **Step 6: Run the full Picard stub suite to confirm no regression**

Run: `cd contrib/picard && PYTHONPATH=. python3 -m pytest tests -q`
Expected: PASS or skips only. Real-Picard/Qt tests (e.g. `test_plugin_loads`, `test_options_page`) silently skip without an importable Picard — that's expected here (verified for real in Task 4). No failures.

- [ ] **Step 7: Commit**

```bash
cd /home/cfutro/git/musefs/.worktrees/picard-tags
git add contrib/picard/musefs/_core.py contrib/picard/tests/conftest.py contrib/picard/tests/test_map_fields.py
git commit -m "$(cat <<'EOF'
feat(picard): map Picard's full tag set, not a static whitelist

Enumerate every populated Picard tag via rawitems() instead of the fixed
~20-field DIRECT_FIELDS, emitting standard on-disk store keys: the
MusicBrainz recording/track id swap, movement swap, track/disc totals,
sort/credit fields, per-role performers (folded to "Name (Role)"), and
comment/lyrics collapse. Hidden ~-prefixed vars are skipped; all fields
multi-value-expand; numeric zero placeholders are dropped. Fixes #424.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

(The pre-commit hook runs fmt + clippy + the full workspace test suite + ruff; a Python-only change keeps the Rust suite green. Do NOT use `--no-verify`.)

---

## Task 2: beets — realign outlier keys to the on-disk standard

**Files:**
- Modify: `contrib/beets/beetsplug/_core.py` (`RENAME`, `_DROP_IF_ZERO`)
- Modify: `contrib/beets/tests/test_map_fields.py`

beets needs a venv (PEP 668; none currently exists). Note: a grep of `contrib/beets/tests/` for `comp`/`artist_sort`/`albumartist_sort` confirmed `test_map_fields.py` is the **only** test asserting these output keys — no other beets test needs updating.

- [ ] **Step 1: Ensure the beets venv exists with the local deps**

Run:
```bash
cd /home/cfutro/git/musefs/.worktrees/picard-tags/contrib/beets
[ -x .venv/bin/python ] || python3 -m venv .venv
.venv/bin/pip install -q -e ../python-musefs && .venv/bin/pip install -q -e ".[test]"
.venv/bin/python -m pytest tests/test_map_fields.py -q
```
Expected: the existing suite PASSES (baseline before changes).

- [ ] **Step 2: Update the beets tests to the realigned keys**

In `contrib/beets/tests/test_map_fields.py`:

(a) Add `albumartist_sort` to the stub `_TAG_FIELDS` tuple (so the sort test can set it). Insert it right after the existing `"artists_sort",` line:

```python
    "artist_sort",
    "artists_sort",
    "albumartist_sort",
```

(b) Replace the `test_comp_one_kept_zero_dropped` function with:

```python
def test_comp_renamed_to_compilation_and_zero_dropped():
    # beets `comp` is a 0/1 int; it maps to the on-disk `compilation` key.
    # 1 -> kept "1", 0/False -> dropped.
    assert dict(map_fields(item(comp=True)))["compilation"] == "1"
    assert dict(map_fields(item(comp=1)))["compilation"] == "1"
    assert "comp" not in dict(map_fields(item(comp=True)))
    assert "compilation" not in dict(map_fields(item(comp=False)))
    assert "compilation" not in dict(map_fields(item(comp=0)))
```

(c) Add a new sort-rename test (place it after the function above):

```python
def test_sort_fields_renamed_to_on_disk_keys():
    # artist_sort/albumartist_sort are beets' internal attribute names; the
    # on-disk standard (matching Picard) is artistsort/albumartistsort.
    d = dict(map_fields(item(artist_sort="Beatles, The", albumartist_sort="V, The")))
    assert d["artistsort"] == "Beatles, The"
    assert d["albumartistsort"] == "V, The"
    assert "artist_sort" not in d and "albumartist_sort" not in d
    # plural twin collapses to the singular attr, then renames to on-disk key
    pairs = map_fields(item(artists_sort=["A", "B"]))
    assert [v for k, v in pairs if k == "artistsort"] == ["A", "B"]
```

- [ ] **Step 3: Run the beets map_fields tests to verify the new ones FAIL**

Run: `cd contrib/beets && .venv/bin/python -m pytest tests/test_map_fields.py -q`
Expected: FAIL — `test_comp_renamed_to_compilation_and_zero_dropped` and `test_sort_fields_renamed_to_on_disk_keys` fail (current code still emits `comp`/`artist_sort`).

- [ ] **Step 4: Add the three `RENAME` entries in `beetsplug/_core.py`**

In `contrib/beets/beetsplug/_core.py`, the `RENAME` dict ends with `"mb_workid": "musicbrainz_workid",`. Add three entries before the closing `}`:

```python
    "mb_workid": "musicbrainz_workid",
    "artist_sort": "artistsort",
    "albumartist_sort": "albumartistsort",
    "comp": "compilation",
}
```

- [ ] **Step 5: Update `_DROP_IF_ZERO` in `beetsplug/_core.py`**

In the same file, change the `_DROP_IF_ZERO` set: replace the `"comp",` line with `"compilation",` (it is keyed on the output key, which is now `compilation`):

```python
    "disctotal",
    "compilation",
    "bpm",
```

- [ ] **Step 6: Run the beets map_fields tests to verify they PASS**

Run: `cd contrib/beets && .venv/bin/python -m pytest tests/test_map_fields.py -q`
Expected: PASS.

- [ ] **Step 7: Run the full beets suite to confirm no regression**

Run: `cd contrib/beets && .venv/bin/python -m pytest tests -q`
Expected: PASS (opt-in `musefs_bin`/`e2e` tiers are skipped by default). The shared contract test (`test_contract.py`) is unaffected — it covers only title/artist/album/genre/composer.

- [ ] **Step 8: Commit**

```bash
cd /home/cfutro/git/musefs/.worktrees/picard-tags
git add contrib/beets/beetsplug/_core.py contrib/beets/tests/test_map_fields.py
git commit -m "$(cat <<'EOF'
fix(beets): emit on-disk-standard store keys for sort and compilation

Map beets' internal attribute names artist_sort/albumartist_sort/comp to
the on-disk tag names artistsort/albumartistsort/compilation, so a
beets-produced musefs view matches what beets (via MediaFile) and Picard
both write to real files, and the two plugins converge on one canonical
key set. Old keys clear via the accumulating musefs_managed delete_keys
diff on the next sync. Part of #424.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Documentation

**Files:** `contrib/picard/README.md` (two edits). `contrib/beets/README.md` needs **no change** — verified: its "Field coverage" note (line 120) already says "every tag beets writes to a file … is synced … under canonical musefs keys" and it documents no outlier key spellings (`comp`/`artist_sort` appear nowhere in it).

- [ ] **Step 1: Fix the stale "Extra field map" example in the Picard README**

The plugin now syncs the full tag set automatically, so the old `comment=comment` example (comment maps by default) is misleading. In `contrib/picard/README.md`, replace these two lines (49–50):

```markdown
- **Extra field map** — optional `key=value` list mapping extra Picard tag names
  to musefs keys, e.g. `comment=comment`.
```

with:

```markdown
- **Extra field map** — optional `key=value` list mapping additional or custom
  Picard tag names to musefs store keys (applied verbatim, last-wins, on top of
  the automatic full-tag-set sync), e.g. `mymood=mood`.
```

- [ ] **Step 2: Add a "Field coverage" note to the Picard README Notes section**

In the `## Notes` section, immediately after the bullet `- **Tags are fully replaced** with Picard's view on every sync.`, insert:

```markdown
- **Field coverage:** every populated Picard tag is synced under its canonical
  musefs (on-disk) key — all MusicBrainz IDs, sort and performer/credit fields,
  movement, totals, and any custom field; multi-values expand and per-role
  performers fold to `Name (Role)`. Picard's hidden `~` internals (length,
  rating, …) are never written.
```

- [ ] **Step 3: Verify the rendered Markdown reads cleanly**

Run: `sed -n '44,72p' contrib/picard/README.md`
Expected: the updated "Extra field map" bullet and the new "Field coverage" bullet appear, correctly placed and formatted.

- [ ] **Step 4: Commit (docs-only)**

```bash
cd /home/cfutro/git/musefs/.worktrees/picard-tags
git add contrib/picard/README.md
git commit -m "$(cat <<'EOF'
docs(picard): document full tag-set mapping coverage

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

(Docs-only commits skip the cargo gate; ruff/shellcheck/yamllint legs still run.)

---

## Task 4: Final verification (real Picard + both suites)

No code changes — confirm the change works against real Picard and re-run both suites.

- [ ] **Step 1: Run the Picard suite against real Picard (the silently-skipped tests)**

Run:
```bash
cd /home/cfutro/git/musefs/.worktrees/picard-tags/contrib/picard
PYTHONPATH=".:/usr/lib/picard:/usr/lib/python3/dist-packages" /usr/bin/python3 -m pytest tests -q
```
Expected: the real-Picard tests now execute (not skip) and PASS. `pytest-qt`-dependent tests (qtbot/qapp) may still skip if pytest-qt is absent — that's acceptable. No failures. (If `test_map_fields`/`test_contract` were already green under the stub run, they stay green here.)

- [ ] **Step 2: Re-run the beets suite**

Run: `cd contrib/beets && .venv/bin/python -m pytest tests -q`
Expected: PASS.

- [ ] **Step 3: Confirm the git log**

Run: `git log --oneline -4`
Expected: the picard feat, beets fix, and docs commits on `picard-tags` atop `bfef2f7`.

---

## Self-Review notes (for the executor)

- **Spec coverage:** enumeration + `~`-skip + multi-value (Task 1 §1), `_output_key`/RENAME/colon-fold (Task 1 §2), drop-if-zero (Task 1 §3), `extra_fields` verbatim (Task 1 §4), removed symbols (Task 1 Step 4 replaces the block), beets §6 alignment (Task 2), tests both plugins (Tasks 1–2), docs (Task 3), real-Picard verification (Task 4). The migration note and convergence audit in the spec are informational (no code).
- **No `--no-verify`, no `--amend`** (project rule). Each non-docs commit runs the full workspace suite via the pre-commit hook; a Python-only change keeps Rust green.
- **Signature stability:** `map_fields(metadata, extra_fields=None)` is unchanged; `__init__.py` calls `map_fields(f.metadata, opts.fields)` positionally — still valid.
- **`normalize_tag` reality:** real Picard yields bare `comment`/`performer` (trailing colon stripped) and `comment:eng`/`performer:Piano` for real descriptions; the stub stores keys verbatim, so tests use those post-normalize forms.
