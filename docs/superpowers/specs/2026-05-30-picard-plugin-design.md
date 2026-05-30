# musefs Picard plugin — design

**Date:** 2026-05-30
**Status:** Approved for planning
**Scope:** A MusicBrainz Picard plugin that syncs Picard's in-memory metadata
(tags + front cover) into the musefs SQLite store via a context-menu action, so a
live musefs mount re-synthesizes the view without rewriting any audio bytes. The
second integration target toward the roadmap's "beets / picard plugins" item; the
beets plugin (`contrib/beets/`) shipped first.

---

## 1. Goal & motivation

musefs presents a virtually re-tagged view of a music library backed by a SQLite
store; the roadmap explicitly targets that store as the *contract* external tools
write to. The beets plugin was the first such tool. This is the second: it lets a
Picard user drive musefs from Picard's matching/editing workflow.

The headline outcome: after matching an album in Picard, the user right-clicks →
**"Sync to musefs"** *instead of* pressing Save. Picard pushes its metadata and
front cover into the store; the backing file is never rewritten, and a live mount
shows the re-tagged view with no remount.

```
musefs mount ~/mnt --db ~/musefs.db \
    --template '$albumartist/$album/$tracknumber - $title'
# In Picard: match/edit an album as normal, then right-click → "Sync to musefs"
#   → plugin runs `musefs scan <file> --db <db>` per selected file (creates rows
#     + structural columns)
#   → writes tags + front cover into the store, keyed by realpath
#   → mount auto-refreshes (data_version poll); audio bytes untouched; Picard
#     never saves the file
```

## 2. The central constraint: no pre-save interception

Picard's plugin API (v2) exposes metadata processors (during MusicBrainz
matching), a **post**-save hook, context-menu actions, cover-art providers, and
options pages — but **no pre-save veto or redirect hook**. Picard's Save always
writes tags into the file via mutagen; there is no supported way to divert that to
the DB without monkeypatching Picard internals (fragile, breaks across versions).

Beets is not different in kind here: `beet musefs` is a *command the user runs
instead of writing files*, and its `after_write` hook fires only *after* beets
already wrote. The Picard analog of that command is a **context-menu action**.

Therefore v1 ships an **action-only** model: the user invokes "Sync to musefs"
instead of Save, and the file is never rewritten. This preserves musefs's
no-rewrite invariant. A post-save hook (sync after a normal Save) is an explicit
non-goal for v1 (§10) — it cannot preserve the invariant on its own.

## 3. Division of labour (the contract)

Identical to the beets plugin's division (see `docs/DB_CONTRACT.md`):

| Owner | Columns / tables | How |
|---|---|---|
| **`musefs scan`** | track existence; `audio_offset`, `audio_length`, `backing_size`, `backing_mtime`, `format`; seeds `tags`/`track_art` from embedded metadata | probes each backing file |
| **this plugin** | `tags`; `track_art` (when Picard has art) | writes Picard's view onto rows scan created |

**The plugin never writes structural `tracks` columns and never creates rows
directly.** It creates rows only by delegating to the `musefs` binary
(autoscan, §6). A path with no matching row after autoscan is skipped. The
plugin's writes are exactly the "external tag edits" that `scan --revalidate`
preserves.

The plugin depends only on the SQLite *contract* (schema + triggers) for its
writes, and on the `musefs` binary for row creation. No musefs process needs to be
running for a sync to land; `mount` is a separate step the user runs.

### 3.1 Track identity — `backing_path`

`scan` stores `backing_path = std::fs::canonicalize(path)` (absolute, symlinks
resolved). The plugin keys on `os.path.realpath(file.filename)` (Picard's
`File.filename` is the absolute path). `realpath` mirrors `canonicalize` on Linux
for existing files. No matching row → skip the item. This agreement is the single
silent-failure point and is retired by the path-matching gate (§10.1).

### 3.2 Tag keys — Vorbis-style lowercase

DB tag keys drive both path-template fields and format synthesis; the format layer
expects canonical lowercase keys. Picard's *internal* tag names are already those
lowercase Vorbis-style names, so the mapping is mostly identity (§5).

### 3.3 Schema version guard

`schema.rs::migrate` sets `PRAGMA user_version` to the number of applied
migrations; the current schema is `user_version = 1`. The plugin reads
`PRAGMA user_version` and **refuses to run** unless it equals the version it was
written against (1), with a message telling the user their musefs/plugin versions
have diverged.

### 3.4 Art — content-addressed, deduplicated

`art` rows are content-addressed by `sha256` (UNIQUE) with
`INSERT … ON CONFLICT(sha256) DO NOTHING` then `SELECT id`. The plugin mirrors
this with lowercase `hashlib.sha256(data).hexdigest()`. `track_art` links a track
to an art row with `picture_type` (3 = front cover), `description`, `ordinal` (PK
is `(track_id, ordinal)`). The size cap is `MAX_ART_BYTES = 16 MiB − 64 KiB`,
matching scan.

