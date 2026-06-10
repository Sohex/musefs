# Beets Computed-Path Tag (`beets_path`) — Design

Date: 2026-06-08
Status: Approved (pending spec review)

## Context

musefs now supports a slash-preserving path field, `$!{field}`
(`musefs-core/src/template.rs`, merged in #170): the value of the named tag is
kept with its `/` as directory separators, so a precomputed relative path
expands into a real directory hierarchy. `ARCHITECTURE.md` documents the
intended "computed-tag" workflow — an external tool evaluates its own
(Turing-complete) path logic and writes the resulting relative path into a
custom text tag, which the user then mounts with `--template '$!{that_tag}'`.

This spec wires that workflow into the **beets** plugin. beets already owns a
rich path-templating system (the `paths:` config / `path_formats`), and exposes
it programmatically via `Item.destination()`. The plugin should compute each
track's beets library path and write it as a `beets_path` text tag, so a musefs
mount can mirror the user's beets library layout with zero extra configuration.

Picard support (`picard_path`) is intentionally deferred to a separate
follow-up spec: Picard has no "library destination" concept and would require
evaluating its file-naming script via `ScriptParser`, a materially larger and
less certain piece.

## Goal

The beets plugin writes one extra text tag per synced track, `beets_path`,
whose value is the beets-computed relative path (no leading slash, no file
extension). A user mounting with `--template '$!{beets_path}'` then sees a tree
matching their beets library layout. Zero config required; opt-out available.

## Non-goals

- A plugin-side `path_template` override. Reorganizing the view to a *different*
  layout already happens at mount time using musefs's own template engine
  (`--template '$genre/$album/...'`, with fallbacks and conditional sections).
  The only thing an override would add is an alternate **beets-function-powered**
  layout that differs from on-disk — niche, and better served by a future
  "multiple named templates" feature if demand appears.
- Picard support (separate spec).
- Any change to the shared `python-musefs` library or the musefs Rust core.

## Behavior

For each track the plugin syncs, it emits a text tag:

- **key:** `beets_path` (fixed, lowercase; this is what users type in
  `$!{beets_path}`).
- **value:** `item.destination(relative_to_libdir=True)`, decoded with
  `os.fsdecode`, with the trailing file extension removed via
  `os.path.splitext`.

Rationale for the transforms:

- `relative_to_libdir=True` yields the path fragment under the beets library
  base — a relative path, never absolute. beets has already sanitized each path
  component per the user's `replace` config.
- The extension is stripped because musefs's `render` appends `.{ext}` itself
  (from the track's scanned `format`); leaving it would double the extension
  (`…/01 Pigs.flac.flac`). `destination()` always appends exactly one extension
  at the end, so `os.path.splitext` removes precisely that. `splitext` strips
  only the suffix after the last dot in the final path component, so dotted
  directory or file names (e.g. `Vol. 2`) are unaffected, and because beets
  always appends an extension there is always exactly one to remove.
- musefs's `$!{}` path field re-sanitizes each segment (control chars → `_`,
  empty/`.`/`..` dropped) at render time, so the stored value is defense-in-depth
  safe regardless of beets' output.

## Configuration

beets `config.yaml`, under the existing `musefs:` block:

```yaml
musefs:
  db: ~/musefs.db
  write_path: yes      # default yes; set no to skip the beets_path tag entirely
```

`write_path` defaults **on**. The `beets_path` tag is inert for any mount that
does not reference `$!{beets_path}`, so writing it by default is harmless and
keeps the feature zero-config. `write_path: no` is provided for users who sync
tags but do not want the extra row.

## Integration

- **Where:** `contrib/beets/beetsplug/_core.py`, in `build_records` (the single
  function that turns beets Items into `Record`s for both trigger sites — the
  import-time listeners and the `musefs` command). It appends
  `("beets_path", value)` to the `Record.pairs` list when `write_path` is on.
- **Plumbing:** `MusefsPlugin.__init__` registers a `write_path: True` config
  default alongside the existing `db`/`fields`/`bin`/`autoscan` defaults, and
  `_sync` passes the resolved value into `build_records` (mirroring how `fields`
  is already threaded). The computed pair flows through the existing, unchanged
  `replace_tags` → `sync_files` path. The shared `python-musefs` library and the
  musefs Rust core are untouched.
- **Idempotency:** `replace_tags` already deletes a track's text tags before
  re-inserting, so re-syncing overwrites a stale `beets_path` cleanly. A path
  that later becomes uncomputable (see below) simply isn't re-written.

## Error handling

Computing the path must never abort a sync, and must never introduce a value
the store cannot hold. In any of these cases the plugin **skips only the
`beets_path` tag** for that item — all other tags still sync — and logs a
warning via the plugin's logger:

- `item.destination()` raises (e.g. a `paths:` template referencing a missing
  field).
- The path is empty after decoding and stripping (musefs treats an empty value
  as absent anyway).
- The decoded path is not UTF-8-encodable. `os.fsdecode` of a non-UTF-8 beets
  path produces lone surrogate code points, which SQLite's TEXT encoder rejects
  — writing such a value would raise mid-transaction. Decode/validate using the
  same approach the plugin already applies to the backing-path key so the two
  stay consistent; if the result still contains surrogates, skip the tag.

No aggregate counter is added: keeping the skip purely a log line avoids any
change to the shared `python-musefs` library's `SyncStats` (and to its
exact-string `summary()` output, which existing tests assert on).

## Testing

beets unit tests (`contrib/beets/tests/`), run via the venv harness
(`contrib/beets/.venv/bin/python -m pytest`):

- Extend the test `FakeItem` with a `destination(relative_to_libdir=...)` method
  returning a bytes path that includes an extension, and a way to make it raise
  (e.g. a constructor flag) for the error-path test.
- `beets_path` equals the destination with the extension stripped and no leading
  slash.
- `write_path: no` → no `beets_path` tag is written.
- `destination()` raising → `beets_path` skipped, other tags still present, and
  the sync completes (no exception propagates).
- DB-level assertion: after a sync, `SELECT value FROM tags WHERE key='beets_path'`
  returns the expected relative path for the track.

## Documentation

- `contrib/beets/README.md` (or the plugin's documented config): add `write_path`
  and a worked example — sync, then `musefs mount … --template '$!{beets_path}'`
  — cross-referencing the computed-tag workflow in `ARCHITECTURE.md`.
- Note the extension is added by musefs, so users must not append one in any
  mount template that consumes `beets_path`.