### 3.5 Triggers do the rest

Any insert/update/delete on `tags` or `track_art` fires triggers that bump the
track's `content_version` and `updated_at`. Committing changes the whole-DB
`PRAGMA data_version`. musefs's `HeaderCache` rebuilds the affected layout on a
`content_version` mismatch, and `Musefs::poll_refresh` rebuilds the tree on a
`data_version` change. A committed sync surfaces at the mount with no remount.

## 4. Components & code layout

A **self-contained folder plugin** under **`contrib/picard/`**, with no shared
runtime dependency on the beets plugin (Picard plugins are meant to be drop-in):

- **`contrib/picard/musefs/__init__.py`** — plugin entry: the Picard `PLUGIN_*`
  metadata constants (API v2), a `BaseAction` subclass registered on files,
  clusters, albums, and tracks (resolving each selection down to its underlying
  `File` objects), and the Qt options page.
- **`contrib/picard/musefs/_core.py`** — a copy of the beets plugin's DB-contract
  logic: `check_schema_version`, content-addressed art (sha256, dedup, cap), mime
  sniff, the SQL helpers (`track_id_for_path`, `replace_tags`, `upsert_art`,
  `replace_track_art`), and the realpath path key. Kept in sync with beets via the
  shared `schema_v1.sql` fixture, the `user_version` guard, and tests. Duplication
  is the deliberate cost of keeping each plugin independently installable.
- **`contrib/picard/README.md`**, **`pyproject.toml`** / **`requirements.txt`**,
  and **`tests/`**.

**Testability seam.** The sync core operates on plain primitives — a
`list[(key, value)]` of tags plus an optional `(art_bytes, mime)` — so unit and
integration tests need no running Picard. A thin `map_fields(metadata)` adapter
accepts any dict-like object (including Picard's `Metadata`) and produces those
primitives. `sync_one(tags, art, conn, opts)` then performs the DB writes.

## 5. Field mapping (tags)

Picard's internal tag names are already the lowercase Vorbis-style keys, so the
default map is mostly identity:

| Picard field | musefs key | note |
|---|---|---|
| `title`, `artist`, `albumartist`, `album`, `genre`, `composer` | same | direct copy |
| `tracknumber` | `tracknumber` | string; omit if empty/0 |
| `discnumber` | `discnumber` | string; omit if empty/0 |
| `date` | `date` | Picard already stores the full date string |

Rules:
- Empty strings and zero numerics are **omitted** — no empty tags written.
- v1 writes **one value per key** at `ordinal 0`. (Picard metadata can be
  multi-valued; the first value is taken. Multi-value is a noted future extension,
  mirroring beets v1.)
- Writing replaces the track's whole tag set: `DELETE FROM tags WHERE track_id=?`
  then batched `INSERT`. Picard is authoritative for tags, overwriting scan's
  seed.
- A `fields` config entry maps an extra Picard field to a musefs key.

## 6. Row creation (autoscan)

The plugin cannot compute structural columns, so it delegates row creation to the
`musefs` binary, mirroring beets:

1. A `bin` setting points at the `musefs` executable (overridable by `MUSEFS_BIN`).
2. For each selected file, the action runs `musefs scan <file> --db <db>`
   (single-file scan is supported) before writing tags/art, creating the row +
   structural columns on demand. `musefs scan` creates the DB if absent.
3. After scanning, the plugin looks up `track_id` by realpath. A path with no row
   (e.g. an unsupported format scan skipped) is counted `skipped`.

## 7. Art sync

Source is Picard's in-memory **`metadata.images`** (Picard already holds the image
bytes — no external file read, unlike beets' `artpath`). Per sync:

1. Take the first **front-cover** image (type 3) from `metadata.images`; read its
   `.data` and `.mimetype`.
2. Skip images larger than `MAX_ART_BYTES`; count as `skipped_art`.
3. Hash with `sha256`; `INSERT INTO art (…) ON CONFLICT(sha256) DO NOTHING` then
   `SELECT id`. `width`/`height` stored `NULL` (no image-decode dependency).
4. Replace the track's art: `DELETE FROM track_art WHERE track_id=?` then one
   `INSERT` with `picture_type=3`, `description=''`, `ordinal=0`.

**Conditional replacement (important):** `track_art` is replaced **only when
Picard has a front cover for the item**. If Picard has no art, the plugin leaves
`track_art` untouched, preserving any embedded art `scan` ingested — Picard art
wins when present, embedded art survives otherwise. (Tags, by contrast, are always
fully replaced.)

**Orphaned art:** replacing `track_art` can orphan old `art` rows. The plugin does
**not** GC; `musefs scan --revalidate` handles that. Documented, not handled here.

## 8. Configuration

Two layers; env vars take precedence over the options page:

- **Qt options page** (primary, discoverable): registered in Picard's Options
  dialog with fields for **DB path**, **musefs binary path**, an **autoscan
  toggle**, and the optional **field map**. Persisted via Picard's config.
- **Env overrides** (headless / test): `MUSEFS_DB`, `MUSEFS_BIN` override the
  corresponding settings when set.

## 9. Threading & error handling

- The action runs the scan subprocess + DB writes on a **background thread**
  (Picard's task runner) so the Qt UI never freezes. Results report via a
  status-bar message and the Picard log.
- **`busy_timeout`** (~5 s) for the concurrent mount poll; transactions kept short
  so the mount's `data_version` poll sees updates promptly.
- **Binary missing / not executable** → clear error message.
- **`user_version` mismatch** → refuse with a version-divergence message (§3.3).
- **Item path not a track row** (after autoscan) → counted `skipped`, sync
  continues.
- **`musefs scan` subprocess failure** → reported with context; that file skipped.
- **Summary** always reports: `synced`, `skipped` (no row), `art_linked`,
  `skipped_art` (oversized / no art).

## 10. Testing

- **Unit (`map_fields`)** — identity fields; `tracknumber`/`discnumber`/`date`
  handling; empty/zero omission; `fields` override applied. Accepts a plain dict;
  no Picard import.
- **Unit (mime sniff, schema guard)** — JPEG/PNG magic-byte detection + extension
  fallback; `user_version` refusal. Shared logic with the beets `_core`.
- **Integration (pytest)** — create a temp DB from the `schema_v1.sql` fixture
  (`PRAGMA user_version = 1` + V1 DDL/triggers), insert a fake track row, run the
  sync core over constructed primitives, then assert: `tags` rows match the
  mapping; `content_version` bumped; art deduped (same bytes on two tracks → one
  `art` row, two `track_art` rows); conditional art replacement (no art →
  pre-existing `track_art` preserved).

### 10.1 Path-matching robustness (gate)

The `realpath`/`canonicalize` agreement (§3.1) is the single silent-failure point,
so it gets its own test tier that runs against the **real `musefs scan` binary** —
never a hand-built DB — proving the plugin's key is byte-identical to what scan
stored. Opt-in (like the beets gate and the FUSE e2e) since it shells out to a
built `musefs` binary; CI builds it first.

For each case the harness: materialises a fixture file under a temp tree, runs
`musefs scan <tree> --db <tmp.db>`, reads back the stored `backing_path`, computes
the plugin's key from the file path, and asserts the two strings are **exactly
equal** — and, end to end, that `track_id_for_path(plugin_key)` returns the row
scan created.

Cases that must pass: plain nested file; a symlinked directory component; a
symlink to the file; a relative path and one containing `.`/`..`; a trailing slash
and other non-normalised forms; non-ASCII / Unicode filenames and ones with spaces
and `%`; (documented, not asserted to match) a path under a different mount/bind,
asserted to be reported as `skipped (no row)`, never a silent wrong-row hit.

A mismatch is a hard stop, not a warning: the keying strategy must be fixed before
the plugin is usable. This gate is shared in intent with the beets plugin's §9.1.

### 10.2 Full Picard-GUI e2e — out of scope

Automating Picard's GUI is out of scope for v1; the sync core is covered by the
integration tier and the path gate. The README documents a manual smoke test
(match an album → "Sync to musefs" → mount → verify tags/art).

## 11. Explicit non-goals (v1)

- **Pre-save interception / redirect** — not possible in Picard's API (§2).
- **Post-save hook** — action-only in v1; a post-save sync cannot preserve the
  no-rewrite invariant and is deferred.
- **Multi-valued tags** — one value per key for now.
- **All-image sync** — front cover (type 3) only; back/booklet/etc. deferred.
- **Picard 3.x manifest-format plugin** — target the Picard 2.x plugin API
  (`PLUGIN_*` constants, `register_*` hooks); 3.x packaging is a follow-up (§11).
- **Creating track rows / writing structural columns directly** — owned by
  `musefs scan`; the plugin only delegates to the binary.
- **Orphaned-art GC** — owned by `musefs scan --revalidate`.

## 12. Open risks

- **Picard 2.x vs 3.x plugin API.** v1 targets the 2.x API (PLUGIN_* metadata
  constants + `register_*` hooks). Picard 3.x introduced a manifest-based plugin
  system; supporting it is a documented follow-up, not v1 scope.
- **`realpath`/`canonicalize` agreement.** Retired by the §10.1 path-matching gate
  against the real `musefs scan` binary, exactly as for the beets plugin.
